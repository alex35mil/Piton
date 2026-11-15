//! `[web_search]`: the web-search extension's settings.

use serde::{Deserialize, Serialize};

use super::{Merge, Rule, TableValue, nearer, present};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl TableValue for WebSearchTable {
    /// A secret: only in a `config.local.toml`.
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        present(Rule::LOCAL, &[("web_search.api_key", self.api_key.is_some())])
    }
}

impl Merge for WebSearchTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.api_key, n.api_key);
    }
}
