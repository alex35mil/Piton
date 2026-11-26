//! Lifts for pi's live sandbox (#408): the daemon issues Seatbelt extension tokens bound to pi's process, and pi
//! consumes them through the addon this module writes out.
//!
//! - **Tokens** are issued with `sandbox_extension_issue_*_to_process` for the audit token of pi's connection on
//!   `agent.sock` ([`super::channel::Peer`]), so only pi's process can consume one; a token held by anyone else is
//!   useless. Each is in Piton's own class for its kind (`rules::LIFT_*`), which only the companion rules of pi's
//!   profile name, so a token reopens only what a liftable deny closed.
//! - **The addon** (`cli/native/lift.c`, compiled by `build.rs` and embedded here) is written to
//!   `<state>/native/lift.node` when the daemon starts; every sandbox may read that folder. The pi package loads it from
//!   next to `agent.sock` and calls `consume` and `release`.
//!
//! Lifts are a Seatbelt mechanism: elsewhere every issue is [`IssueError::Unsupported`].

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use super::answer::{Answer, Never, Once};
use crate::{daemon::Daemon, launch::sandbox::grant::LiveLift, session::SessionCmd};

/// An explicit request from code in pi's process: widen its sandbox for `targets`, for one action.
#[derive(Debug, Clone)]
pub struct LiftRequest {
    pub targets: Vec<LiveLift>,
    /// Why, as the code says it: shown in the question.
    pub reason: String,
}

/// The user's answer to an explicit lift: once (one action, which the client bounds by releasing), for this session,
/// or deny. There's no saved form: code that always needs a path asks for an entry instead.
pub type LiftAnswer = Answer<Once<()>, (), Never, ()>;

/// What the session decided about an explicit lift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiftDecision {
    /// Approved (or covered by a session approval): issue for these, resolved.
    Granted(Vec<LiveLift>),
    Denied,
    /// Not asked: a target is floor, unsupported, or another question holds the slot.
    Refused {
        reason: String,
    },
}

/// What `lift` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiftResponse {
    /// The tokens, one per target in order, bound to the requesting process.
    Granted {
        tokens: Vec<String>,
    },
    Denied,
    /// Not asked or not issued: a target is floor, another question holds the slot, the kernel refused a token. Not
    /// `refused`, which is the channel's own refusal of a request it didn't handle.
    NotGranted {
        reason: String,
    },
    /// Not macOS.
    Unsupported,
    /// The session ended before the user answered.
    Ended,
}

/// Asks `session` about an explicit lift and issues its tokens for `peer` on approval.
pub async fn request(
    daemon: &Daemon,
    session: crate::session::SessionKey,
    peer: &super::channel::Peer,
    request: LiftRequest,
) -> LiftResponse {
    if !cfg!(target_os = "macos") {
        return LiftResponse::Unsupported;
    }
    let Some(handle) = daemon.session(session) else {
        return LiftResponse::Ended;
    };
    let (reply, decision) = tokio::sync::oneshot::channel();
    if handle.tx.send(SessionCmd::Lift { request, reply }).is_err() {
        return LiftResponse::Ended;
    }
    let _wake = super::socket::WakeOnDrop(handle.tx.clone(), || SessionCmd::SettleLift);
    match decision.await {
        Ok(LiftDecision::Granted(targets)) => {
            let mut tokens = Vec::with_capacity(targets.len());
            for target in &targets {
                match issue_for(target, peer) {
                    Ok(token) => tokens.push(token),
                    Err(e) => return LiftResponse::NotGranted { reason: e.to_string() },
                }
            }
            LiftResponse::Granted { tokens }
        }
        Ok(LiftDecision::Denied) => LiftResponse::Denied,
        Ok(LiftDecision::Refused { reason }) => LiftResponse::NotGranted { reason },
        Err(_) => LiftResponse::Ended,
    }
}

/// A token for `lift`, bound to `peer`'s process.
pub fn issue_for(lift: &LiveLift, peer: &super::channel::Peer) -> Result<String, IssueError> {
    #[cfg(target_os = "macos")]
    {
        issue(lift, peer.audit)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = peer;
        issue(lift, ())
    }
}

/// The first option of a lift question, which decides nothing (clients highlight the first option).
const ONCE: &str = "Allow once";
const SESSION: &str = "Allow for this session";
const DENY: &str = "Deny";

/// The dialog for an explicit lift, in pi's `select` format.
pub fn dialog(id: &str, request: &LiftRequest) -> Value {
    let targets: Vec<String> = request.targets.iter().map(|t| format!("- {}", t.describe())).collect();
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": format!(
            "pi asks to widen its sandbox for one action: {}\n{}\n\nWhile it's open, the agent's write and edit tools can change it too.",
            request.reason,
            targets.join("\n")
        ),
        "options": [super::block::CHOOSE_BELOW, ONCE, SESSION, DENY],
    })
}

/// A client's answer to a lift dialog: `None` asks again (the first option); a dismissal or an unknown value denies.
pub fn read_answer(answer: &serde_json::Map<String, Value>) -> Option<LiftAnswer> {
    match answer.get("value").and_then(Value::as_str) {
        Some(super::block::CHOOSE_BELOW) => None,
        Some(ONCE) => Some(LiftAnswer::Narrowest(Once(()))),
        Some(SESSION) => Some(LiftAnswer::Session(())),
        Some(_) | None => Some(LiftAnswer::Deny(())),
    }
}

/// The addon's folder in the daemon's state folder: readable by every sandbox, never writable.
pub fn folder(state_dir: &Path) -> PathBuf {
    state_dir.join("native")
}

/// The addon's file name in [`folder`].
pub const ADDON: &str = "lift.node";

/// The addon, as built for this binary.
#[cfg(target_os = "macos")]
const ADDON_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lift.node"));

/// Writes the addon to `<state>/native/lift.node` (replaced whole, owner-only), so the pi package of every session finds
/// the build that matches this daemon. Off macOS there's none.
pub fn install(state_dir: &Path) -> std::io::Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder(state_dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(ADDON);
        if std::fs::read(&path).is_ok_and(|current| current == ADDON_BYTES) {
            return Ok(Some(path));
        }
        let partial = dir.join(format!(".{ADDON}.{}", std::process::id()));
        std::fs::write(&partial, ADDON_BYTES)?;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o500))?;
        std::fs::rename(&partial, &path)?;
        Ok(Some(path))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state_dir;
        Ok(None)
    }
}

/// Why a token wasn't issued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IssueError {
    /// Not macOS: sandbox extensions are Seatbelt's.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    #[error("lifts need macOS's sandbox")]
    Unsupported,
    /// The path can't be named to the kernel (not UTF-8, a NUL byte).
    #[error("{0} can't be passed to the sandbox")]
    Unnamed(String),
    /// The kernel refused: a path whose folder doesn't exist, a process that ended.
    #[error("the sandbox refused a token for {what}: {reason}")]
    Refused { what: String, reason: String },
}

/// A token for `lift`, bound to the process `audit` names. Paths must be resolved (Seatbelt matches resolved paths).
#[cfg(target_os = "macos")]
pub fn issue(lift: &LiveLift, audit: super::channel::AuditToken) -> Result<String, IssueError> {
    use crate::launch::sandbox::{LIFT_LISTEN, LIFT_MACH, LIFT_READ, LIFT_WRITE};
    let token = ffi::AuditToken { val: audit.0 };
    match lift {
        LiveLift::Read { path } => ffi::file(LIFT_READ, path, token),
        LiveLift::Write { path } => ffi::file(LIFT_WRITE, path, token),
        LiveLift::Service { name } => ffi::mach(LIFT_MACH, name, token),
        LiveLift::Listen => ffi::generic(LIFT_LISTEN, token),
    }
}

/// Lifts are Seatbelt's.
#[cfg(not(target_os = "macos"))]
pub fn issue(_lift: &LiveLift, _audit: ()) -> Result<String, IssueError> {
    Err(IssueError::Unsupported)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod ffi {
    use std::{
        ffi::{CStr, CString, c_char},
        path::Path,
    };

    use super::IssueError;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct AuditToken {
        pub val: [u32; 8],
    }

    // libsystem_sandbox, part of libSystem. Each returns a malloc'd token, or null with errno set.
    unsafe extern "C" {
        fn sandbox_extension_issue_file_to_process(
            class: *const c_char,
            path: *const c_char,
            flags: u32,
            token: AuditToken,
        ) -> *mut c_char;
        fn sandbox_extension_issue_mach_to_process(
            class: *const c_char,
            name: *const c_char,
            flags: u32,
            token: AuditToken,
        ) -> *mut c_char;
        fn sandbox_extension_issue_generic_to_process(
            class: *const c_char,
            flags: u32,
            token: AuditToken,
        ) -> *mut c_char;
    }

    fn cstring(text: &str) -> Result<CString, IssueError> {
        CString::new(text).map_err(|_| IssueError::Unnamed(text.to_string()))
    }

    /// Takes the token the kernel returned, freeing its buffer.
    fn taken(raw: *mut c_char, what: &str) -> Result<String, IssueError> {
        if raw.is_null() {
            return Err(IssueError::Refused {
                what: what.to_string(),
                reason: std::io::Error::last_os_error().to_string(),
            });
        }
        // SAFETY: a non-null result is a NUL-terminated string the kernel library allocated with malloc.
        let token = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        // SAFETY: freed once, with the allocator that made it.
        unsafe { libc::free(raw.cast()) };
        Ok(token)
    }

    pub fn file(class: &str, path: &Path, token: AuditToken) -> Result<String, IssueError> {
        let text = path
            .to_str()
            .ok_or_else(|| IssueError::Unnamed(path.display().to_string()))?;
        let (class, c_path) = (cstring(class)?, cstring(text)?);
        // SAFETY: valid C strings for the call's duration; the token is passed by value.
        let raw = unsafe { sandbox_extension_issue_file_to_process(class.as_ptr(), c_path.as_ptr(), 0, token) };
        taken(raw, text)
    }

    pub fn mach(class: &str, name: &str, token: AuditToken) -> Result<String, IssueError> {
        let (class, c_name) = (cstring(class)?, cstring(name)?);
        // SAFETY: as above.
        let raw = unsafe { sandbox_extension_issue_mach_to_process(class.as_ptr(), c_name.as_ptr(), 0, token) };
        taken(raw, name)
    }

    pub fn generic(class: &str, token: AuditToken) -> Result<String, IssueError> {
        let class = cstring(class)?;
        // SAFETY: as above.
        let raw = unsafe { sandbox_extension_issue_generic_to_process(class.as_ptr(), 0, token) };
        taken(raw, "listening")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every option reads back; a dismissal denies; the first decides nothing.
    #[test]
    fn lift_answers_read_back() {
        let d = dialog(
            "l",
            &LiftRequest {
                targets: vec![LiveLift::Write {
                    path: "/w/.git/config".into(),
                }],
                reason: "delete a merged branch".into(),
            },
        );
        let title = d["title"].as_str().unwrap();
        assert!(
            title.contains("delete a merged branch") && title.contains("/w/.git/config"),
            "{title}"
        );
        let answers: Vec<Option<LiftAnswer>> = d["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| read_answer(json!({"value": v}).as_object().unwrap()))
            .collect();
        assert_eq!(
            answers,
            [
                None,
                Some(LiftAnswer::Narrowest(Once(()))),
                Some(LiftAnswer::Session(())),
                Some(LiftAnswer::Deny(())),
            ]
        );
        assert_eq!(
            read_answer(json!({"cancelled": true}).as_object().unwrap()),
            Some(LiftAnswer::Deny(()))
        );
    }

    /// The addon is written once, replaced whole, and read-only.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_addon_is_written_to_the_state_folder() {
        let state = tempfile::tempdir().unwrap();
        let path = install(state.path()).unwrap().unwrap();
        assert_eq!(path, state.path().join("native/lift.node"));
        assert_eq!(std::fs::read(&path).unwrap(), ADDON_BYTES);
        // A Mach-O bundle.
        assert_eq!(&ADDON_BYTES[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
        assert_eq!(install(state.path()).unwrap().unwrap(), path, "unchanged: kept");
    }

    /// A token for this test process, which isn't sandboxed (the kernel issues one), or refused inside a sandbox.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn tokens_are_issued_for_a_peer() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let peer = super::super::channel::Peer::of(&a).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap().join("new-file");
        let sandboxed = match crate::launch::sandbox::containment() {
            crate::launch::sandbox::Containment::Inherited => true,
            crate::launch::sandbox::Containment::Apply => false,
        };
        for lift in [
            LiveLift::Write { path: path.clone() },
            LiveLift::Read { path: path.clone() },
            LiveLift::Service {
                name: "com.apple.cfprefsd.daemon".into(),
            },
            LiveLift::Listen,
        ] {
            match (issue(&lift, peer.audit), sandboxed) {
                (Ok(token), false) => assert!(!token.is_empty()),
                (Err(IssueError::Refused { .. }), true) => {}
                (other, _) => panic!("{lift:?}: {other:?} (sandboxed: {sandboxed})"),
            }
        }
        let missing = dir.path().join("no/such/folder/file");
        match issue(&LiveLift::Write { path: missing }, peer.audit) {
            Err(IssueError::Refused { .. }) => {}
            other => panic!("a missing folder chain: {other:?}"),
        }
    }
}
