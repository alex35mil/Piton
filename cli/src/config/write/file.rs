//! The one write path, for `piton config set` and the apps: which file a change goes to, the change in a person's
//! terms (a dotted key and a JSON value), and the result checked before it's written. Every new file text is parsed as
//! its level's readers will parse it, so a write never leaves a file they'd refuse.

use std::path::{Path, PathBuf};

pub use super::edit::Placement;
use super::edit::{Change, EditError, append_table, edited, item_from_json, write_changes};
use crate::config::{
    ConfigError,
    model::{
        client::ClientFile,
        host::HostFile,
        level::LevelKind,
        paths::{FilePair, PITON_DIR, Paths, Scope, Sharing},
    },
    read::parse::{ConfigDoc, parse_global},
};

/// The files a write goes to: a project's (the global tier's `config.toml`, or a repository's source checkout's
/// `.piton/`; a worker's own copy is never read), or one of the global tier's own files.
#[derive(Debug, Clone)]
pub enum WriteLevel {
    /// The global `config.toml`: the defaults every project inherits.
    Global,
    Repository {
        source: PathBuf,
    },
    /// `config.host[.local].toml`.
    Host,
    /// `config.client.toml`.
    Client,
}

impl WriteLevel {
    /// The global tier's files of `scope`.
    pub fn global(scope: Scope) -> Self {
        match scope {
            Scope::Project => WriteLevel::Global,
            Scope::Host => WriteLevel::Host,
            Scope::Client => WriteLevel::Client,
        }
    }

    fn files(&self, paths: &Paths) -> FilePair {
        match self {
            WriteLevel::Global => paths.global_files(Scope::Project),
            WriteLevel::Repository { source } => FilePair::project(&source.join(PITON_DIR)),
            WriteLevel::Host => paths.global_files(Scope::Host),
            WriteLevel::Client => paths.global_files(Scope::Client),
        }
    }

    /// Whether a table value is written inline on its key's line (`code_font = { mac = "A" }`) rather than as a
    /// `[table]`: the client file's per-device values, so each setting reads on one line.
    fn inline_tables(&self) -> bool {
        match self {
            WriteLevel::Client => true,
            WriteLevel::Global | WriteLevel::Repository { .. } | WriteLevel::Host => false,
        }
    }

    /// A file's new text, checked as every reader will read it.
    fn validate(&self, text: &str, path: &Path, sharing: Sharing) -> Result<(), ConfigError> {
        match self {
            WriteLevel::Global => ConfigDoc::parse(text, path, LevelKind::Global, sharing)?.validate(),
            WriteLevel::Repository { .. } => ConfigDoc::parse(text, path, LevelKind::Repository, sharing)?.validate(),
            WriteLevel::Host => parse_global::<HostFile>(text, path).map(drop),
            WriteLevel::Client => parse_global::<ClientFile>(text, path).map(drop),
        }
    }
}

/// One key set to a value, or removed (`None`, back to its default). The value is JSON, written as a person would (an
/// object becomes a `[table]`).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyChange {
    pub key: KeyPath,
    pub value: Option<serde_json::Value>,
}

/// A key from the top of a file, as its parts: written as a TOML dotted key, with a part quoted when it isn't bare
/// (`branches.format`, `sandbox.filesystem."~/.cache"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPath(Vec<String>);

impl KeyPath {
    pub fn new(parts: Vec<String>) -> Self {
        Self(parts)
    }

    /// A TOML dotted key: bare parts, or quoted ones (`a."b.c"`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let parts = toml_edit::Key::parse(text).map_err(|e| format!("invalid key {text:?}: {e}"))?;
        match parts.iter().any(|part| part.get().is_empty()) || parts.is_empty() {
            true => Err(format!("invalid key {text:?}")),
            false => Ok(Self(parts.iter().map(|part| part.get().to_string()).collect())),
        }
    }

    pub fn parts(&self) -> &[String] {
        &self.0
    }
}

impl std::fmt::Display for KeyPath {
    /// As a TOML dotted key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|part| toml_edit::Key::new(part.as_str()).display_repr().into_owned())
            .collect();
        f.write_str(&parts.join("."))
    }
}

/// One file of a level's pair.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub level: WriteLevel,
    pub sharing: Sharing,
}

impl ConfigFile {
    /// The file, or why its pair has none of this sharing (the client file has no local one).
    pub fn path(&self, paths: &Paths) -> Result<PathBuf, EditError> {
        let files = self.level.files(paths);
        files
            .path(self.sharing)
            .map(Path::to_path_buf)
            .ok_or(EditError::NoLocalFile { path: files.shared })
    }

    /// The file's text with `changes` applied, without writing it; `None` when nothing changes. For a caller that
    /// writes the file another way (a protected file, through the daemon).
    pub fn plan(&self, paths: &Paths, changes: &[KeyChange]) -> Result<Option<String>, EditError> {
        let path = self.path(paths)?;
        let items = Items::new(changes, &path, self.level.inline_tables())?;
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(EditError::Io { path, source }),
        };
        let content = edited(current.as_deref(), &items.changes(), &path)?;
        if let Some(text) = &content {
            self.level.validate(text, &path, self.sharing)?;
        }
        Ok(content)
    }

    /// Appends a `[[sandbox.commands]]` entry: what "Allow and persist" saves.
    pub fn append_command_grant(&self, paths: &Paths, grant: &crate::config::CommandGrant) -> Result<(), EditError> {
        let path = self.path(paths)?;
        let value = toml::Value::try_from(grant).map_err(|e| EditError::Value {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let text = toml::to_string(&value).map_err(|e| EditError::Value {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let entry: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| EditError::Parse {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let sharing = self.sharing;
        append_table(&path, &["sandbox", "commands"], entry.as_table(), &|text, path| {
            self.level.validate(text, path, sharing)
        })
    }

    /// Writes `changes`, keeping comments and layout.
    pub fn write(&self, paths: &Paths, changes: &[KeyChange]) -> Result<(), EditError> {
        let path = self.path(paths)?;
        let items = Items::new(changes, &path, self.level.inline_tables())?;
        let sharing = self.sharing;
        write_changes(&path, &items.changes(), &|text, path| {
            self.level.validate(text, path, sharing)
        })
    }
}

/// Sets one key of a global pair in the file `placement` names, moving it out of the other one when it moves. Returns
/// the file it's now in.
pub fn set_global(paths: &Paths, scope: Scope, change: &KeyChange, placement: Placement) -> Result<Sharing, EditError> {
    let level = WriteLevel::global(scope);
    let files = level.files(paths);
    let items = Items::new(std::slice::from_ref(change), &files.shared, level.inline_tables())?;
    let (key, item) = &items.0[0];
    files.set(key, item.clone(), placement, &|text, path, sharing| {
        level.validate(text, path, sharing)
    })
}

/// Changes as key parts and TOML items.
struct Items<'a>(Vec<(Vec<&'a str>, Option<toml_edit::Item>)>);

impl<'a> Items<'a> {
    fn new(changes: &'a [KeyChange], path: &Path, inline_tables: bool) -> Result<Self, EditError> {
        let mut items = Vec::new();
        for change in changes {
            let parts: Vec<&str> = change.key.parts().iter().map(String::as_str).collect();
            if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
                return Err(EditError::Value {
                    path: path.to_path_buf(),
                    reason: format!("invalid key {}", change.key),
                });
            }
            let item = match &change.value {
                Some(value) => {
                    let item = item_from_json(&parts, value, path)?;
                    Some(match (inline_tables, item) {
                        (true, item @ toml_edit::Item::Table(_)) => {
                            let mut value = item.into_value().map_err(|_| EditError::Value {
                                path: path.to_path_buf(),
                                reason: format!("{}: not a value", change.key),
                            })?;
                            value.decor_mut().clear();
                            toml_edit::Item::Value(value)
                        }
                        (
                            true | false,
                            item @ (toml_edit::Item::None
                            | toml_edit::Item::Value(_)
                            | toml_edit::Item::Table(_)
                            | toml_edit::Item::ArrayOfTables(_)),
                        ) => item,
                    })
                }
                None => None,
            };
            items.push((parts, item));
        }
        Ok(Self(items))
    }

    fn changes(&self) -> Vec<Change<'_>> {
        self.0
            .iter()
            .map(|(key, item)| Change {
                key,
                value: item.as_ref(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AllGrant, CommandGrant, CommandPattern, CommandRuntime, Grant};

    fn grant(pattern: &str) -> CommandGrant {
        CommandGrant {
            pattern: CommandPattern::new(pattern).unwrap(),
            runtime: CommandRuntime::Sandbox(Grant::All(AllGrant::All)),
        }
    }

    /// "Allow and persist" appends exactly one `[[sandbox.commands]]` entry, keeping the rest of the file. Piton saves
    /// it only to the user's own files (a project's `config.local.toml`); a committed `config.toml` may hold one too,
    /// applied only once the repository is trusted.
    #[test]
    fn appends_a_command_grant() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().into(),
            global: dir.path().join(".config/piton"),
        };
        let global = ConfigFile {
            level: WriteLevel::Global,
            sharing: Sharing::Shared,
        };
        let path = global.path(&paths).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# mine\n[sandbox]\npresets = [\"rust\"]\n").unwrap();
        global.append_command_grant(&paths, &grant("husky install *")).unwrap();
        global
            .append_command_grant(
                &paths,
                &CommandGrant {
                    pattern: CommandPattern::new("git push").unwrap(),
                    runtime: CommandRuntime::Host,
                },
            )
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n[sandbox]\npresets = [\"rust\"]\n"), "{text}");
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        let commands = parsed["sandbox"]["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["match"].as_str(), Some("bash(husky install *)"));
        assert_eq!(commands[0]["runtime"].as_str(), Some("sandbox"));
        assert_eq!(commands[0]["grant"].as_str(), Some("all"));
        assert_eq!(commands[1]["match"].as_str(), Some("bash(git push)"));
        assert_eq!(commands[1]["runtime"].as_str(), Some("host"));
        assert!(commands[1].get("grant").is_none());
        // What it wrote reads back as the configuration reads it.
        let back: crate::config::SandboxTable = parsed["sandbox"].clone().try_into().unwrap();
        assert_eq!(back.commands[1].runtime, CommandRuntime::Host);

        let source = dir.path().join("repo");
        let project = |sharing| ConfigFile {
            level: WriteLevel::Repository { source: source.clone() },
            sharing,
        };
        project(Sharing::Shared)
            .append_command_grant(&paths, &grant("x"))
            .unwrap();
        project(Sharing::Local)
            .append_command_grant(&paths, &grant("x"))
            .unwrap();
        let local = std::fs::read_to_string(source.join(".piton/config.local.toml")).unwrap();
        assert!(local.contains("[[sandbox.commands]]"), "{local}");

        // A process block's entry: one key of a map, the path quoted, keeping what's there.
        for key in ["sandbox.filesystem.\"~/.cache/x\"", "sandbox.filesystem.\"/b\""] {
            global
                .write(
                    &paths,
                    &[KeyChange {
                        key: KeyPath::parse(key).unwrap(),
                        value: Some(serde_json::json!("write")),
                    }],
                )
                .unwrap();
        }
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = toml::from_str(&written).unwrap();
        assert_eq!(
            parsed["sandbox"]["filesystem"]["~/.cache/x"].as_str(),
            Some("write"),
            "{written}"
        );
        assert_eq!(parsed["sandbox"]["filesystem"]["/b"].as_str(), Some("write"));
        assert_eq!(parsed["sandbox"]["commands"].as_array().unwrap().len(), 2, "kept");
        assert!(written.contains("\"~/.cache/x\" = \"write\""), "{written}");
    }

    /// A key is a TOML dotted key: bare parts, or quoted ones holding dots and slashes.
    #[test]
    fn key_paths_parse_and_print() {
        let key = KeyPath::parse("sandbox.filesystem.\"~/.ssh/config\"").unwrap();
        assert_eq!(key.parts(), ["sandbox", "filesystem", "~/.ssh/config"]);
        assert_eq!(key.to_string(), "sandbox.filesystem.\"~/.ssh/config\"");
        assert_eq!(
            KeyPath::parse("branches.format").unwrap().parts(),
            ["branches", "format"]
        );
        for bad in ["", "a..b", "a.", "sandbox.filesystem.~/x"] {
            assert!(KeyPath::parse(bad).is_err(), "{bad}");
        }
    }
}
