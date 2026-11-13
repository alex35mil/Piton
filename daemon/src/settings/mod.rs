//! piton's configuration files: where they are, which levels apply to a workspace, and how they merge.
//!
//! Two chains of folders, read farthest first:
//!
//! - `.piton/`: the source checkout (a worker's source, the checkout around a subfolder), then each ancestor folder
//!   strictly below the home folder, then the global tier `$XDG_CONFIG_HOME/piton/`. Settings that decide what an
//!   agent may do are read from the source, never from a worker, so an agent can't change them by editing its clone.
//! - `.agents/`: the workspace's own checkout (a worker reads its own copy, so instructions apply where they're
//!   edited), then the same ancestors, then the global tier's `agents/`.
//!
//! Each `.piton/` level (and the global tier) has one `config.toml` and its `config.local.toml` (this machine's values,
//! never shared), read in that order; `mcp[.local].json` and `policies.md` stay separate files. Each table of the file
//! is a part with one merge rule (see `merge.rs`). This module is the only implementation: the pi package and the
//! Neovim plugin read the result through `piton config resolve` / `piton config get`, and change it through
//! `piton config set`.

mod chain;
pub mod convert;
pub mod edit;
mod files;
mod merge;
mod resolve;

pub use chain::{LevelKind, global_tier, home};
pub use files::{
    ConfigDoc, DaemonFile, NetworkProfile, SandboxTable, SettingsFile, Sharing, expand_home, validate_config,
};
pub use resolve::{DaemonSettings, Part, Paths, Resolved, Runtime, Section, resolve, resolve_daemon, resolve_global};

use std::path::PathBuf;

/// The folder piton's tools read in a repository or a parent folder.
pub const PITON_DIR: &str = ".piton";
/// The folder the agent reads in a repository or a parent folder.
pub const AGENTS_DIR: &str = ".agents";
/// Workflow artifacts in a source checkout's `.piton/`; never configuration. A worker links its source's.
pub const LOCAL_DIR: &str = ".piton/.local";

/// Where a file came from, for messages and for `piton config check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub path: PathBuf,
    pub level: LevelKind,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
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
    #[error("{}", old_layout_message(.0))]
    OldLayout(Vec<OldPath>),
    #[error(transparent)]
    Project(#[from] crate::project::ProjectError),
}

/// A file or folder from the layout before `.piton/`, where its contents go now, and the parts of the configuration
/// it held (only those refuse it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldPath {
    pub path: PathBuf,
    pub now: &'static str,
    pub parts: &'static [Part],
}

fn old_layout_message(paths: &[OldPath]) -> String {
    let mut text = String::from("this uses piton's old configuration layout; run `piton migrate` (or move by hand):");
    for old in paths {
        text.push_str(&format!("\n  {} -> {}", old.path.display(), old.now));
    }
    text
}
