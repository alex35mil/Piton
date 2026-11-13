//! piton's configuration: the only code that reads or writes its files. Everything else (the daemon, the apps through
//! it, and the pi package and Neovim plugin through `piton config`) uses this module's API.
//!
//! - [`model`]: what configuration is, with no I/O: where files live, which levels apply to a workspace, each table's
//!   schema, scope, checks, and merge rule, and the daemon's options.
//! - [`read`]: files into the model: one file's parts, a chain merged per part, the global tier's tables.
//! - [`write`]: the model into files: changes kept in a person's layout, to the right file, checked before writing.
//! - [`check`]: read-only diagnostics beyond errors.
//!
//! Each `.piton/` level and the global tier has one `config.toml` and its `config.local.toml` (this machine's values,
//! never shared), read in that order; `mcp[.local].json` and `policies.md` stay separate files.

mod check;
mod model;
mod read;
mod write;

use std::path::PathBuf;

pub use check::portability::warnings as portability_warnings;
pub use model::daemon::{DaemonConfig, DaemonTables, GlobalConfig, TailnetConfig};
pub use model::paths::{LOCAL_DIR, PITON_DIR, Paths, Sharing, expand_home, home};
pub use model::tables::{NetworkProfile, SandboxTable};
pub use read::global::{daemon, daemon_lenient, daemon_tables, global};
pub use read::resolve::{LaunchConfig, Resolved, Runtime, resolve, resolve_global, resolve_launch};
pub use write::edit::{EditError, Placement, Placements};
pub use write::file::{ConfigFile, KeyChange, WriteLevel, set_global};

/// Why configuration can't be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("{path}: `{key}` belongs in the global tier ({global})")]
    GlobalOnly {
        path: PathBuf,
        key: String,
        global: PathBuf,
    },
    #[error("{path}: `{key}` is personal; set it in config.local.toml")]
    LocalOnly { path: PathBuf, key: String },
    #[error("{path}: `{key}` {reason}")]
    Conflict { path: PathBuf, key: String, reason: String },
}
