//! `[workers]`: how `piton worker create` clones a source.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, union};
use crate::project::validate_relative;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersTable {
    /// Paths relative to the source not copied into a new worker. Union across levels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
}

impl TableValue for WorkersTable {
    fn check(&self) -> Result<(), String> {
        for skip in &self.skip {
            validate_relative(skip).map_err(|reason| format!("workers.skip {skip:?}: {reason}"))?;
        }
        Ok(())
    }
}

impl Merge for WorkersTable {
    fn merge(&mut self, n: Self) {
        union(&mut self.skip, n.skip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_stay_inside_the_source() {
        let workers = WorkersTable {
            skip: vec!["../x".into()],
        };
        assert!(workers.check().is_err());
    }
}
