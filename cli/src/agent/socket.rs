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
    let pid = peer.as_ref().ok().map(|p| p.pid);
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let authorized = match identify(&daemon, peer, &line).await {
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
            let response = launched(&daemon, session, pid, upstream);
            send(&mut write, &response).await;
        }
        Authorized::Worker { session, request } => {
            // Closing the connection before the user's answer withdraws the question; once approved, the operation runs
            // as its own task to the end (`workers.rs`), and its answer is sent while the connection is open.
            tokio::select! {
                response = super::workers::serve(daemon.clone(), session, request) => {
                    send(&mut write, &response).await;
                }
                _ = closed(&mut lines) => {}
            }
        }
    }
}

/// The authorized request `line` carries, from the connecting process `peer`.
async fn identify(daemon: &Daemon, peer: Result<Peer, channel::PeerError>, line: &str) -> Result<Authorized, Refused> {
    let envelope: Envelope = serde_json::from_str(line).map_err(|e| Refused::Invalid { message: e.to_string() })?;
    crate::debug!(
        "agent.sock: {} ({})",
        envelope.request.kind(),
        asking(envelope.request.asks())
    );
    let peer = peer.map_err(|e| {
        crate::log!("agent.sock: {e}");
        Refused::Unidentified
    })?;
    let started = tokio::time::Instant::now();
    let caller = loop {
        match daemon.agent_tokens().accept(&envelope.token, &peer) {
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
