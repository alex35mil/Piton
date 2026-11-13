//! The one write path, for `piton config set` and the apps: which file a change goes to, the change in a person's
//! terms (a dotted key and a JSON value), and the result checked before it's written. Every new file text is parsed as
//! its level's readers will parse it, so a write never leaves a file they'd refuse.

use std::path::{Path, PathBuf};

use super::edit::{Change, EditError, edited, item_from_json, write_changes};
use crate::config::ConfigError;
use crate::config::model::level::LevelKind;
use crate::config::model::paths::{FilePair, PITON_DIR, Paths, Sharing};
use crate::config::read::parse::ConfigDoc;

pub use super::edit::Placement;

/// The level a write goes to: the global tier, or a repository (its source checkout's `.piton/`; a worker's own copy
/// is never read).
#[derive(Debug, Clone)]
pub enum WriteLevel {
    Global,
    Repository { source: PathBuf },
}

impl WriteLevel {
    fn kind(&self) -> LevelKind {
        match self {
            WriteLevel::Global => LevelKind::Global,
            WriteLevel::Repository { .. } => LevelKind::Repository,
        }
    }

    fn files(&self, paths: &Paths) -> FilePair {
        match self {
            WriteLevel::Global => paths.global_files(),
            WriteLevel::Repository { source } => FilePair::in_dir(&source.join(PITON_DIR)),
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
    pub fn path(&self, paths: &Paths) -> PathBuf {
        self.level.files(paths).path(self.sharing).to_path_buf()
    }

    /// The file's text with `changes` applied, without writing it; `None` when nothing changes. For a caller that
    /// writes the file another way (a protected file, through the daemon).
    pub fn plan(&self, paths: &Paths, changes: &[KeyChange]) -> Result<Option<String>, EditError> {
        let path = self.path(paths);
        let items = Items::new(changes, &path)?;
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(EditError::Io { path, source }),
        };
        let content = edited(current.as_deref(), &items.changes(), &path)?;
        if let Some(text) = &content {
            validate(text, &path, self.level.kind(), self.sharing)?;
        }
        Ok(content)
    }

    /// Writes `changes`, keeping comments and layout.
    pub fn write(&self, paths: &Paths, changes: &[KeyChange]) -> Result<(), EditError> {
        let path = self.path(paths);
        let items = Items::new(changes, &path)?;
        let (level, sharing) = (self.level.kind(), self.sharing);
        write_changes(&path, &items.changes(), &|text, path| {
            validate(text, path, level, sharing)
        })
    }
}

/// Sets one key of the global pair in the file `placement` names, moving it out of the other one when it moves. Returns
/// the file it's now in.
pub fn set_global(paths: &Paths, change: &KeyChange, placement: Placement) -> Result<Sharing, EditError> {
    let path = paths.global_files().path(Sharing::Shared).to_path_buf();
    let items = Items::new(std::slice::from_ref(change), &path)?;
    let (key, item) = &items.0[0];
    paths
        .global_files()
        .set(key, item.clone(), placement, &|text, path, sharing| {
            validate(text, path, LevelKind::Global, sharing)
        })
}

/// A file's new text, checked as every reader will read it.
fn validate(text: &str, path: &Path, level: LevelKind, sharing: Sharing) -> Result<(), ConfigError> {
    ConfigDoc::parse(text, path, level, sharing)?.validate()
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
