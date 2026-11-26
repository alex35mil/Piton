//! `agent.sock`: the socket sandboxed processes reach the daemon on, next to the control socket but separate from it. A
//! sandbox may connect to this one socket (a built-in allow in every launch's policy) and never to the control socket.
//!
//! One request per connection, one JSON line each way, except `run_command`, which streams the command's output as
//! lines before its last one. The server is only a transport: it reads the connecting process ([`Peer`]), parses the
//! [`Envelope`], accepts its token for that process, authorizes the request for its caller, and dispatches to the
//! request's own handler (`write.rs`, `run.rs`, the daemon's launch). The contract is `channel.rs`.

use std::{path::Path, sync::Arc};

use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use super::{
    answer::Asks,
    channel::{self, Authorized, Envelope, Peer, Refused, Unaccepted},
};

/// How a request's declaration reads in the daemon log.
fn asking(asks: Asks) -> &'static str {
    match asks {
        Asks::Always(_) => "asks the user",
        Asks::When(_) => "may ask the user",
        Asks::Never => "asks no one",
    }
}
use crate::{daemon::Daemon, session::SessionCmd};

/// The socket file, in the daemon's state folder.
pub const FILE: &str = "agent.sock";

/// How long a token whose process isn't known yet (pi before the daemon recorded its start, a call before it started)
/// is asked about again before it's refused.
const NOT_YET_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const NOT_YET_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// `n` random bytes as hex; the clock's nanoseconds if the system's randomness can't be read (a tag, not a secret).
pub fn random_hex(n: usize) -> String {
    channel::random_bytes_hex(n).unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("{nanos:x}")
    })
}

/// Binds the socket, owner-only and replacing a stale one.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                tokio::spawn(async move { handle(daemon, stream).await });
            }
            Err(e) => crate::log!("agent.sock: accept failed: {e}"),
        }
    }
}

pub type Writer = tokio::net::unix::OwnedWriteHalf;
type Lines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;

async fn handle(daemon: Arc<Daemon>, stream: UnixStream) {
    let peer = Peer::of(&stream);
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let peer = match peer {
        Ok(peer) => peer,
        Err(e) => {
            crate::log!("agent.sock: {e}");
            send(&mut write, &channel::refused_line(&Refused::Unidentified)).await;
            return;
        }
    };
    let authorized = match identify(&daemon, &peer, &line).await {
        Ok(authorized) => authorized,
        Err(refused) => {
            crate::log!("agent.sock: refused: {refused}");
            send(&mut write, &channel::refused_line(&refused)).await;
            return;
        }
    };
    match authorized {
        Authorized::ProtectedWrite { session, request } => {
            // The agent's tool call holds the connection while the user decides; when it's aborted, the connection
            // closes and the wait ends, which the session sees (its reply channel closes) and settles.
            tokio::select! {
                response = super::write::protected_write(&daemon, session, request) => {
                    send(&mut write, &response).await;
                }
                _ = closed(&mut lines) => {}
            }
        }
        Authorized::RunCommand { session, request } => {
            // Streams: the output lines, then the end. Closing the connection (the agent's stop or timeout) drops the
            // run, which kills the command's process group.
            tokio::select! {
                () = super::run::serve(&daemon, session, request, &mut write) => {}
                _ = closed(&mut lines) => {}
            }
        }
        Authorized::Launched { session, upstream } => {
            let response = launched(&daemon, session, Some(peer.pid), upstream);
            send(&mut write, &response).await;
        }
        Authorized::Lift { session, request } => {
            // The tokens are bound to this connection's process: pi's, which the session token was accepted from.
            tokio::select! {
                response = super::lift::request(&daemon, session, &peer, request) => {
                    send(&mut write, &response).await;
                }
                _ = closed(&mut lines) => {}
            }
        }
        Authorized::Lifts { session } => lifts(&daemon, session, &peer, &mut write, &mut lines).await,
    }
}

/// One line of the `lifts` stream, daemon to pi.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LiftsLine {
    /// Consume this token now; `lift` is what it opens. `for_child`: a process pi started (an MCP server) was blocked,
    /// so pi's MCP servers restart to inherit the grant.
    Grant {
        id: u64,
        token: String,
        lift: crate::launch::sandbox::grant::LiveLift,
        for_child: bool,
    },
    /// Release grant `id`.
    Revoke { id: u64 },
    /// The session ended.
    Ended,
}

/// One line of the `lifts` stream, pi to daemon.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum LiftsReply {
    Consumed { id: u64 },
    Failed { id: u64, message: String },
}

/// pi's `lifts` stream: the session's grants and revocations go out as tokens bound to `peer` (pi's process); pi's
/// replies come back. Held while both sides are open; when it closes, the session ends every grant.
async fn lifts(
    daemon: &Daemon,
    session: crate::session::SessionKey,
    peer: &Peer,
    write: &mut Writer,
    lines: &mut Lines,
) {
    use crate::session::{LiftsEvent, LiftsOut};
    let Some(handle) = daemon.session(session) else {
        send(write, &LiftsLine::Ended).await;
        return;
    };
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stream_id_tx, stream_id) = tokio::sync::oneshot::channel();
    if handle
        .tx
        .send(SessionCmd::Lifts(LiftsEvent::Opened {
            out: out_tx,
            id: stream_id_tx,
            pi: peer.pid,
        }))
        .is_err()
    {
        send(write, &LiftsLine::Ended).await;
        return;
    }
    let Ok(stream) = stream_id.await else {
        send(write, &LiftsLine::Ended).await;
        return;
    };
    let tx = handle.tx.clone();
    let _closed = WakeOnDropWith(tx.clone(), SessionCmd::Lifts(LiftsEvent::Closed { stream }));
    loop {
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(LiftsOut::Grant { id, lift, for_child }) => {
                    let line = match super::lift::issue_for(&lift, peer) {
                        Ok(token) => LiftsLine::Grant { id, token, lift, for_child },
                        Err(e) => {
                            let _ = tx.send(SessionCmd::Lifts(LiftsEvent::Failed { id, message: e.to_string() }));
                            continue;
                        }
                    };
                    if !send(write, &line).await {
                        return;
                    }
                }
                Some(LiftsOut::Revoke { id }) => {
                    if !send(write, &LiftsLine::Revoke { id }).await {
                        return;
                    }
                }
                None => {
                    send(write, &LiftsLine::Ended).await;
                    return;
                }
            },
            line = lines.next_line() => match line {
                Ok(Some(text)) => match serde_json::from_str::<LiftsReply>(&text) {
                    Ok(LiftsReply::Consumed { id }) => {
                        let _ = tx.send(SessionCmd::Lifts(LiftsEvent::Consumed { id }));
                    }
                    Ok(LiftsReply::Failed { id, message }) => {
                        let _ = tx.send(SessionCmd::Lifts(LiftsEvent::Failed { id, message }));
                    }
                    Err(e) => crate::log!("agent.sock: lifts: unreadable reply: {e}"),
                },
                Ok(None) | Err(_) => return,
            },
        }
    }
}

/// Sends a command to the session when dropped.
struct WakeOnDropWith(tokio::sync::mpsc::UnboundedSender<SessionCmd>, SessionCmd);

impl Drop for WakeOnDropWith {
    fn drop(&mut self) {
        let command = std::mem::replace(&mut self.1, SessionCmd::SettleLift);
        let _ = self.0.send(command);
    }
}

/// The authorized request `line` carries, from the connecting process `peer`.
async fn identify(daemon: &Daemon, peer: &Peer, line: &str) -> Result<Authorized, Refused> {
    let envelope: Envelope = serde_json::from_str(line).map_err(|e| Refused::Invalid { message: e.to_string() })?;
    crate::debug!(
        "agent.sock: {} ({})",
        envelope.request.kind(),
        asking(envelope.request.asks())
    );
    let started = tokio::time::Instant::now();
    let caller = loop {
        match daemon.agent_tokens().accept(&envelope.token, peer) {
            Ok(caller) => break caller,
            Err(Unaccepted::NotYet) if started.elapsed() < NOT_YET_WAIT => tokio::time::sleep(NOT_YET_POLL).await,
            Err(unaccepted @ (Unaccepted::NotYet | Unaccepted::UnknownToken | Unaccepted::WrongProcess)) => {
                return Err(Refused::Token(unaccepted));
            }
        }
    };
    // A session's tokens exist from just before it starts; it's registered once it started (the launch helper can ask
    // in between).
    while daemon.session(caller.session()).is_none() {
        if started.elapsed() >= NOT_YET_WAIT || !daemon.agent_tokens().live(caller.session()) {
            return Err(Refused::Ended);
        }
        tokio::time::sleep(NOT_YET_POLL).await;
    }
    channel::authorize(envelope.request, caller)
}

/// What `launched` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LaunchedResponse {
    /// pi's process is recorded, and the gate's route when one was given.
    Recorded,
    /// The session ended.
    Ended,
}

/// The helper is about to become pi: its process is pi's from now on. Behind fnox with a filtered network, the gate
/// forwards through fnox's proxy.
fn launched(
    daemon: &Daemon,
    session: crate::session::SessionKey,
    pid: Option<i32>,
    upstream: Option<String>,
) -> LaunchedResponse {
    let Some(handle) = daemon.session(session) else {
        return LaunchedResponse::Ended;
    };
    if let Some(pid) = pid {
        daemon.agent_tokens().pi_started(session, pid);
    }
    if let Some(url) = upstream
        && handle.tx.send(SessionCmd::GateUpstream { url }).is_err()
    {
        return LaunchedResponse::Ended;
    }
    LaunchedResponse::Recorded
}

/// Sends one line; false when the client stopped listening.
pub async fn send(write: &mut Writer, response: &impl Serialize) -> bool {
    let mut out = serde_json::to_string(response).unwrap_or_default();
    out.push('\n');
    write.write_all(out.as_bytes()).await.is_ok()
}

/// Resolves when the client closes its side (or sends more, which this protocol doesn't allow).
async fn closed(lines: &mut Lines) {
    let _ = lines.next_line().await;
}

/// Asks the session to look at its pending question when dropped: the agent stopped waiting.
pub struct WakeOnDrop(
    pub tokio::sync::mpsc::UnboundedSender<SessionCmd>,
    pub fn() -> SessionCmd,
);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send((self.1)());
    }
}
