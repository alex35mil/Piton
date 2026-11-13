//! A protected write an agent asks for: the daemon asks the user (a dialog in every client), then writes the file
//! itself. The agent can ask (over `agent.sock`, see agent.rs) but never answer: answers come only from clients.
//!
//! The dialog is pi's file review format, so pi.nvim opens its diff view and the apps their review sheet: a `select`
//! whose title is `{"prompt", "toolName", "toolInput"}` with `toolName` `write` and `toolInput {path, content}`. pi.nvim
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
    /// Written as asked, without asking (skill instructions with unchanged `allowed-tools`).
    Written,
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
pub fn dialog(id: &str, path: &Path, content: &str) -> Value {
    let title = json!({
        "prompt": format!("write (protected): {}", path.display()),
        "toolName": "write",
        "toolInput": {"path": path, "content": content},
    });
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": title.to_string(),
        "options": ["Accept", "Reject"],
    })
}

/// Writes `content` to `path` the way an editor saves: through a symlink to its target, atomically (a temporary file
/// in the same folder, then a rename), keeping the file's permissions.
pub fn write(path: &Path, content: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = dir.join(format!(".{name}.piton-{}-{n}", std::process::id()));
    std::fs::write(&temporary, content)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&temporary, meta.permissions());
    }
    std::fs::rename(&temporary, &target).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Whether a skill's `SKILL.md` change leaves its `allowed-tools` as they were (then it needs no approval: the
/// instructions changed, not what the skill may do).
pub fn same_allowed_tools(before: &str, after: &str) -> bool {
    allowed_tools(before) == allowed_tools(after)
}

/// The `allowed-tools` entries of a `SKILL.md`'s frontmatter, in order; empty when it has none.
fn allowed_tools(text: &str) -> Vec<String> {
    let Some(rest) = text.strip_prefix("---\n") else {
        return Vec::new();
    };
    let Some(end) = rest.find("\n---") else {
        return Vec::new();
    };
    let mut tools = Vec::new();
    let mut inside = false;
    for line in rest[..end].lines() {
        if let Some(value) = line.strip_prefix("allowed-tools:") {
            inside = true;
            // An inline list: `allowed-tools: [a, b]` or `allowed-tools: a b`.
            let inline = value.trim().trim_start_matches('[').trim_end_matches(']');
            tools.extend(
                inline
                    .split([',', ' '])
                    .map(|t| t.trim().trim_matches('"').trim_matches('\''))
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
            );
            continue;
        }
        if inside {
            match line.trim_start().strip_prefix("- ") {
                Some(item) if line.starts_with(' ') || line.starts_with('-') => {
                    tools.push(item.trim().trim_matches('"').trim_matches('\'').to_string());
                }
                Some(_) | None => inside = false,
            }
        }
    }
    tools
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
    fn the_dialog_is_pi_nvims_diff_format() {
        let d = dialog("piton:w:1", Path::new("/r/.piton/permissions.json"), "{}");
        assert_eq!(d["type"], "extension_ui_request");
        assert_eq!(d["method"], "select");
        let title: Value = serde_json::from_str(d["title"].as_str().unwrap()).unwrap();
        assert_eq!(title["toolName"], "write");
        assert_eq!(title["toolInput"]["path"], "/r/.piton/permissions.json");
        assert_eq!(title["toolInput"]["content"], "{}");
    }

    #[test]
    fn writes_atomically_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/permissions.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "old").unwrap();
        let link = dir.path().join("permissions.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write(&link, "new").unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        // A new file in a new folder.
        write(&dir.path().join("a/.pi/settings.json"), "{}").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/.pi/settings.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn compares_allowed_tools() {
        let list = "---\nname: s\nallowed-tools:\n    - \"read\"\n    - \"bash(git *)\"\n---\n# Body\n";
        let edited = "---\nname: s\nallowed-tools:\n    - \"read\"\n    - \"bash(git *)\"\n---\n# Better body\n";
        let widened = "---\nname: s\nallowed-tools:\n    - \"read\"\n    - \"bash(*)\"\n---\n# Body\n";
        assert!(same_allowed_tools(list, edited));
        assert!(!same_allowed_tools(list, widened));
        assert!(same_allowed_tools("# no frontmatter", "# still none"));
        assert!(!same_allowed_tools("# none", "---\nallowed-tools: [read]\n---\n"));
        assert!(same_allowed_tools(
            "---\nallowed-tools: [read, write]\n---\n",
            "---\nallowed-tools: read write\n---\nmore\n"
        ));
    }
}
