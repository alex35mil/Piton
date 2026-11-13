//! `[web_search]`: the web-search extension's settings.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, nearer};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl TableValue for WebSearchTable {}

impl Merge for WebSearchTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.api_key, n.api_key);
    }
}
