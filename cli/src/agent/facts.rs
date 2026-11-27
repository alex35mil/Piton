//! Facts: the session's view, which only the daemon knows, asked over the channel instead of inherited from
//! environment snapshots.
//!
//! A [`Facts`] value is built once at launch and kept on the session's handle. Any caller of the session but the
//! launch helper may ask (`facts`): pi, a process pi started, or a bash call. Commands that need the session's view
//! ([`View`]) ask through [`Context`]; outside a session they read the configuration and the state database
//! themselves.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{GrantFilter, Paths, Runtime};

/// A session's view, as the daemon decided it at launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    /// Where the session's pi runs.
    pub runtime: FactRuntime,
    /// Which of the repository's committed grants the session keeps: the launch's, so a later trust decision or commit
    /// doesn't widen a running session.
    pub grants: GrantFilter,
    /// The session's protected files: the definition the daemon checks a protected write against.
    pub protected: super::Protected,
    /// The global tier.
    pub global: PathBuf,
    /// The session's workspace and what its checkout is.
    pub workspace: FactWorkspace,
}

/// A session's runtime, as `facts` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactRuntime {
    Host,
    Sandbox,
}

impl FactRuntime {
    pub fn of(mode: crate::launch::LaunchMode) -> Self {
        match mode {
            crate::launch::LaunchMode::Host => FactRuntime::Host,
            crate::launch::LaunchMode::Sandbox => FactRuntime::Sandbox,
        }
    }

    /// The permission runtime it selects.
    pub fn runtime(self) -> Runtime {
        match self {
            FactRuntime::Host => Runtime::Host,
            FactRuntime::Sandbox => Runtime::Sandbox,
        }
    }
}

/// A session's workspace: the folder pi runs in, its checkout, and the checkout's relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactWorkspace {
    pub path: PathBuf,
    pub checkout: PathBuf,
    pub relation: FactRelation,
}

/// A checkout's relation, as `facts` names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FactRelation {
    Source,
    Worker { source: PathBuf, name: String },
}

impl FactWorkspace {
    pub fn of(workspace: &crate::project::Workspace) -> Self {
        Self {
            path: workspace.path.clone(),
            checkout: workspace.checkout.clone(),
            relation: match &workspace.relation {
                crate::project::Relation::Source => FactRelation::Source,
                crate::project::Relation::Worker { source, name } => FactRelation::Worker {
                    source: source.clone(),
                    name: name.clone(),
                },
            },
        }
    }
}

/// What `facts` answers: the session's facts, or that it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FactsAnswer {
    Facts(Box<Facts>),
    Ended,
}

/// The `facts` request, as the Rust client sends it.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "facts")]
pub struct FactsRequest {}

/// Where this process runs: in a session the daemon launched (its socket and this process's token), or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    Session { socket: PathBuf, token: String },
    NoSession,
}

impl Context {
    /// From this process's environment: both channel variables set and nonempty, or no session.
    pub fn current() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        match (var(super::SOCKET_ENV), var(super::TOKEN_ENV)) {
            (Some(socket), Some(token)) => Context::Session {
                socket: PathBuf::from(socket),
                token: token.to_string_lossy().into_owned(),
            },
            (Some(_) | None, Some(_) | None) => Context::NoSession,
        }
    }
}

/// Why a session's facts couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    #[error("can't read this session's facts: {0}")]
    Client(#[from] super::client::ClientError),
    #[error("can't read this session's facts: the session ended")]
    Ended,
}

/// The view a command reads configuration with: the session's facts inside a session, this process's own outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Session(Box<Facts>),
    NoSession,
}

impl View {
    /// Asks the daemon in a session; outside one, nothing to ask.
    pub fn current() -> Result<Self, FactsError> {
        Self::of(&Context::current())
    }

    pub fn of(context: &Context) -> Result<Self, FactsError> {
        match context {
            Context::NoSession => Ok(View::NoSession),
            Context::Session { socket, token } => {
                match super::client::request::<FactsAnswer>(socket, token, &FactsRequest {})? {
                    FactsAnswer::Facts(facts) => Ok(View::Session(facts)),
                    FactsAnswer::Ended => Err(FactsError::Ended),
                }
            }
        }
    }

    /// The paths configuration is read with: `--config-home` when given, else the session's global tier, else this
    /// process's.
    pub fn paths(&self, config_home: Option<&Path>) -> Paths {
        let mut paths = Paths::current();
        match (config_home, self) {
            (Some(dir), View::Session(_) | View::NoSession) => paths.global = dir.to_path_buf(),
            (None, View::Session(facts)) => paths.global = facts.global.clone(),
            (None, View::NoSession) => {}
        }
        paths
    }

    /// The runtime permissions resolve for when the caller doesn't name one: the session's, else the host.
    pub fn runtime(&self) -> Runtime {
        match self {
            View::Session(facts) => facts.runtime.runtime(),
            View::NoSession => Runtime::Host,
        }
    }

    /// The session's grant filter, or `None` outside a session (the caller reads the state database).
    pub fn grants(&self) -> Option<&GrantFilter> {
        match self {
            View::Session(facts) => Some(&facts.grants),
            View::NoSession => None,
        }
    }

    /// The session's protected files, or `None` outside a session (the caller computes the workspace's).
    pub fn protected(&self) -> Option<&super::Protected> {
        match self {
            View::Session(facts) => Some(&facts.protected),
            View::NoSession => None,
        }
    }
}

/// Whether `pid` descends from `ancestor` through its chain of parent processes (`pid` itself excluded).
pub fn descends_from(pid: i32, ancestor: i32) -> bool {
    // A process tree deeper than this is no session's: pi's children are a few levels down at most.
    const MAX_STEPS: usize = 64;
    let mut current = pid;
    for _ in 0..MAX_STEPS {
        match parent_of(current) {
            Some(parent) if parent == ancestor => return true,
            Some(parent) if parent > 1 && parent != current => current = parent,
            Some(_) | None => return false,
        }
    }
    false
}

/// `pid`'s parent process, while it runs.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn parent_of(pid: i32) -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a zeroed `proc_bsdinfo` of the size passed; the kernel writes at most that many bytes and
    // returns how many it wrote.
    let written = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if written != size {
        return None;
    }
    // SAFETY: the kernel filled the whole struct (checked above), and a zeroed one is valid too.
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbi_ppid).ok()
}

/// `pid`'s parent process, while it runs: the fourth field of `/proc/<pid>/stat`, after the command's parentheses.
#[cfg(not(target_os = "macos"))]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real child descends from this process; this process doesn't descend from its child, nor from an unrelated pid.
    #[test]
    fn ancestry_follows_real_processes() {
        let me = std::process::id() as i32;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 5 & wait"])
            .spawn()
            .unwrap();
        let child_pid = child.id() as i32;
        assert!(descends_from(child_pid, me));
        assert!(!descends_from(me, child_pid));
        assert!(
            !descends_from(child_pid, child_pid),
            "a process isn't its own descendant"
        );
        // The shell's own child is a grandchild of this process.
        let grandchild = (0..100).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let out = std::process::Command::new("/usr/bin/pgrep")
                .args(["-P", &child_pid.to_string()])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .lines()
                .next()?
                .parse::<i32>()
                .ok()
        });
        if let Some(grandchild) = grandchild {
            assert!(descends_from(grandchild, me));
            assert!(descends_from(grandchild, child_pid));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(!descends_from(1, me));
    }

    fn facts(global: &Path) -> Facts {
        Facts {
            runtime: FactRuntime::Sandbox,
            grants: GrantFilter::All,
            protected: super::super::Protected::new(&super::super::ProtectedFacts {
                source: Path::new("/r"),
                checkout: Path::new("/r"),
                levels: &[],
                agent_dirs: &[],
                fnox_files: &[],
                global,
                home: Path::new("/h"),
                excluded: &[],
            }),
            global: global.to_path_buf(),
            workspace: FactWorkspace {
                path: "/r--w".into(),
                checkout: "/r--w".into(),
                relation: FactRelation::Worker {
                    source: "/r".into(),
                    name: "w".into(),
                },
            },
        }
    }

    /// The answer's wire shape: what the pi package's client reads.
    #[test]
    fn the_answer_is_one_typed_line() {
        let json = serde_json::to_value(FactsAnswer::Facts(Box::new(facts(Path::new("/g"))))).unwrap();
        assert_eq!(json["type"], "facts");
        assert_eq!(json["runtime"], "sandbox");
        assert_eq!(json["grants"], serde_json::json!({"kind": "all"}));
        assert_eq!(json["global"], "/g");
        assert_eq!(json["workspace"]["relation"]["kind"], "worker");
        assert_eq!(json["workspace"]["relation"]["source"], "/r");
        assert_eq!(json["protected"]["source"], "/r");
        let back: FactsAnswer = serde_json::from_value(json).unwrap();
        assert_eq!(back, FactsAnswer::Facts(Box::new(facts(Path::new("/g")))));
    }

    /// A session's view decides the global tier and runtime unless the caller names its own; outside one, this
    /// process's.
    #[test]
    fn the_view_decides_what_the_caller_leaves_open() {
        let session = View::Session(Box::new(facts(Path::new("/session-global"))));
        assert_eq!(session.paths(None).global, Path::new("/session-global"));
        assert_eq!(session.paths(Some(Path::new("/flag"))).global, Path::new("/flag"));
        assert_eq!(session.runtime(), Runtime::Sandbox);
        assert_eq!(session.grants(), Some(&GrantFilter::All));
        assert!(session.protected().is_some());
        assert_eq!(View::NoSession.paths(None).global, Paths::current().global);
        assert_eq!(View::NoSession.runtime(), Runtime::Host);
        assert_eq!(View::NoSession.grants(), None);
        assert!(View::NoSession.protected().is_none());
    }
}
