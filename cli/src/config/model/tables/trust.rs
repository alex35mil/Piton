//! `[trust]`: how the user's decision about a repository's grants is kept (`crate::trust`). The user's own files only:
//! a repository can't choose how its own grants are re-checked.

use serde::{Deserialize, Serialize};

use super::{Merge, Rule, TableValue, nearer, present};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconfirm: Option<Reconfirm>,
}

/// When a trusted repository is asked about again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reconfirm {
    /// Never: trusted stays trusted, whatever the repository's grants become, until the user changes it.
    #[default]
    Never,
    /// When the repository's committed grants gain one the user didn't trust: the trusted ones keep applying, and the
    /// next launch asks about the new ones.
    OnChange,
}

impl TrustTable {
    /// The setting in effect: the nearest file's, else the default.
    pub fn reconfirm(&self) -> Reconfirm {
        self.reconfirm.unwrap_or_default()
    }
}

impl TableValue for TrustTable {
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        present(Rule::USERS_OWN, &[("trust.reconfirm", self.reconfirm.is_some())])
    }
}

impl Merge for TrustTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.reconfirm, n.reconfirm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_kebab_case_and_default_to_never() {
        let table: TrustTable = toml::from_str("reconfirm = \"on-change\"\n").unwrap();
        assert_eq!(table.reconfirm(), Reconfirm::OnChange);
        assert_eq!(TrustTable::default().reconfirm(), Reconfirm::Never);
        assert!(toml::from_str::<TrustTable>("reconfirm = \"on_change\"\n").is_err());
        assert!(toml::from_str::<TrustTable>("other = 1\n").is_err());
    }
}
