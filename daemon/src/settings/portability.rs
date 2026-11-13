//! What keeps configuration portable between Macs and people, checked by `piton config check`: no absolute path under
//! another user's home (write `~` or `$HOME`), and no secret in a file Git tracks (use the environment or a `.local`
//! file). Both are warnings: the files still work on this Mac.

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::resolve::FileOrigin;

/// Keys whose value is a secret, compared without case, `_`, and `-` (`api_key`, `apiKey`, `Authorization`).
const SECRET_KEYS: [&str; 6] = ["apikey", "token", "password", "secret", "authorization", "accesstoken"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// A path under another user's home folder, which won't exist for this user or on another Mac.
    ForeignHome { path: PathBuf, key: String, value: String },
    /// A secret in a file Git tracks, so it travels with the repository.
    TrackedSecret { path: PathBuf, key: String },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Warning::ForeignHome { path, key, value } => write!(
                f,
                "{}: `{key}` = {value:?} is under another user's home folder; write it with `~` or `$HOME`",
                path.display()
            ),
            Warning::TrackedSecret { path, key } => write!(
                f,
                "{}: `{key}` holds a secret in a file Git tracks; move it to the environment or a `.local` file",
                path.display()
            ),
        }
    }
}

/// Every warning for the configuration files read (`config[.local].toml`, `mcp[.local].json`); other files and ones
/// that can't be parsed (the resolver reports those) are skipped.
pub fn warnings(files: &[FileOrigin], home: &Path) -> Vec<Warning> {
    let mut warnings = Vec::new();
    for file in files {
        let Some(value) = read(&file.path) else {
            continue;
        };
        let tracked = tracked_by_git(&file.path);
        walk(&value, &mut Vec::new(), &mut |key, value| {
            if let Value::String(text) = value {
                if foreign_home(text, home) {
                    warnings.push(Warning::ForeignHome {
                        path: file.path.clone(),
                        key: key.join("."),
                        value: text.clone(),
                    });
                }
                if tracked && is_secret(key, text) {
                    warnings.push(Warning::TrackedSecret {
                        path: file.path.clone(),
                        key: key.join("."),
                    });
                }
            }
        });
    }
    warnings
}

fn read(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => serde_json::to_value(text.parse::<toml::Table>().ok()?).ok(),
        Some("json") => serde_json::from_str(&text).ok(),
        Some(_) | None => None,
    }
}

/// Calls `visit` with each leaf's dotted key (list items keep their list's key).
fn walk(value: &Value, key: &mut Vec<String>, visit: &mut dyn FnMut(&[String], &Value)) {
    match value {
        Value::Object(map) => {
            for (name, inner) in map {
                key.push(name.clone());
                walk(inner, key, visit);
                key.pop();
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, key, visit);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => visit(key, value),
    }
}

/// Under a sibling of this user's home (`/Users/other/...`), not under the home itself.
fn foreign_home(text: &str, home: &Path) -> bool {
    let path = Path::new(text);
    let Some(homes) = home.parent() else {
        return false;
    };
    path.is_absolute() && path.starts_with(homes) && !path.starts_with(home) && path != homes
}

/// A secret key with a literal value: `${VAR}` and `$VAR` name the environment, which is the right place.
fn is_secret(key: &[String], text: &str) -> bool {
    let normalized = |name: &str| name.to_lowercase().replace(['_', '-'], "");
    let secret_key = key
        .last()
        .is_some_and(|name| SECRET_KEYS.contains(&normalized(name).as_str()));
    secret_key && !text.is_empty() && !text.contains('$')
}

fn tracked_by_git(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::LevelKind;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn warns_about_foreign_homes_and_tracked_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("users/me");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".piton")).unwrap();
        let other = root.join("users/other/data");
        let shared = repo.join(".piton/config.toml");
        let local = repo.join(".piton/config.local.toml");
        let mcp = repo.join(".piton/mcp.json");
        std::fs::write(
            &shared,
            format!(
                "[sandbox]\nread = [\"{}\", \"{}\", \"~/x\"]\n[web_search]\napi_key = \"sk-1\"\n",
                other.display(),
                home.join("mine").display()
            ),
        )
        .unwrap();
        std::fs::write(&local, "[web_search]\napi_key = \"sk-2\"\n").unwrap();
        std::fs::write(
            &mcp,
            r#"{"mcpServers": {"a": {"headers": {"Authorization": "Bearer ${KEY}"}}}}"#,
        )
        .unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", ".piton/config.toml", ".piton/mcp.json"]);

        let files: Vec<FileOrigin> = [&shared, &local, &mcp]
            .into_iter()
            .map(|path| FileOrigin {
                path: path.clone(),
                level: LevelKind::Repository,
            })
            .collect();
        assert_eq!(
            warnings(&files, &home),
            [
                Warning::ForeignHome {
                    path: shared.clone(),
                    key: "sandbox.read".into(),
                    value: other.display().to_string(),
                },
                // The tracked file's literal key; the untracked local file and an environment reference are fine.
                Warning::TrackedSecret {
                    path: shared,
                    key: "web_search.api_key".into(),
                },
            ]
        );
    }
}
