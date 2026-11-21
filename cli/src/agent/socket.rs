//! `agent.sock`: the socket sandboxed pis reach the daemon on, next to the control socket but separate from it. A
//! sandbox may connect to this one socket (a built-in allow in every launch's policy) and never to the control socket.
//!
//! One request per connection, one JSON line each way, except `run_command`, which streams the command's output as
//! lines before its last one. A request names its session by the token the daemon gave that pi
//! (`PITON_SESSION_TOKEN`): a request can act only for the session it came from, and only in the ways listed here: a
//! protected write (see `write.rs`), and a bash call run through the daemon (see `run.rs`, `block.rs`), each answered
//! only by the user through a client or by the user's own grants.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::{
    daemon::Daemon,
    session::{SessionCmd, SessionKey, lock},
};

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
        let token = random_bytes_hex(32)?;
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

fn random_bytes_hex(n: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `n` random bytes as hex; the clock's nanoseconds if the system's randomness can't be read (a tag, not a secret).
pub fn random_hex(n: usize) -> String {
    random_bytes_hex(n).unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("{nanos:x}")
    })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    ProtectedWrite {
        token: String,
        path: PathBuf,
        content: String,
    },
    RunCommand {
        token: String,
        #[serde(flatten)]
        request: super::RunRequest,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    ProtectedWrite(super::write::WriteOutcome),
    /// A chunk of the running command's output, base64.
    Output {
        data: String,
    },
    /// The daemon's run ended: the exit code, or 128 + the signal.
    Exit {
        code: i32,
    },
    TimedOut,
    /// Not run, with why: the user denied it, didn't answer, or the request doesn't match a running call.
    NotRun {
        reason: String,
    },
    Error {
        message: String,
    },
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
        Ok(Request::RunCommand { token, request }) => {
            // Streams: the output lines, then the end. Closing the connection (the agent's stop or timeout) drops the
            // run, which kills the command's process group.
            tokio::select! {
                () = run_command(&daemon, token, request, &mut write) => {}
                _ = closed(&mut lines) => {}
            }
            return;
        }
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
    send(&mut write, &response).await;
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, response: &Response) -> bool {
    let mut out = serde_json::to_string(response).unwrap_or_default();
    out.push('\n');
    write.write_all(out.as_bytes()).await.is_ok()
}

/// Decides a bash call with its session (a grant, the user's answer), runs it when approved, and streams it back.
async fn run_command(
    daemon: &Daemon,
    token: String,
    request: super::RunRequest,
    write: &mut tokio::net::unix::OwnedWriteHalf,
) {
    use super::{RunDecision, RunEnd, RunEvent};
    let Some(handle) = daemon
        .agent_tokens()
        .session(&token)
        .and_then(|key| daemon.session(key))
    else {
        send(
            write,
            &Response::Error {
                message: "unknown or ended session".into(),
            },
        )
        .await;
        return;
    };
    let (reply, decision) = tokio::sync::oneshot::channel();
    if handle.tx.send(SessionCmd::RunCommand { request, reply }).is_err() {
        send(
            write,
            &Response::Error {
                message: "unknown or ended session".into(),
            },
        )
        .await;
        return;
    }
    // Wakes the session when the agent stops waiting, so it closes the question at once.
    let _wake = WakeOnDrop(handle.tx.clone(), || SessionCmd::SettleQuestion);
    let approved = match decision.await {
        Ok(RunDecision::Run(approved)) => approved,
        Ok(RunDecision::NotRun { reason }) => {
            send(write, &Response::NotRun { reason }).await;
            return;
        }
        Err(_) => {
            send(
                write,
                &Response::Error {
                    message: "the session ended before the user answered".into(),
                },
            )
            .await;
            return;
        }
    };
    let helper = match crate::launch::helper() {
        Ok(helper) => helper,
        Err(e) => {
            send(write, &Response::Error { message: e.to_string() }).await;
            return;
        }
    };
    let (events, mut output) = tokio::sync::mpsc::unbounded_channel();
    let spec = super::RunSpec {
        helper: &helper,
        policy: &approved.policy,
        shell: &approved.shell,
        command: &approved.command,
        cwd: &approved.cwd,
        env: &approved.env,
        timeout: approved.timeout,
    };
    let run = super::run(spec, events);
    tokio::pin!(run);
    let end = loop {
        tokio::select! {
            end = &mut run => break end,
            Some(RunEvent::Output(chunk)) = output.recv() => {
                if !send(write, &Response::Output { data: base64(&chunk) }).await {
                    return;
                }
            }
        }
    };
    // What the command printed before it ended.
    while let Ok(RunEvent::Output(chunk)) = output.try_recv() {
        if !send(write, &Response::Output { data: base64(&chunk) }).await {
            return;
        }
    }
    let session = handle.key;
    let last = match end {
        RunEnd::Exited { code } => {
            crate::log!("session {session}: command run by the daemon exited with {code}");
            Response::Exit { code }
        }
        RunEnd::TimedOut => {
            crate::log!("session {session}: command run by the daemon timed out");
            Response::TimedOut
        }
        RunEnd::Failed { reason } => {
            crate::log!("session {session}: command run by the daemon failed: {reason}");
            Response::Error { message: reason }
        }
    };
    send(write, &last).await;
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            match i <= chunk.len() {
                true => out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
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
            let _wake = WakeOnDrop(handle.tx.clone(), || SessionCmd::SettleProtectedWrite);
            // Waits for the user, as long as it takes; the agent's tool call waits with it.
            match outcome.await {
                Ok(outcome) => Response::ProtectedWrite(outcome),
                Err(_) => Response::Error {
                    message: "the session ended before the user answered".into(),
                },
            }
        }
        Request::RunCommand { .. } => unreachable!("handled with its stream"),
    }
}

/// Asks the session to look at its pending question when dropped: the agent stopped waiting.
struct WakeOnDrop(tokio::sync::mpsc::UnboundedSender<SessionCmd>, fn() -> SessionCmd);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send((self.1)());
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
