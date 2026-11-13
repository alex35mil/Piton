//! `[host]`, global tier only: this host's preferences the apps change. `config.local.toml` wins per key.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, nearer};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostTable {
    /// Where the apps may browse and start sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorites: Option<Vec<String>>,
    /// Host logging default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<crate::diagnostics::Mode>,
    /// Mode for `piton attach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_mode: Option<crate::wire::LaunchMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_grace_ms: Option<u64>,
}

impl TableValue for HostTable {
    /// A part exists only when a file sets its table, so setting it at all outside the global tier is refused.
    fn global_only_keys(&self) -> Vec<&'static str> {
        vec!["host"]
    }
}

impl Merge for HostTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.roots, n.roots);
        nearer(&mut self.favorites, n.favorites);
        nearer(&mut self.logging, n.logging);
        nearer(&mut self.attach_mode, n.attach_mode);
        nearer(&mut self.empty_grace_ms, n.empty_grace_ms);
    }
}
