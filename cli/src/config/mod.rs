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

pub use self::{
    check::portability::warnings as portability_warnings,
    model::{
        daemon::{Build, DaemonConfig, TailnetConfig, ToolChoice, ToolConfig, default_socket},
        paths::{AGENTS_DIR, LOCAL_DIR, PITON_DIR, Paths, Scope, Sharing, expand_home, home},
        tables::{
            AllGrant, ClaudeCodeOptions, ClaudeLogin, CommandGrant, CommandPattern, CommandRuntime, Connect, Grant,
            GrantLists, Listed, NetworkTarget, PermissionSettings, Preset, Profile, SandboxTable, ServiceEntry,
            ServiceName, Silence, SilenceTarget,
        },
    },
    read::{
        global::{
            GlobalConfig, GlobalResolved, client, daemon, daemon_lenient, global, global_candidates, host, own_files,
            resolve_global,
        },
        resolve::{LaunchConfig, Resolved, Runtime, launch_levels, resolve, resolve_launch},
    },
    write::{
        edit::{EditError, Placement},
        file::{ConfigFile, KeyChange, WriteLevel, set_global},
    },
};

/// Why configuration can't be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("{path}: `{key}` belongs in a project's .piton/, not the global tier")]
    ProjectOnly { path: PathBuf, key: String },
    /// A secret, in a file that isn't `config.local.toml`.
    #[error("{path}: `{key}` belongs in config.local.toml, which isn't committed")]
    LocalOnly { path: PathBuf, key: String },
    /// A key the user sets for themselves (one that loosens the sandbox, a personal value), in a project's
    /// `config.toml`, which travels with the repository.
    #[error(
        "{path}: `{key}` can't be set in a project's config.toml, which travels with the repository; set it in that \
         folder's config.local.toml or the global config.toml"
    )]
    NotInProjectConfig { path: PathBuf, key: String },
    #[error("{path}: `{key}` {reason}")]
    Conflict { path: PathBuf, key: String, reason: String },
}
