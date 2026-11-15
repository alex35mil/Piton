//! The daemon's Unix socket: one hello line picks the connection's protocol (`rpc`, `control`, or the apps' `view`),
//! and the line writers they share.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;

use super::Daemon;
use super::protocol::{AttachControl, ControlRequest, ControlResponse, FRAME_CONTROL, Hello, PROTOCOL_VERSION};

impl Daemon {
    pub(super) async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let hello = match lines.next_line().await {
            Ok(Some(line)) => serde_json::from_str::<Hello>(&line),
            Ok(None) | Err(_) => return,
        };
        match hello {
            Ok(Hello::Rpc {
                version,
                workspace,
                client: _,
            }) if version == PROTOCOL_VERSION => {
                self.serve_rpc(workspace, lines, write).await;
            }
            Ok(Hello::Control { version }) if version == PROTOCOL_VERSION => {
                let response = match lines.next_line().await {
                    Ok(Some(line)) => match serde_json::from_str::<ControlRequest>(&line) {
                        Ok(request) => self.control(request).await,
                        Err(e) => ControlResponse::Error {
                            message: format!("invalid request: {e}"),
                        },
                    },
                    Ok(None) | Err(_) => return,
                };
                let _ = write_json_line(&mut write, &response).await;
            }
            Ok(Hello::View { version, client }) if version == PROTOCOL_VERSION => {
                crate::log!("view client connected ({})", client.as_deref().unwrap_or("unnamed"));
                let (in_tx, in_rx) = mpsc::unbounded_channel::<String>();
                let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Arc<str>>();
                let writer = tokio::spawn(async move {
                    while let Some(line) = out_rx.recv().await {
                        if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                            break;
                        }
                    }
                });
                let reader = tokio::spawn(async move {
                    while let Ok(Some(line)) = lines.next_line().await {
                        if in_tx.send(line).is_err() {
                            break;
                        }
                    }
                });
                crate::apps::serve(self.clone(), in_rx, out_tx).await;
                reader.abort();
                let _ = writer.await;
                crate::log!("view client disconnected");
            }
            Ok(Hello::Rpc { .. } | Hello::Control { .. } | Hello::View { .. }) => {
                let message = format!("unsupported protocol version (daemon speaks {PROTOCOL_VERSION})");
                let _ = write_control(&mut write, &AttachControl::Error { message }).await;
            }
            Err(e) => {
                let _ = write_control(
                    &mut write,
                    &AttachControl::Error {
                        message: format!("invalid hello: {e}"),
                    },
                )
                .await;
            }
        }
    }
}

pub(super) async fn write_control(write: &mut OwnedWriteHalf, control: &AttachControl) -> std::io::Result<()> {
    let mut line = vec![FRAME_CONTROL];
    line.extend_from_slice(serde_json::to_string(control).unwrap_or_default().as_bytes());
    line.push(b'\n');
    write.write_all(&line).await
}

pub(super) async fn write_json_line<T: serde::Serialize>(write: &mut OwnedWriteHalf, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).unwrap_or_default();
    line.push(b'\n');
    write.write_all(&line).await
}
