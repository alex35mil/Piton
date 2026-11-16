//! `rpc` connections (`piton attach`): raw pi RPC relayed to one pi, with the few commands piton answers itself
//! intercepted, and the connection moved to another pi on `new_session` or `switch_session`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use super::Daemon;
use super::protocol::{AttachControl, FRAME_CONTROL, FRAME_RECORD};
use crate::launch::SessionArg;
use crate::session::{Frame, Outstanding, SessionCmd, SessionHandle};

impl Daemon {
    pub(super) async fn serve_rpc(
        self: Arc<Self>,
        workspace: PathBuf,
        mut lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
        write: OwnedWriteHalf,
    ) {
        let client = self.next_client.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        let writer = tokio::spawn(write_frames(write, rx));

        let mode = self.config().default_attach_mode;
        let mut current = match self.launch_in(&workspace, mode, SessionArg::New).await {
            Ok(handle) => handle,
            Err(e) => {
                let _ = tx.send(Frame::Control(Arc::new(AttachControl::Error {
                    message: e.to_string(),
                })));
                drop(tx);
                let _ = writer.await;
                return;
            }
        };
        let _ = tx.send(Frame::Control(Arc::new(AttachControl::Welcome {
            session: current.key,
        })));
        crate::log!(
            "client {client}: attached to session {} ({})",
            current.key,
            workspace.display()
        );
        current.attach(client, tx.clone()).await;

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(Value::Object(command)) = serde_json::from_str::<Value>(&line) else {
                let _ = tx.send(Frame::Record(
                    json!({"type":"response","command":"parse","success":false,"error":"Failed to parse command"})
                        .to_string()
                        .into(),
                ));
                continue;
            };
            match self.intercept(&current, &command).await {
                Intercept::Forward => {
                    let _ = current.tx.send(SessionCmd::FromClient { client, command });
                }
                Intercept::Reject(error) => {
                    crate::log!(
                        "client {client}: rejected {} on session {}: {error}",
                        command_type(&command),
                        current.key
                    );
                    let _ = tx.send(Frame::Record(response(&command, Err(error)).to_string().into()));
                }
                Intercept::MoveTo(target) => {
                    let old = current;
                    crate::log!(
                        "client {client}: {} moves session {} -> {}",
                        command_type(&command),
                        old.key,
                        target.key
                    );
                    let outstanding = old.detach(client).await;
                    for o in outstanding {
                        let _ = tx.send(Frame::Record(moved_response(&o).to_string().into()));
                    }
                    let _ = tx.send(Frame::Record(
                        response(&command, Ok(success_data(&command))).to_string().into(),
                    ));
                    target.attach(client, tx.clone()).await;
                    current = target;
                }
            }
        }
        crate::log!("client {client}: detached from session {}", current.key);
        current.detach(client).await;
        drop(tx);
        let _ = writer.await;
    }

    async fn intercept(self: &Arc<Self>, current: &SessionHandle, command: &Map<String, Value>) -> Intercept {
        let kind = command.get("type").and_then(Value::as_str).unwrap_or_default();
        let shared = current.snapshot();
        let is_shared = shared.clients > 1;
        match kind {
            "switch_session" => {
                let Some(path) = command.get("sessionPath").and_then(Value::as_str) else {
                    return Intercept::Forward;
                };
                if let Some(target) = self.live_with_file(path, current.key) {
                    return Intercept::MoveTo(target);
                }
                if !is_shared {
                    return Intercept::Forward;
                }
                match self
                    .launch_in(&shared.workspace, shared.mode, SessionArg::Resume(path.into()))
                    .await
                {
                    Ok(target) => Intercept::MoveTo(target),
                    Err(e) => Intercept::Reject(e.to_string()),
                }
            }
            "new_session" if is_shared => match self.launch_in(&shared.workspace, shared.mode, SessionArg::New).await {
                Ok(target) => Intercept::MoveTo(target),
                Err(e) => Intercept::Reject(e.to_string()),
            },
            "fork" | "clone" if is_shared => {
                Intercept::Reject(format!("{kind} isn't supported while other clients share this session"))
            }
            _ => Intercept::Forward,
        }
    }
}

fn command_type(command: &Map<String, Value>) -> &str {
    command.get("type").and_then(Value::as_str).unwrap_or("?")
}

enum Intercept {
    Forward,
    Reject(String),
    MoveTo(SessionHandle),
}

fn response(command: &Map<String, Value>, result: Result<Value, String>) -> Value {
    let mut out = Map::new();
    if let Some(id) = command.get("id") {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), json!("response"));
    out.insert("command".into(), command.get("type").cloned().unwrap_or(Value::Null));
    match result {
        Ok(data) => {
            out.insert("success".into(), json!(true));
            out.insert("data".into(), data);
        }
        Err(error) => {
            out.insert("success".into(), json!(false));
            out.insert("error".into(), json!(error));
        }
    }
    Value::Object(out)
}

fn success_data(command: &Map<String, Value>) -> Value {
    match command.get("type").and_then(Value::as_str) {
        Some("switch_session" | "new_session") => json!({"cancelled": false}),
        Some(_) | None => json!({}),
    }
}

fn moved_response(outstanding: &Outstanding) -> Value {
    let mut out = Map::new();
    if let Some(id) = &outstanding.original_id {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), json!("response"));
    out.insert("command".into(), json!(outstanding.command));
    out.insert("success".into(), json!(false));
    out.insert("error".into(), json!("connection moved to another session"));
    Value::Object(out)
}

async fn write_frames(mut write: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Frame>) {
    let mut buf = Vec::new();
    while let Some(frame) = rx.recv().await {
        buf.clear();
        let end = match &frame {
            Frame::Record(line) => {
                buf.push(FRAME_RECORD);
                buf.extend_from_slice(line.as_bytes());
                false
            }
            Frame::Control(control) => {
                buf.push(FRAME_CONTROL);
                buf.extend_from_slice(serde_json::to_string(control.as_ref()).unwrap_or_default().as_bytes());
                matches!(
                    control.as_ref(),
                    AttachControl::End { .. } | AttachControl::Error { .. }
                )
            }
        };
        buf.push(b'\n');
        if write.write_all(&buf).await.is_err() || end {
            break;
        }
    }
    let _ = write.shutdown().await;
}
