//! A protected write an agent asks for: the daemon asks the user (a dialog in every client), then writes the file
//! itself. The agent can ask (over `agent.sock`, see `socket.rs`) but never answer: answers come only from clients.
//!
//! The dialog is pi's file review format, so pi.nvim opens its diff view and the apps their review sheet: a `select`
//! whose title is `{"prompt", "toolName", "toolInput"}` with `toolName` `write` and `toolInput {path, content}`, plus
//! `current` (the file's content now, null for a new file) so a client that can't read the file shows a diff. pi.nvim
//! answers `"Reject"` or a JSON `{"result": "Accepted" | "AcceptModified" | "Rejected", "content"?, "notes"?}`; on
//! acceptance it has already written the file itself (its diff view writes on accept), which the daemon then confirms
//! by writing the same bytes again. The daemon's write, not the client's, is the one that counts: a client without
//! write access (the phone) answers the same way.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A request from the agent.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteRequest {
    /// Absolute path (the agent's view of it; resolved and checked by the daemon).
    pub path: PathBuf,
    /// The whole new content.
    pub content: String,
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

/// What the user decided, from a client's answer to the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteDecision {
    Approve {
        /// The user's own version, when they changed it.
        content: Option<String>,
        notes: Vec<Value>,
    },
    Reject {
        notes: Vec<Value>,
    },
}

impl WriteDecision {
    /// Reads a client's `extension_ui_response` fields: `value` from a select (pi.nvim's JSON or a plain option),
    /// `cancelled` or a missing value as a rejection.
    pub fn from_answer(answer: &serde_json::Map<String, Value>) -> WriteDecision {
        let Some(value) = answer.get("value").and_then(Value::as_str) else {
            return WriteDecision::Reject { notes: Vec::new() };
        };
        match value {
            "Accept" => WriteDecision::Approve {
                content: None,
                notes: Vec::new(),
            },
            text => {
                let parsed: Option<serde_json::Map<String, Value>> = serde_json::from_str(text).ok();
                let notes = parsed
                    .as_ref()
                    .and_then(|p| p.get("notes"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                match parsed.as_ref().and_then(|p| p.get("result")).and_then(Value::as_str) {
                    Some("Accepted") => WriteDecision::Approve { content: None, notes },
                    Some("AcceptModified") => WriteDecision::Approve {
                        content: parsed
                            .as_ref()
                            .and_then(|p| p.get("content"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        notes,
                    },
                    Some(_) | None => WriteDecision::Reject { notes },
                }
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

/// Removes the regular file at `path` (canonical, as approved), then each folder it leaves empty, up to `root`,
/// without following any symlink. A file already missing is fine.
pub fn remove_file(root: &Path, path: &Path) -> std::io::Result<()> {
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let Some((name, dirs)) = walk(root, path, Walk::Existing)? else {
        return Ok(());
    };
    let dir = dirs.last().expect("the walk starts at the root");
    if regular_file(dir, &name, path)?.is_none() {
        return Ok(());
    }
    unlinkat(dir.as_fd(), name.as_os_str(), UnlinkatFlags::NoRemoveDir)?;
    // Folders left empty, innermost first; the first one that isn't empty stops it.
    let relative = path
        .strip_prefix(root)
        .map_err(|_| refused("outside the root".into()))?;
    let folders: Vec<&std::ffi::OsStr> = relative.parent().into_iter().flat_map(Path::iter).collect();
    for (folder, parent) in folders.iter().zip(&dirs).rev() {
        match unlinkat(parent.as_fd(), *folder, UnlinkatFlags::RemoveDir) {
            Ok(()) => {}
            Err(nix::errno::Errno::ENOTEMPTY | nix::errno::Errno::EEXIST) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
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
        assert_eq!(
            WriteDecision::from_answer(&answer(json!({"value": "Accept"}))),
            WriteDecision::Approve {
                content: None,
                notes: vec![]
            }
        );
        assert_eq!(
            WriteDecision::from_answer(&answer(json!({"value": "Reject"}))),
            WriteDecision::Reject { notes: vec![] }
        );
        assert_eq!(
            WriteDecision::from_answer(&answer(json!({"cancelled": true}))),
            WriteDecision::Reject { notes: vec![] }
        );
        let note = json!({"path": "p", "note": "why"});
        assert_eq!(
            WriteDecision::from_answer(&answer(json!({
                "value": json!({"result": "AcceptModified", "content": "mine", "notes": [note]}).to_string()
            }))),
            WriteDecision::Approve {
                content: Some("mine".into()),
                notes: vec![note.clone()]
            }
        );
        assert_eq!(
            WriteDecision::from_answer(&answer(json!({
                "value": json!({"result": "Rejected", "notes": [note]}).to_string()
            }))),
            WriteDecision::Reject { notes: vec![note] }
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

        remove_file(&root, &hook).unwrap();
        assert!(!root.join(".pi/bin").exists(), "the emptied folder goes");
        assert!(root.join(".pi/keep").exists(), "a folder with other files stays");
        remove_file(&root, &hook).unwrap();
        remove_file(&root, &root.join("missing/x")).unwrap();

        // Through a symlinked folder: refused, nothing removed there.
        let elsewhere = root.join("../elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let elsewhere = std::fs::canonicalize(elsewhere).unwrap();
        std::fs::write(elsewhere.join("x"), "theirs").unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();
        assert!(remove_file(&root, &root.join("link/x")).is_err());
        assert!(elsewhere.join("x").exists());
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
