//! `[branches]`: how workflows name branches. Merged per key, so a level can set the format and leave the username to
//! `config.local.toml`.

use serde::{Deserialize, Serialize};

use super::{Merge, Rule, TableValue, nearer, present};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchesTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<BranchFormat>,
    /// Personal: `config.local.toml` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum BranchFormat {
    #[serde(rename = "identifier-title")]
    IdentifierTitle,
    #[serde(rename = "username/identifier-title")]
    UsernameIdentifierTitle,
    #[serde(rename = "tracker")]
    Tracker,
}

impl TableValue for BranchesTable {
    /// A name: never in a project's committed `config.toml`.
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        present(Rule::USERS_OWN, &[("branches.username", self.username.is_some())])
    }
}

impl Merge for BranchesTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.format, n.format);
        nearer(&mut self.username, n.username);
    }
}
