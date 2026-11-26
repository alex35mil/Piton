//! The channel's contract: who may ask the daemon what, over `agent.sock`.
//!
//! Every request names its caller with a token, and the daemon also checks the process on the other end of the
//! connection ([`Peer`]): a token is accepted only from the process it was given to. Each [`Caller`] has its own
//! token, so a bash call never holds pi's, and [`authorize`] says which callers may send each [`Request`], matched
//! exhaustively.
//!
//! | Caller | Token | Accepted from |
//! | --- | --- | --- |
//! | pi's process | the session token, issued at launch | pi's process |
//! | a process pi started | pi's session token | a descendant of pi's process (`facts` only) |
//! | a bash call | a call token, issued when the daemon starts the call | the call's process group |
//! | the launch helper | the one-time launch token | any process (spent once) |

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::answer::Asks;
use crate::session::{SessionKey, lock};

/// The variable a sandboxed process reads its own token from: pi's session token, or a bash call's.
pub const TOKEN_ENV: &str = "PITON_AGENT_TOKEN";
/// The variable a sandboxed process reads the socket path from.
pub const SOCKET_ENV: &str = "PITON_AGENT_SOCKET";
/// The variable that carries a launch's one-time token to the helper (removed before pi starts).
pub const LAUNCH_TOKEN_ENV: &str = "PITON_LAUNCH_TOKEN";

/// Who sent a request, resolved from its token and checked against the connecting process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// pi's own process for a session: its extensions, workflow tools, MCP servers' requests through them.
    Pi { session: SessionKey },
    /// A process pi started (a `piton config` call, an MCP server, the reviewer pi, a host session's bash), with pi's
    /// token: it may read facts, never act as pi.
    PiChild { session: SessionKey },
    /// A bash call the daemon runs for a session.
    Call { session: SessionKey, call: CallId },
    /// The launch helper, before it becomes pi.
    Launcher { session: SessionKey },
}

impl Caller {
    pub fn session(self) -> SessionKey {
        match self {
            Caller::Pi { session }
            | Caller::PiChild { session }
            | Caller::Call { session, .. }
            | Caller::Launcher { session } => session,
        }
    }

    /// How a refusal names it.
    pub fn kind(self) -> CallerKind {
        match self {
            Caller::Pi { .. } => CallerKind::Pi,
            Caller::PiChild { .. } => CallerKind::PiChild,
            Caller::Call { .. } => CallerKind::Call,
            Caller::Launcher { .. } => CallerKind::Launcher,
        }
    }
}

/// A caller's kind, as a refusal reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerKind {
    Pi,
    PiChild,
    Call,
    Launcher,
}

/// One run the daemon started for a session: what a call token names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallId(pub u64);

/// The process on the other end of a connection, read once when it's accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub pid: i32,
    /// The process's group: a bash call's processes share the group the daemon made for the call.
    pub group: Option<i32>,
    /// The process's audit token (macOS): what lifts bind their tokens to.
    #[cfg(target_os = "macos")]
    pub audit: AuditToken,
}

/// A macOS audit token: identifies one process for its whole life (a pid can be reused, the token can't).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditToken(pub [u32; 8]);

#[cfg(target_os = "macos")]
impl AuditToken {
    /// The pid it names (`audit_token_to_pid`: the sixth word).
    pub fn pid(self) -> i32 {
        self.0[5] as i32
    }
}

/// Why the connecting process couldn't be identified.
#[derive(Debug, thiserror::Error)]
#[error("can't identify the connecting process: {0}")]
pub struct PeerError(#[from] std::io::Error);

impl Peer {
    /// The process on the other end of `stream`: on macOS from its audit token (so the pid and the token name the same
    /// process), elsewhere from its credentials.
    pub fn of(stream: &tokio::net::UnixStream) -> Result<Self, PeerError> {
        #[cfg(target_os = "macos")]
        {
            use nix::sys::socket::{getsockopt, sockopt::LocalPeerToken};
            let token = getsockopt(stream, LocalPeerToken).map_err(std::io::Error::from)?;
            let audit = AuditToken(token.val);
            let pid = audit.pid();
            Ok(Self {
                pid,
                group: group_of(pid),
                audit,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let pid = stream
                .peer_cred()?
                .pid()
                .ok_or_else(|| std::io::Error::other("the system didn't report the peer's pid"))?;
            Ok(Self {
                pid,
                group: group_of(pid),
            })
        }
    }
}

/// `pid`'s process group, when it still runs.
fn group_of(pid: i32) -> Option<i32> {
    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid)))
        .ok()
        .map(nix::unistd::Pid::as_raw)
}

/// Where pi's process is, for a session: what a session token is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PiProcess {
    /// Behind fnox, pi is fnox's child: the helper reports it (`launched`) before it becomes pi.
    Awaited,
    Known {
        pid: i32,
    },
}

/// A session's live credential.
#[derive(Debug, Clone, Copy)]
enum Credential {
    Session {
        session: SessionKey,
    },
    Call {
        session: SessionKey,
        call: CallId,
        group: CallGroup,
    },
    Launch {
        session: SessionKey,
    },
}

/// A call's process group: unknown from when the token is put in the call's environment until the daemon started it.
#[derive(Debug, Clone, Copy)]
enum CallGroup {
    Starting,
    Started(i32),
}

/// Why a token wasn't accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Unaccepted {
    /// No live token: a guess, or one whose session or call ended.
    UnknownToken,
    /// The token belongs to another process.
    WrongProcess,
    /// The process the token belongs to isn't known yet: pi's before the daemon recorded its start, a call's before the
    /// daemon started it. Only for a moment; the socket asks again before refusing.
    NotYet,
}

/// Every live token, and pi's process per session.
#[derive(Debug, Default)]
pub struct Tokens {
    tokens: std::sync::Mutex<HashMap<String, Credential>>,
    pi: std::sync::Mutex<HashMap<SessionKey, PiProcess>>,
    next_call: std::sync::atomic::AtomicU64,
}

impl Tokens {
    /// A session token for `session`, and its launch token when the helper reports (behind fnox). pi's process is the
    /// launched process `pid` unless the helper reports it.
    pub fn issue_session(&self, session: SessionKey, launch: bool) -> std::io::Result<SessionTokens> {
        let token = random_bytes_hex(32)?;
        lock(&self.tokens).insert(token.clone(), Credential::Session { session });
        lock(&self.pi).insert(session, PiProcess::Awaited);
        let launch = match launch {
            true => {
                let launch = random_bytes_hex(32)?;
                lock(&self.tokens).insert(launch.clone(), Credential::Launch { session });
                Some(launch)
            }
            false => None,
        };
        Ok(SessionTokens { session: token, launch })
    }

    /// pi's process for `session`: the process the daemon started, or the one the helper reported.
    pub fn pi_started(&self, session: SessionKey, pid: i32) {
        if let Some(entry) = lock(&self.pi).get_mut(&session) {
            *entry = PiProcess::Known { pid };
        }
    }

    /// A call token for a run the daemon is about to start for `session`: put in the call's environment, then bound to
    /// the call's process group with [`CallToken::started`]. Forgotten when the returned guard drops.
    pub fn issue_call(self: &std::sync::Arc<Self>, session: SessionKey) -> std::io::Result<CallToken> {
        let token = random_bytes_hex(32)?;
        let call = CallId(self.next_call.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        lock(&self.tokens).insert(
            token.clone(),
            Credential::Call {
                session,
                call,
                group: CallGroup::Starting,
            },
        );
        Ok(CallToken {
            token,
            tokens: std::sync::Arc::downgrade(self),
        })
    }

    fn call_started(&self, token: &str, group: i32) {
        if let Some(Credential::Call { group: slot, .. }) = lock(&self.tokens).get_mut(token) {
            *slot = CallGroup::Started(group);
        }
    }

    /// The caller `token` names, checked against `peer`. A launch token is spent.
    pub fn accept(&self, token: &str, peer: &Peer) -> Result<Caller, Unaccepted> {
        let mut tokens = lock(&self.tokens);
        let credential = *tokens.get(token).ok_or(Unaccepted::UnknownToken)?;
        match credential {
            Credential::Session { session } => match lock(&self.pi).get(&session) {
                Some(PiProcess::Known { pid }) if *pid == peer.pid => Ok(Caller::Pi { session }),
                Some(PiProcess::Known { pid }) if super::facts::descends_from(peer.pid, *pid) => {
                    Ok(Caller::PiChild { session })
                }
                Some(PiProcess::Known { .. }) | None => Err(Unaccepted::WrongProcess),
                Some(PiProcess::Awaited) => Err(Unaccepted::NotYet),
            },
            Credential::Call { session, call, group } => match group {
                CallGroup::Started(group) if peer.group == Some(group) => Ok(Caller::Call { session, call }),
                CallGroup::Started(_) => Err(Unaccepted::WrongProcess),
                CallGroup::Starting => Err(Unaccepted::NotYet),
            },
            Credential::Launch { session } => {
                tokens.remove(token);
                Ok(Caller::Launcher { session })
            }
        }
    }

    /// Whether `session`'s tokens are still live: it's launching or running.
    pub fn live(&self, session: SessionKey) -> bool {
        lock(&self.pi).contains_key(&session)
    }

    /// Forgets every token of `session`.
    pub fn forget(&self, session: SessionKey) {
        lock(&self.tokens).retain(|_, c| match c {
            Credential::Session { session: s }
            | Credential::Call { session: s, .. }
            | Credential::Launch { session: s } => *s != session,
        });
        lock(&self.pi).remove(&session);
    }

    fn forget_token(&self, token: &str) {
        lock(&self.tokens).remove(token);
    }
}

/// A session's tokens at launch.
#[derive(Debug, Clone)]
pub struct SessionTokens {
    pub session: String,
    /// The helper's one-time token, when it reports pi's process.
    pub launch: Option<String>,
}

/// A live call token: forgotten when dropped (the run ended).
#[derive(Debug)]
pub struct CallToken {
    pub token: String,
    tokens: std::sync::Weak<Tokens>,
}

impl CallToken {
    /// The call started in process group `group`: only its processes may use the token.
    pub fn started(&self, group: i32) {
        if let Some(tokens) = self.tokens.upgrade() {
            tokens.call_started(&self.token, group);
        }
    }
}

impl Drop for CallToken {
    fn drop(&mut self) {
        if let Some(tokens) = self.tokens.upgrade() {
            tokens.forget_token(&self.token);
        }
    }
}

pub(super) fn random_bytes_hex(n: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// One line a client sends: its token and the request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub token: String,
    pub request: Request,
}

/// What a caller asks the daemon.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// The session's facts (`facts.rs`): asked by anyone in the session, never approval-gated. A struct variant, so an
    /// unknown field is refused as for every other request.
    Facts {},
    /// Write a protected file, after the user approves the diff (`write.rs`).
    ProtectedWrite { path: std::path::PathBuf, content: String },
    /// Run a bash call or a `!` command through the daemon (`run.rs`).
    RunCommand {
        #[serde(flatten)]
        request: super::RunRequest,
    },
    /// The helper is about to become pi: its process is pi's, and behind fnox with a filtered network, fnox's proxy
    /// URL is the gate's route.
    Launched {
        #[serde(default)]
        upstream: Option<String>,
    },
}

impl Request {
    pub fn kind(&self) -> &'static str {
        match self {
            Request::Facts {} => "facts",
            Request::ProtectedWrite { .. } => "protected_write",
            Request::RunCommand { .. } => "run_command",
            Request::Launched { .. } => "launched",
        }
    }

    /// Whether it asks the user, and the answers it offers, read from its answer type.
    pub fn asks(&self) -> Asks {
        match self {
            Request::Facts {} => Asks::Never,
            // A write changes what later sessions may do: once (the diff as shown or the user's version), or deny.
            Request::ProtectedWrite { .. } => Asks::Always(super::write::WriteAnswer::OFFERED),
            // A plain call runs as the session's policy and saved entries say; a rerun asks.
            Request::RunCommand { .. } => Asks::When(super::question::Answer::OFFERED),
            Request::Launched { .. } => Asks::Never,
        }
    }
}

/// A request its caller may send, with the session it acts for.
#[derive(Debug)]
pub enum Authorized {
    Facts {
        session: SessionKey,
    },
    ProtectedWrite {
        session: SessionKey,
        request: super::write::WriteRequest,
    },
    RunCommand {
        session: SessionKey,
        request: super::RunRequest,
    },
    Launched {
        session: SessionKey,
        upstream: Option<String>,
    },
}

/// Which callers may send each request: everyone in the session but the launch helper reads facts; only pi's process
/// writes protected files and runs bash calls; the launch helper reports pi's start.
pub fn authorize(request: Request, caller: Caller) -> Result<Authorized, Refused> {
    let not_allowed = |request: &Request| Refused::NotAllowed {
        caller: caller.kind(),
        request: request.kind(),
    };
    match (request, caller) {
        (Request::Facts {}, Caller::Pi { session } | Caller::PiChild { session } | Caller::Call { session, .. }) => {
            Ok(Authorized::Facts { session })
        }
        (request @ Request::Facts {}, Caller::Launcher { .. }) => Err(not_allowed(&request)),
        (Request::ProtectedWrite { path, content }, Caller::Pi { session }) => Ok(Authorized::ProtectedWrite {
            session,
            request: super::write::WriteRequest { path, content },
        }),
        (Request::RunCommand { request }, Caller::Pi { session }) => Ok(Authorized::RunCommand { session, request }),
        (Request::Launched { upstream }, Caller::Launcher { session }) => {
            Ok(Authorized::Launched { session, upstream })
        }
        (
            request @ (Request::ProtectedWrite { .. } | Request::RunCommand { .. }),
            Caller::PiChild { .. } | Caller::Call { .. } | Caller::Launcher { .. },
        ) => Err(not_allowed(&request)),
        (request @ Request::Launched { .. }, Caller::Pi { .. } | Caller::PiChild { .. } | Caller::Call { .. }) => {
            Err(not_allowed(&request))
        }
    }
}

/// A request refused before it was handled: the one shape every request kind shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Refused {
    /// The line isn't a request.
    Invalid { message: String },
    /// The connecting process couldn't be identified.
    Unidentified,
    /// The token wasn't accepted.
    Token(Unaccepted),
    /// The caller may not send this request.
    NotAllowed { caller: CallerKind, request: &'static str },
    /// The session the caller belongs to ended.
    Ended,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Invalid { message } => write!(f, "invalid request: {message}"),
            Refused::Unidentified => write!(f, "the connecting process couldn't be identified"),
            Refused::Token(Unaccepted::UnknownToken) => write!(f, "unknown or ended session"),
            Refused::Token(Unaccepted::WrongProcess) => write!(f, "the token belongs to another process"),
            Refused::Token(Unaccepted::NotYet) => write!(f, "the token's process isn't known yet"),
            Refused::NotAllowed { caller, request } => {
                let who = match caller {
                    CallerKind::Pi => "pi's process",
                    CallerKind::Call => "a bash call",
                    CallerKind::Launcher => "the launch helper",
                    CallerKind::PiChild => "a process pi started",
                };
                write!(f, "{who} can't send {request}")
            }
            Refused::Ended => write!(f, "unknown or ended session"),
        }
    }
}

/// The line a refusal is sent as: `{"type": "refused", "why": …, "message": …}`.
pub fn refused_line(refused: &Refused) -> serde_json::Value {
    let mut value = serde_json::to_value(refused).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("type".into(), "refused".into());
        object.insert("message".into(), refused.to_string().into());
    }
    value
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn peer(pid: i32, group: i32) -> Peer {
        Peer {
            pid,
            group: Some(group),
            #[cfg(target_os = "macos")]
            audit: AuditToken([0, 0, 0, 0, 0, pid as u32, 0, 0]),
        }
    }

    /// A session token is pi's from pi's process, a process pi started's from a descendant of pi (a real one: this
    /// test process plays pi), and nobody's from anywhere else.
    #[test]
    fn a_session_token_is_pi_s_or_its_children_s() {
        let tokens = Tokens::default();
        let issued = tokens.issue_session(4, false).unwrap();
        let me = std::process::id() as i32;
        tokens.pi_started(4, me);
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let pid = child.id() as i32;
        assert_eq!(
            tokens.accept(&issued.session, &peer(pid, pid)),
            Ok(Caller::PiChild { session: 4 })
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            tokens.accept(&issued.session, &peer(1, 1)),
            Err(Unaccepted::WrongProcess),
            "not pi's"
        );
    }

    /// A session token works only from pi's process, once it's known; a guess never does.
    #[test]
    fn a_session_token_is_pi_s_alone() {
        let tokens = Tokens::default();
        let issued = tokens.issue_session(1, false).unwrap();
        assert_eq!(issued.session.len(), 64);
        assert!(issued.launch.is_none());
        assert_eq!(tokens.accept(&issued.session, &peer(10, 10)), Err(Unaccepted::NotYet));
        tokens.pi_started(1, 10);
        assert_eq!(
            tokens.accept(&issued.session, &peer(10, 10)),
            Ok(Caller::Pi { session: 1 })
        );
        assert_eq!(
            tokens.accept(&issued.session, &peer(11, 10)),
            Err(Unaccepted::WrongProcess),
            "pi's child"
        );
        assert_eq!(tokens.accept("guess", &peer(10, 10)), Err(Unaccepted::UnknownToken));
        tokens.forget(1);
        assert_eq!(
            tokens.accept(&issued.session, &peer(10, 10)),
            Err(Unaccepted::UnknownToken)
        );
    }

    /// A call token works from any process in the call's group once it started, until the run ends.
    #[test]
    fn a_call_token_is_its_group_s_until_the_run_ends() {
        let tokens = Arc::new(Tokens::default());
        let call = tokens.issue_call(2).unwrap();
        assert_eq!(tokens.accept(&call.token, &peer(501, 500)), Err(Unaccepted::NotYet));
        call.started(500);
        let caller = tokens.accept(&call.token, &peer(501, 500)).unwrap();
        assert!(matches!(caller, Caller::Call { session: 2, .. }));
        assert_eq!(
            tokens.accept(&call.token, &peer(600, 600)),
            Err(Unaccepted::WrongProcess)
        );
        let other = tokens.issue_call(2).unwrap();
        other.started(700);
        assert_ne!(other.token, call.token);
        let token = call.token.clone();
        drop(call);
        assert_eq!(tokens.accept(&token, &peer(501, 500)), Err(Unaccepted::UnknownToken));
        assert!(tokens.accept(&other.token, &peer(700, 700)).is_ok());
    }

    /// The launch token is spent once, from whichever process (the helper isn't pi yet).
    #[test]
    fn the_launch_token_is_spent_once() {
        let tokens = Tokens::default();
        let issued = tokens.issue_session(3, true).unwrap();
        let launch = issued.launch.unwrap();
        assert_eq!(
            tokens.accept(&launch, &peer(42, 42)),
            Ok(Caller::Launcher { session: 3 })
        );
        assert_eq!(tokens.accept(&launch, &peer(42, 42)), Err(Unaccepted::UnknownToken));
    }

    /// Each request is authorized for exactly the callers its table row names.
    #[test]
    fn requests_name_their_callers() {
        let pi = Caller::Pi { session: 1 };
        let child = Caller::PiChild { session: 1 };
        let call = Caller::Call {
            session: 1,
            call: CallId(0),
        };
        let launcher = Caller::Launcher { session: 1 };
        let lines = [
            r#"{"token":"t","request":{"type":"facts"}}"#,
            r#"{"token":"t","request":{"type":"protected_write","path":"/p","content":""}}"#,
            r#"{"token":"t","request":{"type":"run_command","call_id":"c","command":"ls","cwd":"/w"}}"#,
            r#"{"token":"t","request":{"type":"launched","upstream":"http://u"}}"#,
        ];
        let allowed = |line: &str, caller| {
            let request = serde_json::from_str::<Envelope>(line).unwrap().request;
            authorize(request, caller).is_ok()
        };
        let table: Vec<[bool; 4]> = lines
            .iter()
            .map(|line| {
                [
                    allowed(line, pi),
                    allowed(line, child),
                    allowed(line, call),
                    allowed(line, launcher),
                ]
            })
            .collect();
        assert_eq!(
            table,
            [
                [true, true, true, false],
                [true, false, false, false],
                [true, false, false, false],
                [false, false, false, true]
            ]
        );
        let request = serde_json::from_str::<Envelope>(lines[1]).unwrap().request;
        assert_eq!(
            authorize(request, child).unwrap_err().to_string(),
            "a process pi started can't send protected_write"
        );
        let request = serde_json::from_str::<Envelope>(lines[1]).unwrap().request;
        assert_eq!(
            authorize(request, call).unwrap_err(),
            Refused::NotAllowed {
                caller: CallerKind::Call,
                request: "protected_write"
            }
        );
    }

    /// Each request's question is the request table's (docs/design.md, "The channel to the daemon").
    #[test]
    fn requests_declare_their_questions() {
        use super::super::answer::{Narrowest, Offered};
        let parse = |line: &str| serde_json::from_str::<Envelope>(line).unwrap().request;
        let offered = |session, always| Offered {
            narrowest: Narrowest::Once,
            session,
            always,
        };
        assert_eq!(
            parse(r#"{"token":"t","request":{"type":"protected_write","path":"/p","content":""}}"#).asks(),
            Asks::Always(offered(false, false))
        );
        assert_eq!(
            parse(r#"{"token":"t","request":{"type":"run_command","call_id":"c","command":"ls","cwd":"/w"}}"#).asks(),
            Asks::When(offered(true, true))
        );
        assert_eq!(
            parse(r#"{"token":"t","request":{"type":"launched"}}"#).asks(),
            Asks::Never
        );
        assert_eq!(parse(r#"{"token":"t","request":{"type":"facts"}}"#).asks(), Asks::Never);
    }

    #[test]
    fn envelopes_are_strict() {
        for bad in [
            r#"{"type":"protected_write","token":"t","path":"/p","content":""}"#,
            r#"{"token":"t","request":{"type":"answer","id":"x"}}"#,
            r#"{"token":"t","request":{"type":"protected_write","path":"/p","content":"","x":1}}"#,
            r#"{"token":"t","request":{"type":"launched"},"x":1}"#,
            r#"{"token":"t","request":{"type":"facts","x":1}}"#,
        ] {
            assert!(serde_json::from_str::<Envelope>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn refusals_say_why() {
        let line = refused_line(&Refused::NotAllowed {
            caller: CallerKind::Call,
            request: "protected_write",
        });
        assert_eq!(line["type"], "refused");
        assert_eq!(line["why"], "not_allowed");
        assert_eq!(line["caller"], "call");
        assert_eq!(line["message"], "a bash call can't send protected_write");
        let line = refused_line(&Refused::Token(Unaccepted::WrongProcess));
        assert_eq!(line["why"], "token");
        assert_eq!(line["reason"], "wrong_process");
    }

    /// The peer of a real connection is this process.
    #[tokio::test]
    async fn the_peer_is_the_connecting_process() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let peer = Peer::of(&a).unwrap();
        assert_eq!(peer.pid, std::process::id() as i32);
        assert_eq!(peer.group, group_of(std::process::id() as i32));
    }
}
