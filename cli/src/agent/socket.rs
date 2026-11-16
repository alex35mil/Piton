//! `agent.sock`: the socket sandboxed pis reach the daemon on, next to the control socket but separate from it. A
//! sandbox may connect to this one socket (a `unix_socket` grant in its profile) and never to the control socket.
//!
//! One request per connection, one JSON line each way. A request names its session by the token the daemon gave that
//! pi (`PITON_SESSION_TOKEN`): a request can act only for the session it came from, and only in the ways listed here.
//! Today that's one: a protected write (see `write.rs`), answered only by the user through a client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::Daemon;
use crate::session::{SessionCmd, SessionKey, lock};

/// The socket file, in the daemon's state folder.
pub const FILE: &str = "agent.sock";
/// The variable a pi reads its token from.
pub const TOKEN_ENV: &str = "PITON_SESSION_TOKEN";
/// The variable a pi reads the socket path from.
pub const SOCKET_ENV: &str = "PITON_AGENT_SOCKET";

/// Session tokens: random, one per launched pi, forgotten when the session ends.
#[derive(Debug, Default)]
pub struct Tokens {
    by_token: Mutex<HashMap<String, SessionKey>>,
}

impl Tokens {
    /// A new token for `key`.
    pub fn issue(&self, key: SessionKey) -> std::io::Result<String> {
        use std::io::Read;
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        lock(&self.by_token).insert(token.clone(), key);
        Ok(token)
    }

    pub fn session(&self, token: &str) -> Option<SessionKey> {
        lock(&self.by_token).get(token).copied()
    }

    pub fn forget(&self, key: SessionKey) {
        lock(&self.by_token).retain(|_, k| *k != key);
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    ProtectedWrite {
        token: String,
        path: PathBuf,
        content: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    ProtectedWrite(super::write::WriteOutcome),
    Error { message: String },
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

async fn handle(daemon: Arc<Daemon>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return;
    };
    let response = match serde_json::from_str::<Request>(&line) {
        // The agent's tool call holds the connection while the user decides; when it's aborted, the connection closes
        // and the wait ends, which the session sees (its reply channel closes) and settles.
        Ok(request) => tokio::select! {
            response = respond(&daemon, request) => response,
            _ = closed(&mut lines) => return,
        },
        Err(e) => Response::Error {
            message: format!("invalid request: {e}"),
        },
    };
    let mut out = serde_json::to_string(&response).unwrap_or_default();
    out.push('\n');
    let _ = write.write_all(out.as_bytes()).await;
}

/// Resolves when the client closes its side (or sends more, which this protocol doesn't allow).
async fn closed(lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>) {
    let _ = lines.next_line().await;
}

async fn respond(daemon: &Daemon, request: Request) -> Response {
    let unknown = || Response::Error {
        message: "unknown or ended session".into(),
    };
    match request {
        Request::ProtectedWrite { token, path, content } => {
            let Some(handle) = daemon
                .agent_tokens()
                .session(&token)
                .and_then(|key| daemon.session(key))
            else {
                return unknown();
            };
            let (reply, outcome) = tokio::sync::oneshot::channel();
            let request = super::write::WriteRequest { path, content };
            if handle.tx.send(SessionCmd::ProtectedWrite { request, reply }).is_err() {
                return unknown();
            }
            // Wakes the session when the agent stops waiting, so it closes the dialog at once.
            let session = handle.tx.clone();
            let _wake = WakeOnDrop(session);
            // Waits for the user, as long as it takes; the agent's tool call waits with it.
            match outcome.await {
                Ok(outcome) => Response::ProtectedWrite(outcome),
                Err(_) => Response::Error {
                    message: "the session ended before the user answered".into(),
                },
            }
        }
    }
}

/// Asks the session to look at its pending protected write when dropped: the agent stopped waiting.
struct WakeOnDrop(tokio::sync::mpsc::UnboundedSender<SessionCmd>);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(SessionCmd::SettleProtectedWrite);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_name_one_session_until_forgotten() {
        let tokens = Tokens::default();
        let a = tokens.issue(1).unwrap();
        let b = tokens.issue(2).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert_eq!(tokens.session(&a), Some(1));
        tokens.forget(1);
        assert_eq!(tokens.session(&a), None);
        assert_eq!(tokens.session(&b), Some(2));
        assert_eq!(tokens.session("guess"), None);
    }

    #[test]
    fn requests_are_strict() {
        assert!(
            serde_json::from_str::<Request>(r#"{"type":"protected_write","token":"t","path":"/p","content":""}"#)
                .is_ok()
        );
        assert!(serde_json::from_str::<Request>(r#"{"type":"answer","id":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"type":"protected_write","token":"t","path":"/p","content":"","x":1}"#)
                .is_err()
        );
    }
}
