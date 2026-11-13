//! Which file a change goes to, and the change checked before it's written: the one write path for `piton config set`
//! and the apps. Every new file text is parsed as its level's readers will parse it, so a write never leaves a file
//! they'd refuse.

use std::path::{Path, PathBuf};

use toml_edit::Item;

use super::edit::{Change, EditError, Target, edited, write_changes};
use crate::config::ConfigError;
use crate::config::model::level::LevelKind;
use crate::config::model::paths::{FilePair, PITON_DIR, Paths, Sharing};
use crate::config::read::parse::ConfigDoc;

/// The level a write goes to: the global tier, or a repository (its source checkout's `.piton/`; a worker's own copy
/// is never read).
#[derive(Debug, Clone)]
pub enum Level {
    Global,
    Repository { source: PathBuf },
}

impl Level {
    fn kind(&self) -> LevelKind {
        match self {
            Level::Global => LevelKind::Global,
            Level::Repository { .. } => LevelKind::Repository,
        }
    }

    fn files(&self, paths: &Paths) -> FilePair {
        match self {
            Level::Global => paths.global_files(),
            Level::Repository { source } => FilePair::in_dir(&source.join(PITON_DIR)),
        }
    }
}

/// A file's new text, checked as every reader will read it.
pub fn validate(text: &str, path: &Path, level: LevelKind, sharing: Sharing) -> Result<(), ConfigError> {
    ConfigDoc::parse(text, path, level, sharing)?.validate()
}

/// One file of a level's pair.
pub struct File {
    pub level: Level,
    pub sharing: Sharing,
}

impl File {
    pub fn path(&self, paths: &Paths) -> PathBuf {
        self.level.files(paths).path(self.sharing).to_path_buf()
    }

    /// The file's text with `changes` applied, without writing it; `None` when nothing changes. For a caller that
    /// writes the file another way (a protected file, through the daemon).
    pub fn plan(&self, paths: &Paths, changes: &[Change]) -> Result<Option<String>, EditError> {
        let path = self.path(paths);
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(EditError::Io { path, source }),
        };
        let content = edited(current.as_deref(), changes, &path)?;
        if let Some(text) = &content {
            validate(text, &path, self.level.kind(), self.sharing)?;
        }
        Ok(content)
    }

    /// Writes `changes`, keeping comments and layout.
    pub fn write(&self, paths: &Paths, changes: &[Change]) -> Result<(), EditError> {
        let (level, sharing) = (self.level.kind(), self.sharing);
        write_changes(&self.path(paths), changes, &|text, path| {
            validate(text, path, level, sharing)
        })
    }
}

/// Sets one key of the global pair in the file `target` names, moving it out of the other one when it moves.
pub fn set_global(paths: &Paths, key: &[&str], value: Option<Item>, target: Target) -> Result<Sharing, EditError> {
    paths.global_files().set(key, value, target, &|text, path, sharing| {
        validate(text, path, LevelKind::Global, sharing)
    })
}
