//! A change to a protected file an agent asks for: a write, a delete, or a rename. The daemon asks the user (a dialog in
//! every client), then makes the change itself. The agent can ask (over `agent.sock`, see `socket.rs`) but never
//! answer: answers come only from clients.
//!
//! A delete or rename shows a plain `select` that says what it does. A write's dialog is pi's file review format, so pi.nvim opens its diff view and the apps their review sheet: a `select`
//! whose title is `{"prompt", "toolName", "toolInput"}` with `toolName` `write` and `toolInput {path, content}`, plus
//! `current` (the file's content now, null for a new file) so a client that can't read the file shows a diff. pi.nvim
//! answers `"Reject"` or a JSON `{"result": "Accepted" | "AcceptModified" | "Rejected", "content"?, "notes"?}`; on
//! acceptance it has already written the file itself (its diff view writes on accept), which the daemon then confirms
//! by writing the same bytes again. The daemon's write, not the client's, is the one that counts: a client without
//! write access (the phone) answers the same way.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::answer::{Never, Once};
use crate::{daemon::Daemon, session::SessionCmd};

/// A write from the agent.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteRequest {
    /// Absolute path (the agent's view of it; resolved and checked by the daemon).
    pub path: PathBuf,
    /// The whole new content.
    pub content: String,
}

/// A delete from the agent: one file or symlink (the link, not its target).
#[derive(Debug, Clone)]
pub struct DeleteRequest {
    pub path: PathBuf,
}

/// A rename from the agent: a file or symlink to a path that doesn't exist yet. Both ends must be protected.
#[derive(Debug, Clone)]
pub struct RenameRequest {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// A change to a protected file, as the session gets it: the request and where its outcome goes.
#[derive(Debug)]
pub enum ProtectedAsk {
    Write {
        request: WriteRequest,
        reply: tokio::sync::oneshot::Sender<WriteOutcome>,
    },
    Delete {
        request: DeleteRequest,
        reply: tokio::sync::oneshot::Sender<ChangeOutcome>,
    },
    Rename {
        request: RenameRequest,
        reply: tokio::sync::oneshot::Sender<ChangeOutcome>,
    },
}

/// A change to a protected file, as the channel authorized it.
#[derive(Debug)]
pub enum ProtectedRequest {
    Write(WriteRequest),
    Delete(DeleteRequest),
    Rename(RenameRequest),
}

/// The outcome the agent gets back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WriteOutcome {
    /// The user approved; `content` is what was written (the user's version when they changed it).
    Approved {
        modified: bool,
        content: String,
        notes: Vec<Value>,
    },
    Rejected {
        notes: Vec<Value>,
    },
    /// Not done, with why: not protected, outside this session, another request pending, a write failure.
    Refused {
        reason: String,
    },
}

/// What a delete or rename came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ChangeOutcome {
    /// The user approved, and it's done.
    Done,
    /// The user denied it.
    Denied,
    /// Not done, with why and what works instead: not protected, missing, a folder, the target exists, a failure.
    Refused { reason: String },
}

/// What `write`, `delete`, and `rename` answer: the outcome, or that the session ended before the user answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtectedResponse {
    Write(WriteOutcome),
    Delete(ChangeOutcome),
    Rename(ChangeOutcome),
    /// The session ended before the user answered.
    Ended,
}

/// Asks `session` for the change: the user answers in a client, as long as it takes. The agent's tool call holds the
/// connection meanwhile; when it closes, the caller drops this future, and the session closes the dialog.
pub async fn serve(
    daemon: &Daemon,
    session: crate::session::SessionKey,
    request: ProtectedRequest,
) -> ProtectedResponse {
    let Some(handle) = daemon.session(session) else {
        return ProtectedResponse::Ended;
    };
    // Wakes the session when the agent stops waiting, so it closes the dialog at once.
    let _wake = super::socket::WakeOnDrop(handle.tx.clone());
    match request {
        ProtectedRequest::Write(request) => {
            let (reply, outcome) = tokio::sync::oneshot::channel();
            let ask = ProtectedAsk::Write { request, reply };
            if handle.tx.send(SessionCmd::Protected(ask)).is_err() {
                return ProtectedResponse::Ended;
            }
            outcome.await.map_or(ProtectedResponse::Ended, ProtectedResponse::Write)
        }
        ProtectedRequest::Delete(request) => {
            let (reply, outcome) = tokio::sync::oneshot::channel();
            let ask = ProtectedAsk::Delete { request, reply };
            if handle.tx.send(SessionCmd::Protected(ask)).is_err() {
                return ProtectedResponse::Ended;
            }
            outcome
                .await
                .map_or(ProtectedResponse::Ended, ProtectedResponse::Delete)
        }
        ProtectedRequest::Rename(request) => {
            let (reply, outcome) = tokio::sync::oneshot::channel();
            let ask = ProtectedAsk::Rename { request, reply };
            if handle.tx.send(SessionCmd::Protected(ask)).is_err() {
                return ProtectedResponse::Ended;
            }
            outcome
                .await
                .map_or(ProtectedResponse::Ended, ProtectedResponse::Rename)
        }
    }
}

/// What the user decided, from a client's answer to the dialog: once (the write is one operation the daemon bounds),
/// or deny, each with the user's notes. A write changes what later sessions may do, so nothing broader is offered.
pub type WriteAnswer = super::answer::Answer<Once<WriteApproval>, Never, Never, Vec<Value>>;

/// An approved write: the user's own version, when they changed it, and their notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteApproval {
    pub content: Option<String>,
    pub notes: Vec<Value>,
}

/// Reads a client's `extension_ui_response` fields: `value` from a select (pi.nvim's JSON or a plain option),
/// `cancelled` or a missing value as a rejection.
pub fn read_answer(answer: &serde_json::Map<String, Value>) -> WriteAnswer {
    let approve = |content, notes| WriteAnswer::Narrowest(Once(WriteApproval { content, notes }));
    let Some(value) = answer.get("value").and_then(Value::as_str) else {
        return WriteAnswer::Deny(Vec::new());
    };
    match value {
        "Accept" => approve(None, Vec::new()),
        text => {
            let parsed: Option<serde_json::Map<String, Value>> = serde_json::from_str(text).ok();
            let notes = parsed
                .as_ref()
                .and_then(|p| p.get("notes"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            match parsed.as_ref().and_then(|p| p.get("result")).and_then(Value::as_str) {
                Some("Accepted") => approve(None, notes),
                Some("AcceptModified") => approve(
                    parsed
                        .as_ref()
                        .and_then(|p| p.get("content"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    notes,
                ),
                Some(_) | None => WriteAnswer::Deny(notes),
            }
        }
    }
}

/// The dialog record for a request, in pi's `extension_ui_request` format. The prompt says what the file controls, since
/// the diff is the user's only check: `write (protected): <path> (runs code in every session)`.
pub fn write_dialog(id: &str, path: &Path, controls: super::Controls, current: Option<&str>, content: &str) -> Value {
    let prompt = match controls.label() {
        Some(label) => format!("write (protected): {} ({label})", path.display()),
        None => format!("write (protected): {}", path.display()),
    };
    let title = json!({
        "prompt": prompt,
        "toolName": "write",
        "toolInput": {"path": path, "content": content},
        "current": current,
    });
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": title.to_string(),
        "options": ["Accept", "Reject"],
    })
}

/// A protected entry a delete or rename acts on, checked before the user is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The folder the change walks from without following symlinks.
    pub root: PathBuf,
    /// The entry as the file system names it: its folder resolved, its own name kept (a symlink stays the link).
    pub path: PathBuf,
    pub controls: super::Controls,
}

/// `path` with its folder resolved ([`super::canonical_target`]) and its last name kept, so a symlink names itself.
fn canonical_entry(path: &Path) -> Option<PathBuf> {
    let clean = super::lexical(path);
    let name = clean.file_name()?.to_os_string();
    Some(super::canonical_target(clean.parent()?).join(name))
}

fn checked(protected: &super::Protected, path: &Path) -> Option<Checked> {
    let path = canonical_entry(path)?;
    let file = protected.check(&path)?;
    Some(Checked {
        root: file.root,
        path,
        controls: file.controls,
    })
}

/// What a delete removes: a protected file or symlink that exists. Refusals say what works instead.
pub fn check_delete(protected: &super::Protected, requested: &Path) -> Result<Checked, String> {
    let Some(entry) = checked(protected, requested) else {
        return Err(format!(
            "{} isn't a protected file of this session; delete it with bash",
            requested.display()
        ));
    };
    match std::fs::symlink_metadata(&entry.path) {
        Ok(meta) if meta.file_type().is_file() || meta.file_type().is_symlink() => Ok(entry),
        Ok(_) => Err(format!(
            "{} is a folder; delete_protected_file removes one file or symlink at a time",
            requested.display()
        )),
        Err(_) => Err(format!("{} doesn't exist", requested.display())),
    }
}

/// What a rename moves, and where: both ends protected, the source a file or symlink that exists, the destination
/// missing. With one end unprotected, the refusal names the way out that works.
pub fn check_rename(protected: &super::Protected, from: &Path, to: &Path) -> Result<(Checked, Checked), String> {
    let (source, target) = match (checked(protected, from), checked(protected, to)) {
        (Some(source), Some(target)) => (source, target),
        (None, None) => {
            return Err(format!(
                "neither {} nor {} is a protected file of this session; rename it with bash",
                from.display(),
                to.display()
            ));
        }
        (None, Some(_)) => {
            return Err(format!(
                "only {} is protected: write it with the write tool, then remove {} with bash",
                to.display(),
                from.display()
            ));
        }
        (Some(_), None) => {
            return Err(format!(
                "only {} is protected: copy it to {} with bash, then delete it with delete_protected_file",
                from.display(),
                to.display()
            ));
        }
    };
    match std::fs::symlink_metadata(&source.path) {
        Ok(meta) if meta.file_type().is_file() || meta.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(format!(
                "{} is a folder; rename_protected_file moves one file or symlink at a time",
                from.display()
            ));
        }
        Err(_) => return Err(format!("{} doesn't exist", from.display())),
    }
    if std::fs::symlink_metadata(&target.path).is_ok() {
        return Err(format!("{} already exists; a rename never replaces it", to.display()));
    }
    Ok((source, target))
}

/// What the user decided about a delete or rename: once, or deny. Like a write, it changes what later sessions may do,
/// so nothing broader is offered.
pub type ChangeAnswer = super::answer::Answer<Once<()>, Never, Never, ()>;

const CHOOSE_BELOW: &str = super::block::CHOOSE_BELOW;
const DELETE: &str = "Delete";
const RENAME: &str = "Rename";
const DENY: &str = "Deny";

/// A client's answer to a delete or rename dialog: `None` asks again (the first option); a dismissal or an unknown value
/// denies.
pub fn read_change_answer(answer: &serde_json::Map<String, Value>) -> Option<ChangeAnswer> {
    match answer.get("value").and_then(Value::as_str) {
        Some(CHOOSE_BELOW) => None,
        Some(DELETE | RENAME) => Some(ChangeAnswer::Narrowest(Once(()))),
        Some(_) | None => Some(ChangeAnswer::Deny(())),
    }
}

/// What a delete removes, for its dialog: a file of some size, or a symlink to somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { bytes: u64 },
    Link { target: PathBuf },
}

impl Entry {
    /// The entry at `path`, not following a symlink there; `None` when it's missing or anything else (a folder).
    pub fn at(path: &Path) -> Option<Self> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Some(Entry::Link {
                target: std::fs::read_link(path).ok()?,
            });
        }
        kind.is_file().then_some(Entry::File { bytes: meta.len() })
    }

    fn describe(&self) -> String {
        match self {
            Entry::File { bytes: 1 } => "a file of 1 byte".into(),
            Entry::File { bytes } => format!("a file of {bytes} bytes"),
            Entry::Link { target } => format!("a symlink to {}", target.display()),
        }
    }
}

fn prompt(operation: &str, what: String, labels: &[Option<&'static str>]) -> String {
    let mut labels: Vec<&str> = labels.iter().flatten().copied().collect();
    labels.dedup();
    match labels.as_slice() {
        [] => format!("{operation} (protected): {what}"),
        labels => format!("{operation} (protected): {what} ({})", labels.join("; ")),
    }
}

/// A delete's dialog: `delete (protected): <path> (<what it controls>)`, then what the entry is.
pub fn delete_dialog(id: &str, path: &Path, controls: super::Controls, entry: Option<&Entry>) -> Value {
    let mut title = prompt("delete", path.display().to_string(), &[controls.label()]);
    if let Some(entry) = entry {
        title.push('\n');
        title.push_str(&entry.describe());
    }
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": title,
        "options": [CHOOSE_BELOW, DELETE, DENY],
    })
}

/// A rename's dialog: `rename (protected): <from> → <to> (<what it controls>)`. The destination's label comes first,
/// since it says what the file will control; the source's follows when it differs.
pub fn rename_dialog(
    id: &str,
    from: &Path,
    from_controls: super::Controls,
    to: &Path,
    to_controls: super::Controls,
) -> Value {
    let what = format!("{} → {}", from.display(), to.display());
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": prompt("rename", what, &[to_controls.label(), from_controls.label()]),
        "options": [CHOOSE_BELOW, RENAME, DENY],
    })
}

/// The permissions an approved write gives its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// An existing file keeps its own; a new one is `0644`.
    Keep,
    /// Exactly these (a Git blob's `0644` or `0755`).
    Set(u32),
}

/// Writes `content` to `path` (canonical, as approved) atomically, keeping an existing file's permissions, without
/// following any symlink at or below `root`.
pub fn write_approved(root: &Path, path: &Path, content: &str) -> std::io::Result<()> {
    write_file(root, path, content.as_bytes(), FileMode::Keep)
}

/// Writes `content` to `path` (canonical, as approved) atomically with `mode`, without following any symlink at or
/// below `root`.
///
/// The agent can change its checkout while the user decides (move a folder away, put a symlink in its place). So the
/// path is walked from `root` one folder at a time, each opened with `O_NOFOLLOW` (missing ones created, never
/// followed), and the temporary file and the rename happen relative to the last folder's descriptor. A path that no
/// longer leads where the user saw it fails here instead of writing elsewhere.
pub fn write_file(root: &Path, path: &Path, content: &[u8], mode: FileMode) -> std::io::Result<()> {
    use std::io::Write;

    use nix::{
        fcntl::{OFlag, openat, renameat},
        sys::stat::Mode,
    };

    let (name, dirs) = walk(root, path, Walk::Create)?.ok_or_else(|| refused("a folder vanished".into()))?;
    let dir = dirs.last().expect("the walk starts at the root");
    // The file itself must be a regular file (or missing): a symlink there could have been put in place while the user
    // decided, and would send the write elsewhere.
    let mode = match (regular_file(dir, &name, path)?, mode) {
        (_, FileMode::Set(bits)) => Mode::from_bits_truncate(bits as nix::sys::stat::mode_t),
        (Some(existing), FileMode::Keep) => existing,
        (None, FileMode::Keep) => Mode::from_bits_truncate(0o644),
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = format!(".{}.piton-{}-{n}", Path::new(&name).display(), std::process::id());
    let file = openat(
        dir.as_fd(),
        temporary.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        mode,
    )?;
    let mut file = std::fs::File::from(file);
    // `openat` applies the umask; set the exact bits the change names.
    let written = file
        .write_all(content)
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(u32::from(mode.bits())))
        })
        .and_then(|()| file.sync_all())
        .and_then(|()| renameat(dir.as_fd(), temporary.as_str(), dir.as_fd(), name.as_os_str()).map_err(Into::into));
    if written.is_err() {
        let _ = nix::unistd::unlinkat(dir.as_fd(), temporary.as_str(), nix::unistd::UnlinkatFlags::NoRemoveDir);
    }
    written
}

/// What [`remove_entry`] may remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removable {
    /// Only a regular file: a symlink there refuses (a source update, which writes only what Git's blobs say).
    File,
    /// A regular file or a symlink, the link itself (an approved delete).
    FileOrLink,
}

/// Whether [`remove_entry`] removed something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    Removed,
    /// Nothing was there.
    Missing,
}

/// Removes the entry at `path` (canonical, as approved) that `accept` allows, then each folder it leaves empty, up to
/// `root`, without following any symlink. A folder refuses.
pub fn remove_entry(root: &Path, path: &Path, accept: Removable) -> std::io::Result<Removed> {
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let Some((name, dirs)) = walk(root, path, Walk::Existing)? else {
        return Ok(Removed::Missing);
    };
    let dir = dirs.last().expect("the walk starts at the root");
    let present = match accept {
        Removable::File => regular_file(dir, &name, path)?.is_some(),
        Removable::FileOrLink => file_or_link(dir, &name, path)?,
    };
    if !present {
        return Ok(Removed::Missing);
    }
    unlinkat(dir.as_fd(), name.as_os_str(), UnlinkatFlags::NoRemoveDir)?;
    remove_emptied(root, path, &dirs)?;
    Ok(Removed::Removed)
}

/// Removes the folders on the way from `root` to `path` that are left empty, innermost first; the first one that isn't
/// empty stops it. `dirs` are the walk's folders.
fn remove_emptied(root: &Path, path: &Path, dirs: &[OwnedFd]) -> std::io::Result<()> {
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let relative = path
        .strip_prefix(root)
        .map_err(|_| refused("outside the root".into()))?;
    let folders: Vec<&std::ffi::OsStr> = relative.parent().into_iter().flat_map(Path::iter).collect();
    for (folder, parent) in folders.iter().zip(dirs).rev() {
        match unlinkat(parent.as_fd(), *folder, UnlinkatFlags::RemoveDir) {
            Ok(()) => {}
            Err(nix::errno::Errno::ENOTEMPTY | nix::errno::Errno::EEXIST) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Renames the file or symlink at `from` (canonical, below `from_root`) to `to` (below `to_root`), which must not exist,
/// without following any symlink on either path. The destination's missing folders are created, as for a write; the
/// source's folders left empty are removed.
pub fn rename_entry(from_root: &Path, from: &Path, to_root: &Path, to: &Path) -> std::io::Result<()> {
    let (from_name, from_dirs) = walk(from_root, from, Walk::Existing)?
        .ok_or_else(|| refused(format!("{} no longer exists", from.display())))?;
    let from_dir = from_dirs.last().expect("the walk starts at the root");
    if !file_or_link(from_dir, &from_name, from)? {
        return Err(refused(format!("{} no longer exists", from.display())));
    }
    let (to_name, to_dirs) = walk(to_root, to, Walk::Create)?.ok_or_else(|| refused("a folder vanished".into()))?;
    let to_dir = to_dirs.last().expect("the walk starts at the root");
    match rename_exclusive(from_dir, &from_name, to_dir, &to_name) {
        Ok(()) => {}
        Err(RenameFailed::Exists) => {
            return Err(refused(format!("{} already exists; it isn't replaced", to.display())));
        }
        Err(RenameFailed::OtherVolume) => {
            return Err(refused(format!(
                "{} and {} are on different volumes; write the new file and delete the old one instead",
                from.display(),
                to.display()
            )));
        }
        Err(RenameFailed::Os(e)) => return Err(e.into()),
    }
    remove_emptied(from_root, from, &from_dirs)
}

/// Why an exclusive rename failed.
enum RenameFailed {
    /// The destination exists.
    Exists,
    /// The two folders are on different volumes.
    OtherVolume,
    Os(nix::errno::Errno),
}

/// `renameat` that never replaces an existing entry.
#[allow(unsafe_code)]
fn rename_exclusive(
    from_dir: &OwnedFd,
    from: &std::ffi::OsStr,
    to_dir: &OwnedFd,
    to: &std::ffi::OsStr,
) -> Result<(), RenameFailed> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    let c = |name: &std::ffi::OsStr| {
        std::ffi::CString::new(name.as_bytes()).map_err(|_| RenameFailed::Os(nix::errno::Errno::EINVAL))
    };
    let (from, to) = (c(from)?, c(to)?);
    // SAFETY: both descriptors are open folders, and both names are NUL-terminated strings that outlive the call, which
    // only reads them.
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::renameatx_np(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::renameat2(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let errno = nix::errno::Errno::last();
    Err(if errno == nix::errno::Errno::EEXIST {
        RenameFailed::Exists
    } else if errno == nix::errno::Errno::EXDEV {
        RenameFailed::OtherVolume
    } else {
        RenameFailed::Os(errno)
    })
}

use std::os::fd::{AsFd, OwnedFd};

fn refused(why: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, why)
}

#[derive(Debug, Clone, Copy)]
enum Walk {
    /// Missing folders are created.
    Create,
    /// A missing folder ends the walk with `None`.
    Existing,
}

/// The file's name and the folders from `root` to it, each opened with `O_NOFOLLOW`: `dirs[0]` is `root`.
fn walk(root: &Path, path: &Path, mode: Walk) -> std::io::Result<Option<(std::ffi::OsString, Vec<OwnedFd>)>> {
    use nix::{
        fcntl::{OFlag, open, openat},
        sys::stat::{Mode, mkdirat},
    };

    let relative = path
        .strip_prefix(root)
        .map_err(|_| refused(format!("{} is outside {}", path.display(), root.display())))?;
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => parts.push(name),
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir
            | std::path::Component::ParentDir => {
                return Err(refused(format!("{} isn't a plain path", path.display())));
            }
        }
    }
    let Some((name, folders)) = parts.split_last() else {
        return Err(refused(format!("{} names no file", path.display())));
    };
    let directory = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut dirs: Vec<OwnedFd> = vec![open(root, directory, Mode::empty())?];
    for folder in folders {
        let dir = dirs.last().expect("the walk starts at the root");
        match mode {
            Walk::Create => match mkdirat(dir.as_fd(), *folder, Mode::from_bits_truncate(0o755)) {
                Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
                Err(e) => return Err(e.into()),
            },
            Walk::Existing => {}
        }
        // ELOOP or ENOTDIR here means a symlink or a file took the folder's place.
        let next = match (openat(dir.as_fd(), *folder, directory, Mode::empty()), mode) {
            (Ok(next), _) => next,
            (Err(nix::errno::Errno::ENOENT), Walk::Existing) => return Ok(None),
            (Err(e), Walk::Create | Walk::Existing) => {
                return Err(refused(format!(
                    "{} no longer leads to the approved file ({e})",
                    Path::new(folder).display()
                )));
            }
        };
        dirs.push(next);
    }
    Ok(Some((name.to_os_string(), dirs)))
}

/// Whether a regular file or a symlink is there (`false` when nothing is); anything else (a folder) refuses.
fn file_or_link(dir: &OwnedFd, name: &std::ffi::OsStr, path: &Path) -> std::io::Result<bool> {
    use nix::{fcntl::AtFlags, sys::stat::fstatat};
    match fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) if matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFLNK) => Ok(true),
        Ok(_) => Err(refused(format!(
            "{} isn't a file or a symlink (a folder?); change it by hand",
            path.display()
        ))),
        Err(nix::errno::Errno::ENOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// An existing regular file's permissions, or `None` when it's missing; anything else (a symlink, a folder) refuses.
fn regular_file(dir: &OwnedFd, name: &std::ffi::OsStr, path: &Path) -> std::io::Result<Option<nix::sys::stat::Mode>> {
    use nix::{
        fcntl::AtFlags,
        sys::stat::{Mode, fstatat},
    };
    match fstatat(dir.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFREG => {
            Ok(Some(Mode::from_bits_truncate(stat.st_mode & 0o7777)))
        }
        Ok(_) => Err(refused(format!(
            "{} isn't a regular file (a symlink or folder); change it by hand",
            path.display()
        ))),
        Err(nix::errno::Errno::ENOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn reads_pi_nvim_and_plain_answers() {
        let approved = |content: Option<&str>, notes| {
            WriteAnswer::Narrowest(Once(WriteApproval {
                content: content.map(str::to_string),
                notes,
            }))
        };
        assert_eq!(read_answer(&answer(json!({"value": "Accept"}))), approved(None, vec![]));
        assert_eq!(
            read_answer(&answer(json!({"value": "Reject"}))),
            WriteAnswer::Deny(vec![])
        );
        assert_eq!(
            read_answer(&answer(json!({"cancelled": true}))),
            WriteAnswer::Deny(vec![])
        );
        let note = json!({"path": "p", "note": "why"});
        assert_eq!(
            read_answer(&answer(json!({
                "value": json!({"result": "AcceptModified", "content": "mine", "notes": [note]}).to_string()
            }))),
            approved(Some("mine"), vec![note.clone()])
        );
        assert_eq!(
            read_answer(&answer(json!({
                "value": json!({"result": "Rejected", "notes": [note]}).to_string()
            }))),
            WriteAnswer::Deny(vec![note])
        );
    }

    #[test]
    fn the_dialog_is_pi_nvims_diff_format_with_the_current_content() {
        use super::super::Controls;
        let d = write_dialog(
            "piton:w:1",
            Path::new("/r/.piton/config.toml"),
            Controls::Other,
            Some("old"),
            "{}",
        );
        assert_eq!(d["type"], "extension_ui_request");
        assert_eq!(d["method"], "select");
        let title: Value = serde_json::from_str(d["title"].as_str().unwrap()).unwrap();
        assert_eq!(title["toolName"], "write");
        assert_eq!(title["toolInput"]["path"], "/r/.piton/config.toml");
        assert_eq!(title["toolInput"]["content"], "{}");
        assert_eq!(title["current"], "old");
        assert_eq!(title["prompt"], "write (protected): /r/.piton/config.toml");
        let new = write_dialog("piton:w:2", Path::new("/r/.pi/x"), Controls::Other, None, "y");
        let title: Value = serde_json::from_str(new["title"].as_str().unwrap()).unwrap();
        assert_eq!(title["current"], Value::Null);
    }

    /// The prompt says what the file controls; nothing else in the dialog changes.
    #[test]
    fn the_prompt_names_what_the_file_controls() {
        use super::super::Controls;
        let prompt = |controls| {
            let d = write_dialog("piton:w:1", Path::new("/g/config.toml"), controls, None, "");
            let title: Value = serde_json::from_str(d["title"].as_str().unwrap()).unwrap();
            title["prompt"].as_str().unwrap().to_string()
        };
        assert_eq!(
            prompt(Controls::Sandbox),
            "write (protected): /g/config.toml (can loosen the sandbox)"
        );
        assert!(prompt(Controls::Code).ends_with(" (runs code in every session)"));
        assert!(prompt(Controls::Secrets).ends_with(" (decides where secrets go)"));
        assert_eq!(prompt(Controls::Other), "write (protected): /g/config.toml");
    }

    /// What the daemon writes for an approved source update: a Git blob's mode, and removal that clears emptied folders
    /// without following a symlink.
    #[test]
    fn writes_with_a_mode_and_removes_emptied_folders() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let hook = root.join(".pi/bin/run");
        write_file(&root, &hook, b"#!/bin/sh\n", FileMode::Set(0o755)).unwrap();
        assert_eq!(std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777, 0o755);
        write_file(&root, &hook, b"x", FileMode::Set(0o644)).unwrap();
        assert_eq!(std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::write(root.join(".pi/keep"), "k").unwrap();

        assert_eq!(remove_entry(&root, &hook, Removable::File).unwrap(), Removed::Removed);
        assert!(!root.join(".pi/bin").exists(), "the emptied folder goes");
        assert!(root.join(".pi/keep").exists(), "a folder with other files stays");
        assert_eq!(remove_entry(&root, &hook, Removable::File).unwrap(), Removed::Missing);
        assert_eq!(
            remove_entry(&root, &root.join("missing/x"), Removable::File).unwrap(),
            Removed::Missing
        );

        // Through a symlinked folder: refused, nothing removed there.
        let elsewhere = root.join("../elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let elsewhere = std::fs::canonicalize(elsewhere).unwrap();
        std::fs::write(elsewhere.join("x"), "theirs").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();
        assert!(remove_entry(&root, &root.join("link/x"), Removable::FileOrLink).is_err());
        assert!(elsewhere.join("x").exists());

        // A symlink at the path: a source update refuses it, as before; an approved delete removes the link alone.
        std::fs::write(elsewhere.join("target"), "t").unwrap();
        let link = root.join(".pi/linked");
        std::os::unix::fs::symlink(elsewhere.join("target"), &link).unwrap();
        assert!(remove_entry(&root, &link, Removable::File).is_err());
        assert_eq!(
            remove_entry(&root, &link, Removable::FileOrLink).unwrap(),
            Removed::Removed
        );
        assert!(link.symlink_metadata().is_err(), "the link is gone");
        assert!(elsewhere.join("target").exists(), "its target stays");
        // A folder refuses.
        std::fs::create_dir_all(root.join(".pi/dir/inner")).unwrap();
        assert!(remove_entry(&root, &root.join(".pi/dir/inner"), Removable::FileOrLink).is_err());
    }

    /// A rename never replaces, creates the destination's folders, removes the source's emptied ones, and follows no
    /// symlink on either side.
    #[test]
    fn renames_without_replacing_or_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let root = base.join("repo");
        let other = base.join("level");
        std::fs::create_dir_all(root.join(".pi/notes")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(root.join(".pi/notes/a.ts"), "a").unwrap();

        // Into a missing folder: created; the emptied source folder goes.
        let to = root.join(".pi/extensions/a/index.ts");
        rename_entry(&root, &root.join(".pi/notes/a.ts"), &root, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "a");
        assert!(!root.join(".pi/notes").exists());

        // Across roots (a level outside the checkout).
        let moved = other.join("rules/a.ts");
        rename_entry(&root, &to, &other, &moved).unwrap();
        assert!(moved.exists() && !to.exists());

        // An existing target isn't replaced.
        std::fs::write(other.join("b.ts"), "b").unwrap();
        let refused = rename_entry(&other, &moved, &other, &other.join("b.ts")).unwrap_err();
        assert!(refused.to_string().contains("already exists"), "{refused}");
        assert_eq!(std::fs::read_to_string(other.join("b.ts")).unwrap(), "b");
        assert!(moved.exists());

        // A symlink: the link moves, not its target.
        std::os::unix::fs::symlink(&moved, other.join("link")).unwrap();
        rename_entry(&other, &other.join("link"), &other, &other.join("link2")).unwrap();
        assert!(other.join("link2").symlink_metadata().unwrap().file_type().is_symlink());
        assert!(moved.exists());

        // A symlinked folder on the destination's path: refused, nothing written through it.
        let elsewhere = base.join("home");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // The cross-root move emptied `.pi`, which went with it.
        std::fs::create_dir_all(root.join(".pi")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".pi/out")).unwrap();
        assert!(rename_entry(&other, &moved, &root, &root.join(".pi/out/a.ts")).is_err());
        assert!(!elsewhere.join("a.ts").exists());
        assert!(moved.exists());
        // Nor on the source's.
        std::fs::write(elsewhere.join("theirs"), "t").unwrap();
        assert!(rename_entry(&root, &root.join(".pi/out/theirs"), &root, &root.join(".pi/mine")).is_err());
        assert!(elsewhere.join("theirs").exists());
    }

    /// The checks before a delete or rename is asked: protected entries only, each refusal naming what works instead.
    #[test]
    fn checks_deletes_and_renames_before_asking() {
        use super::super::{Protected, ProtectedFacts};
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".pi/extensions/x")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join(".pi/settings.json"), "{}").unwrap();
        std::fs::write(repo.join(".pi/old.json"), "{}").unwrap();
        std::fs::write(repo.join("src/a.ts"), "a").unwrap();
        std::os::unix::fs::symlink(repo.join("src/a.ts"), repo.join(".pi/link")).unwrap();
        let protected = Protected::new(&ProtectedFacts {
            source: &repo,
            checkout: &repo,
            levels: &[],
            agent_dirs: &[],
            fnox_files: &[],
            global: &repo.join("global"),
            home: &repo.join("home"),
            excluded: &[],
        });
        let delete = |p: &str| check_delete(&protected, &repo.join(p));
        assert_eq!(
            delete(".pi/settings.json").unwrap().path,
            repo.join(".pi/settings.json")
        );
        assert_eq!(
            delete(".pi/link").unwrap().path,
            repo.join(".pi/link"),
            "the link, not its target"
        );
        assert!(delete("src/a.ts").unwrap_err().contains("with bash"));
        assert!(delete(".pi/extensions/x").unwrap_err().contains("is a folder"));
        assert!(delete(".pi/missing").unwrap_err().contains("doesn't exist"));

        let rename = |from: &str, to: &str| check_rename(&protected, &repo.join(from), &repo.join(to));
        let (from, to) = rename(".pi/old.json", ".pi/extensions/new.json").unwrap();
        assert_eq!(
            (from.path, to.path),
            (repo.join(".pi/old.json"), repo.join(".pi/extensions/new.json"))
        );
        assert!(
            rename(".pi/old.json", ".pi/settings.json")
                .unwrap_err()
                .contains("already exists")
        );
        assert!(rename(".pi/missing", ".pi/new").unwrap_err().contains("doesn't exist"));
        assert!(rename(".pi/extensions/x", ".pi/y").unwrap_err().contains("is a folder"));
        assert!(
            rename("src/a.ts", "src/b.ts")
                .unwrap_err()
                .contains("rename it with bash")
        );
        let into = rename("src/a.ts", ".pi/a.ts").unwrap_err();
        assert!(into.contains("write tool") && into.contains("with bash"), "{into}");
        let out = rename(".pi/old.json", "src/old.json").unwrap_err();
        assert!(
            out.contains("copy it") && out.contains("delete_protected_file"),
            "{out}"
        );
    }

    /// Delete and rename dialogs say what happens and what the file controls, the destination's first.
    #[test]
    fn delete_and_rename_dialogs() {
        use super::super::Controls;
        let d = delete_dialog(
            "piton:delete:1",
            Path::new("/w/.pi/settings.json"),
            Controls::Code,
            Some(&Entry::File { bytes: 12 }),
        );
        assert_eq!(d["method"], "select");
        assert_eq!(
            d["title"],
            "delete (protected): /w/.pi/settings.json (runs code in every session)\na file of 12 bytes"
        );
        assert_eq!(d["options"], json!([CHOOSE_BELOW, "Delete", "Deny"]));
        let title = |from, to| {
            rename_dialog("piton:rename:2", Path::new("/w/.pi/a"), from, Path::new("/w/.pi/b"), to)["title"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            title(Controls::Other, Controls::Code),
            "rename (protected): /w/.pi/a → /w/.pi/b (runs code in every session)"
        );
        assert_eq!(
            title(Controls::Code, Controls::Other),
            "rename (protected): /w/.pi/a → /w/.pi/b (runs code in every session)",
            "the source's label when only it has one"
        );
        assert_eq!(
            title(Controls::Secrets, Controls::Code),
            "rename (protected): /w/.pi/a → /w/.pi/b (runs code in every session; decides where secrets go)"
        );
        assert_eq!(
            title(Controls::Other, Controls::Other),
            "rename (protected): /w/.pi/a → /w/.pi/b"
        );
        let answer = |v: Value| read_change_answer(v.as_object().unwrap());
        assert_eq!(answer(json!({"value": CHOOSE_BELOW})), None);
        assert_eq!(
            answer(json!({"value": "Delete"})),
            Some(ChangeAnswer::Narrowest(Once(())))
        );
        assert_eq!(
            answer(json!({"value": "Rename"})),
            Some(ChangeAnswer::Narrowest(Once(())))
        );
        assert_eq!(answer(json!({"value": "Deny"})), Some(ChangeAnswer::Deny(())));
        assert_eq!(answer(json!({"cancelled": true})), Some(ChangeAnswer::Deny(())));
    }

    #[test]
    fn writes_atomically_creating_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join(".piton/config.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "old").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_approved(&root, &file, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // A new file in new folders.
        let extension = root.join("apps/.pi/extensions/x.ts");
        write_approved(&root, &extension, "x").unwrap();
        assert_eq!(std::fs::read_to_string(&extension).unwrap(), "x");
        // Nothing outside the root.
        assert!(write_approved(&root, Path::new("/tmp/elsewhere"), "x").is_err());
    }

    /// The agent swaps a folder on the approved path for a symlink while the user decides: the write refuses instead
    /// of following it out of the checkout.
    #[test]
    fn never_follows_a_symlink_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let root = base.join("repo");
        let elsewhere = base.join("home");
        std::fs::create_dir_all(root.join("apps")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let approved = root.join("apps/.pi/settings.json");
        std::fs::rename(root.join("apps"), root.join("apps.bak")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("apps")).unwrap();
        let refused = write_approved(&root, &approved, "{}").unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(!elsewhere.join(".pi").exists(), "nothing written through the link");

        // Nor a symlink at the file itself: refused, and neither the link nor its target changes.
        std::fs::remove_file(root.join("apps")).unwrap();
        std::fs::create_dir_all(root.join(".piton")).unwrap();
        let target = elsewhere.join("config.toml");
        std::fs::write(&target, "theirs").unwrap();
        std::os::unix::fs::symlink(&target, root.join(".piton/config.toml")).unwrap();
        let refused = write_approved(&root, &root.join(".piton/config.toml"), "mine").unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "theirs");
        assert!(
            root.join(".piton/config.toml")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
