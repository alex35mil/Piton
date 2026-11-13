//! `[development]`, global tier only: switches that apply only to a development install (the pi package running from a
//! git checkout of piton). Documented in docs/development.md.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, nearer};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentTable {
    /// A shipped Deliverable ends with an analysis of the session for workflow problems. Absent: on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_analysis: Option<bool>,
}

impl TableValue for DevelopmentTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        vec!["development"]
    }
}

impl Merge for DevelopmentTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.session_analysis, n.session_analysis);
    }
}
