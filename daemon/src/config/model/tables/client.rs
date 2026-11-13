//! `[client]`, global tier only: preferences the host serves to every client.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, nearer};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_font: Option<String>,
}

impl TableValue for ClientTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        vec!["client"]
    }
}

impl Merge for ClientTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.text_font, n.text_font);
        nearer(&mut self.code_font, n.code_font);
    }
}
