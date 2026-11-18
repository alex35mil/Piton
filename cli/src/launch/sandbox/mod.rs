//! The one place that knows the sandbox: Piton's built-in policy (`policy`), one launch's resolved rules (`resolve`,
//! `rules`), and applying them with the `nono` library (`apply`, through `piton sandbox-exec`).
//!
//! The daemon resolves a launch's [`Policy`] and starts `<this binary> sandbox-exec -- pi ...` with it; the helper
//! sandboxes itself and becomes pi. The daemon is never sandboxed.

mod apply;
mod policy;
mod resolve;
mod rules;

use std::path::PathBuf;

pub use self::{
    apply::{POLICY_ENV, exec},
    policy::{Source, WRITE_ROOTS, nono_version},
    resolve::{Facts, Resolution, resolve},
    rules::{Access, Policy},
};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox grant {grant:?}: ${var} isn't set in the daemon's environment")]
    UnsetVariable { grant: String, var: String },
    #[error("sandbox grant {grant:?}: expands to {expanded:?}, which isn't an absolute path")]
    NotAbsolute { grant: String, expanded: String },
    #[error("PI_CODING_AGENT_DIR {path}: refused for a sandboxed launch: {reason}")]
    AgentDir { path: PathBuf, reason: String },
}
