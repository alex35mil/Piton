//! A protected write an agent asks for: the daemon asks the user (a dialog in every client), then writes the file
//! itself. The agent can ask (over `agent.sock`, see agent.rs) but never answer: answers come only from clients.
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
pub struct Request {
    /// Absolute path (the agent's view of it; resolved and checked by the daemon).
    pub path: PathBuf,
    /// The whole new content.
    pub content: String,
}

/// The outcome the agent gets back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
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
pub enum Decision {
    Approve {
        /// The user's own version, when they changed it.
        content: Option<String>,
        notes: Vec<Value>,
    },
    Reject {
        notes: Vec<Value>,
    },
}

impl Decision {
    /// Reads a client's `extension_ui_response` fields: `value` from a select (pi.nvim's JSON or a plain option),
    /// `cancelled` or a missing value as a rejection.
    pub fn from_answer(answer: &serde_json::Map<String, Value>) -> Decision {
        let Some(value) = answer.get("value").and_then(Value::as_str) else {
            return Decision::Reject { notes: Vec::new() };
        };
        match value {
            "Accept" => Decision::Approve {
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
                    Some("Accepted") => Decision::Approve { content: None, notes },
                    Some("AcceptModified") => Decision::Approve {
                        content: parsed
                            .as_ref()
                            .and_then(|p| p.get("content"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        notes,
                    },
                    Some(_) | None => Decision::Reject { notes },
                }
            }
        }
    }
}

/// The dialog record for a request, in pi's `extension_ui_request` format.
pub fn dialog(id: &str, path: &Path, current: Option<&str>, content: &str) -> Value {
    let title = json!({
        "prompt": format!("write (protected): {}", path.display()),
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

/// Writes `content` to `path` (canonical, as approved) atomically, keeping an existing file's permissions, without
/// following any symlink at or below `root`.
///
/// The agent can change its checkout while the user decides (move a folder away, put a symlink in its place). So the
/// path is walked from `root` one folder at a time, each opened with `O_NOFOLLOW` (missing ones created, never
/// followed), and the temporary file and the rename happen relative to the last folder's descriptor. A path that no
/// longer leads where the user saw it fails here instead of writing elsewhere.
pub fn write(root: &Path, path: &Path, content: &str) -> std::io::Result<()> {
    use nix::fcntl::{AtFlags, OFlag, open, openat, renameat};
    use nix::sys::stat::{Mode, fstatat, mkdirat};
    use std::io::Write;
    use std::os::fd::{AsFd, OwnedFd};

    let refused = |why: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why);
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
    let mut dir: OwnedFd = open(root, directory, Mode::empty())?;
    for folder in folders {
        match mkdirat(dir.as_fd(), *folder, Mode::from_bits_truncate(0o755)) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
            Err(e) => return Err(e.into()),
        }
        // ELOOP or ENOTDIR here means a symlink or a file took the folder's place.
        dir = openat(dir.as_fd(), *folder, directory, Mode::empty()).map_err(|e| {
            refused(format!(
                "{} no longer leads to the approved file ({e})",
                Path::new(folder).display()
            ))
        })?;
    }
    // The file itself must be a regular file (or missing): a symlink there could have been put in place while the user
    // decided, and would send the write elsewhere. Its mode carries over.
    let mode = match fstatat(dir.as_fd(), *name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFREG => Mode::from_bits_truncate(stat.st_mode & 0o7777),
        Ok(_) => {
            return Err(refused(format!(
                "{} isn't a regular file (a symlink or folder); change it by hand",
                path.display()
            )));
        }
        Err(nix::errno::Errno::ENOENT) => Mode::from_bits_truncate(0o644),
        Err(e) => return Err(e.into()),
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = format!(".{}.piton-{}-{n}", Path::new(name).display(), std::process::id());
    let file = openat(
        dir.as_fd(),
        temporary.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        mode,
    )?;
    let mut file = std::fs::File::from(file);
    let written = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| renameat(dir.as_fd(), temporary.as_str(), dir.as_fd(), *name).map_err(Into::into));
    if written.is_err() {
        let _ = nix::unistd::unlinkat(dir.as_fd(), temporary.as_str(), nix::unistd::UnlinkatFlags::NoRemoveDir);
    }
    written
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
            Decision::from_answer(&answer(json!({"value": "Accept"}))),
            Decision::Approve {
                content: None,
                notes: vec![]
            }
        );
        assert_eq!(
            Decision::from_answer(&answer(json!({"value": "Reject"}))),
            Decision::Reject { notes: vec![] }
        );
        assert_eq!(
            Decision::from_answer(&answer(json!({"cancelled": true}))),
            Decision::Reject { notes: vec![] }
        );
        let note = json!({"path": "p", "note": "why"});
        assert_eq!(
            Decision::from_answer(&answer(json!({
                "value": json!({"result": "AcceptModified", "content": "mine", "notes": [note]}).to_string()
            }))),
            Decision::Approve {
                content: Some("mine".into()),
                notes: vec![note.clone()]
            }
        );
        assert_eq!(
            Decision::from_answer(&answer(json!({
                "value": json!({"result": "Rejected", "notes": [note]}).to_string()
            }))),
            Decision::Reject { notes: vec![note] }
        );
    }

    #[test]
    fn the_dialog_is_pi_nvims_diff_format_with_the_current_content() {
        let d = dialog("piton:w:1", Path::new("/r/.piton/config.toml"), Some("old"), "{}");
        assert_eq!(d["type"], "extension_ui_request");
        assert_eq!(d["method"], "select");
        let title: Value = serde_json::from_str(d["title"].as_str().unwrap()).unwrap();
        assert_eq!(title["toolName"], "write");
        assert_eq!(title["toolInput"]["path"], "/r/.piton/config.toml");
        assert_eq!(title["toolInput"]["content"], "{}");
        assert_eq!(title["current"], "old");
        let new = dialog("piton:w:2", Path::new("/r/.pi/x"), None, "y");
        let title: Value = serde_json::from_str(new["title"].as_str().unwrap()).unwrap();
        assert_eq!(title["current"], Value::Null);
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
        write(&root, &file, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // A new file in new folders.
        let skill = root.join("apps/.agents/skills/x/SKILL.md");
        write(&root, &skill, "skill").unwrap();
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), "skill");
        // Nothing outside the root.
        assert!(write(&root, Path::new("/tmp/elsewhere"), "x").is_err());
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
        let approved = root.join("apps/.agents/skills/x/SKILL.md");
        std::fs::rename(root.join("apps"), root.join("apps.bak")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("apps")).unwrap();
        let refused = write(&root, &approved, "allowed-tools: [\"bash(*)\"]").unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied, "{refused}");
        assert!(!elsewhere.join(".agents").exists(), "nothing written through the link");

        // Nor a symlink at the file itself: refused, and neither the link nor its target changes.
        std::fs::remove_file(root.join("apps")).unwrap();
        std::fs::create_dir_all(root.join(".piton")).unwrap();
        let target = elsewhere.join("config.toml");
        std::fs::write(&target, "theirs").unwrap();
        std::os::unix::fs::symlink(&target, root.join(".piton/config.toml")).unwrap();
        let refused = write(&root, &root.join(".piton/config.toml"), "mine").unwrap_err();
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
