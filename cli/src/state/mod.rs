//! The host's state database: facts about this host and what happened on it, as opposed to configuration (what the user
//! decides, in files). SQLite, owned by the daemon alone, in its state folder next to the socket
//! (`~/.local/state/piton/piton.db` by default). Holds no preferences.
//!
//! - [`db`]: opening, the schema's migrations, and the backup before migrating.
//! - [`transfer`]: `piton state export|import`, and the read-only check `piton setup` uses.
//!
//! Tables are added by the work that needs them: `host` (this host's identity) so far.

mod db;
mod transfer;

use std::path::PathBuf;

pub use self::{
    db::{FILE, State},
    transfer::{check, export, import},
};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{path}: {source}")]
    Sqlite { path: PathBuf, source: rusqlite::Error },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error(
        "{path} has schema version {found}, newer than this piton understands ({supported}); update piton, or move the file aside to start over"
    )]
    Newer { path: PathBuf, found: i64, supported: i64 },
    #[error("{path} isn't a Piton state database (no host identity)")]
    NotAState { path: PathBuf },
    #[error("{path} already holds state ({tables}); import over it with --force, which replaces it")]
    HasState { path: PathBuf, tables: String },
    #[error("{path} exists; choose another file")]
    Exists { path: PathBuf },
    #[error("{path} doesn't exist: this host has no state yet")]
    Missing { path: PathBuf },
}

fn sqlite(path: &std::path::Path, source: rusqlite::Error) -> StateError {
    StateError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn io(path: &std::path::Path, source: std::io::Error) -> StateError {
    StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn set_owner_only(path: &std::path::Path) -> Result<(), StateError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| io(path, source))
}
