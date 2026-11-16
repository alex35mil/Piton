//! The one write path, for `piton config set` and the apps: which file a change goes to, the change in a person's
//! terms (a dotted key and a JSON value), and the result checked before it's written. Every new file text is parsed as
//! its level's readers will parse it, so a write never leaves a file they'd refuse.

use std::path::{Path, PathBuf};

pub use super::edit::Placement;
use super::edit::{Change, EditError, edited, item_from_json, write_changes};
use crate::config::ConfigError;
use crate::config::model::client::ClientFile;
use crate::config::model::host::HostFile;
use crate::config::model::level::LevelKind;
use crate::config::model::paths::{FilePair, PITON_DIR, Paths, Scope, Sharing};
use crate::config::read::parse::{ConfigDoc, parse_global};

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

/// One key set to a value, or removed (`None`, back to its default). The key is dotted (`branches.format`); the value
/// is JSON, written as a person would (an object becomes a `[table]`).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyChange {
    pub key: String,
    pub value: Option<serde_json::Value>,
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
        let items = Items::new(changes, &path)?;
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

    /// Writes `changes`, keeping comments and layout.
    pub fn write(&self, paths: &Paths, changes: &[KeyChange]) -> Result<(), EditError> {
        let path = self.path(paths)?;
        let items = Items::new(changes, &path)?;
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
    let items = Items::new(std::slice::from_ref(change), &files.shared)?;
    let (key, item) = &items.0[0];
    files.set(key, item.clone(), placement, &|text, path, sharing| {
        level.validate(text, path, sharing)
    })
}

/// Changes as key parts and TOML items.
struct Items<'a>(Vec<(Vec<&'a str>, Option<toml_edit::Item>)>);

impl<'a> Items<'a> {
    fn new(changes: &'a [KeyChange], path: &Path) -> Result<Self, EditError> {
        let mut items = Vec::new();
        for change in changes {
            let parts: Vec<&str> = change.key.split('.').collect();
            if parts.iter().any(|part| part.is_empty()) {
                return Err(EditError::Value {
                    path: path.to_path_buf(),
                    reason: format!("invalid key {:?}", change.key),
                });
            }
            let item = match &change.value {
                Some(value) => Some(item_from_json(&parts, value, path)?),
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
