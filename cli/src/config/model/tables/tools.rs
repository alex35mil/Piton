//! `[tools]`: how the agent's tools behave. Its only table is `[tools.permissions]` (see [`super::permissions`]).

use serde::{Deserialize, Deserializer, Serialize};

use super::{TableValue, permissions::PermissionsTable};
use crate::config::model::grants::{GrantEntry, GrantFilter, Grants};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsTable {
    #[serde(
        default,
        deserialize_with = "table",
        skip_serializing_if = "PermissionsTable::is_empty"
    )]
    pub permissions: PermissionsTable,
}

impl TableValue for ToolsTable {
    fn grants(&self) -> Grants {
        self.permissions.grants()
    }
    fn gated(self, filter: &GrantFilter) -> (Self, Vec<GrantEntry>) {
        let (permissions, out) = self.permissions.filtered(filter);
        (Self { permissions }, out)
    }
}

/// A value that must be written as a table: serde reads an empty array as an empty struct, so `permissions = []` would
/// otherwise pass as no rules.
fn table<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(deserializer: D) -> Result<T, D::Error> {
    let value = toml::Value::deserialize(deserializer)?;
    if !value.is_table() {
        return Err(serde::de::Error::custom("must be a table"));
    }
    value.try_into().map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ToolsTable, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn permissions_hold_the_rules() {
        let tools = parse("[permissions]\nallow = [\"read\"]\n[permissions.sandbox]\ndeny = [\"write\"]\n").unwrap();
        assert_eq!(tools.permissions.allow, ["read"]);
        assert_eq!(tools.permissions.sandbox.deny, ["write"]);
    }

    #[test]
    fn permissions_must_be_a_table() {
        for text in ["permissions = []\n", "permissions = 1\n", "permissions = \"allow\"\n"] {
            assert!(parse(text).is_err(), "{text}");
        }
        assert!(parse("other = 1\n").is_err(), "[tools] is strict");
        assert_eq!(parse("").unwrap(), ToolsTable::default());
    }
}
