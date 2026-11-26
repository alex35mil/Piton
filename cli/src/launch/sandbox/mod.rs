//! The one place that knows the sandbox: Piton's built-in policy (`policy`), one launch's resolved rules (`resolve`,
//! `rules`), applying them with the `nono` library (`apply`, through `piton sandbox-exec`), and the network gate that
//! enforces a filtered network's named hosts (`network`).
//!
//! The daemon resolves a launch's [`Policy`] and starts `<this binary> sandbox-exec -- pi ...` with it; the helper
//! sandboxes itself and becomes pi. The daemon is never sandboxed.

mod apply;
pub mod grant;
pub mod network;
mod policy;
mod resolve;
mod rules;

use std::path::PathBuf;

#[cfg(test)]
pub use self::policy::PitonArea;
#[cfg(test)]
pub use self::resolve::{ClaudeCode, ProfileChoice, SilenceSource, Silent};
#[cfg(test)]
pub use self::rules::{Apps, Effect, Listen, Rule, ServiceGrant, Services, Target};
pub use self::{
    apply::{Contained, Containment, POLICY_ENV, contained, containment, exec, inside, report_launched},
    policy::{Provenance, Source, WRITE_ROOTS, agent_dir_runtime, nono_version},
    resolve::{
        ClaudeCodeFolder, ClaudeCodeSockets, Facts, Resolution, Silenced, agent_dir, never_configuration, resolve,
        silenced,
    },
    rules::{Access, Destination, Gate, Network, NetworkGrant, Policy, TAG_PREFIX, is_listen},
};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox grant {grant:?}: ${var} isn't set in the daemon's environment")]
    UnsetVariable { grant: String, var: String },
    #[error("sandbox grant {grant:?}: expands to {expanded:?}, which isn't an absolute path")]
    NotAbsolute { grant: String, expanded: String },
    #[error("PI_CODING_AGENT_DIR {path}: refused for a sandboxed launch: {reason}")]
    AgentDir { path: PathBuf, reason: String },
    #[error(
        "{var}={value}: refused for a sandboxed launch: set by {set_by}, which a sandboxed session can write, and it \
         leads out of the checkout {checkout}; set it where the agent can't write (your shell or global mise \
         configuration), or point it inside the checkout"
    )]
    Widening {
        var: &'static str,
        value: String,
        /// The file that set it, or what the tool said when it named none.
        set_by: String,
        checkout: PathBuf,
    },
}
