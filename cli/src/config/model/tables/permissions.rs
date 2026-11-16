//! `[permissions]`: pi's tool rules, with `[permissions.host]` / `[permissions.sandbox]` for one launch mode. Each file
//! contributes `[permissions]` and then the current runtime's table; rule lists combine across levels (deny and ask
//! beat allow when evaluated), and `default_mode` and `skill_grants` come from the nearest table that sets them.

use serde::{Deserialize, Serialize};

use super::{TableValue, nearer};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    /// Whether loaded skills' `allowed-tools` become allow rules. Off unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_grants: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "PermissionRules::is_empty")]
    pub host: PermissionRules,
    #[serde(default, skip_serializing_if = "PermissionRules::is_empty")]
    pub sandbox: PermissionRules,
}

/// Rules for one launch mode.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_grants: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Allow,
    Ask,
    Deny,
}

impl PermissionRules {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl TableValue for PermissionsTable {}

/// The rules in effect for one runtime: `[permissions]` and `[permissions.<runtime>]` of every file, merged.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PermissionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_grants: Option<bool>,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

impl PermissionSettings {
    /// Folds a nearer table's rules in: lists combine; `default_mode` and `skill_grants` from the nearest that sets
    /// them.
    pub fn merge(&mut self, n: PermissionSettings) {
        nearer(&mut self.default_mode, n.default_mode);
        nearer(&mut self.skill_grants, n.skill_grants);
        self.allow.extend(n.allow);
        self.ask.extend(n.ask);
        self.deny.extend(n.deny);
    }

    /// A file's `[permissions]`, without its runtime tables.
    pub fn shared(table: &PermissionsTable) -> Self {
        Self {
            default_mode: table.default_mode,
            skill_grants: table.skill_grants,
            allow: table.allow.clone(),
            ask: table.ask.clone(),
            deny: table.deny.clone(),
        }
    }
}

impl From<PermissionRules> for PermissionSettings {
    fn from(rules: PermissionRules) -> Self {
        let PermissionRules {
            default_mode,
            skill_grants,
            allow,
            ask,
            deny,
        } = rules;
        Self {
            default_mode,
            skill_grants,
            allow,
            ask,
            deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_combine_and_the_nearest_default_wins() {
        let mut base = PermissionSettings {
            default_mode: Some(PermissionMode::Ask),
            deny: vec!["bash(rm *)".into()],
            allow: vec!["read".into()],
            ..PermissionSettings::default()
        };
        base.merge(PermissionSettings {
            default_mode: Some(PermissionMode::Allow),
            ask: vec!["write".into()],
            ..PermissionSettings::default()
        });
        assert_eq!(base.default_mode, Some(PermissionMode::Allow));
        assert_eq!(base.deny, ["bash(rm *)"]);
        assert_eq!(base.ask, ["write"]);
        assert_eq!(base.allow, ["read"]);
        base.merge(PermissionSettings::default());
        assert_eq!(
            base.default_mode,
            Some(PermissionMode::Allow),
            "a table without default_mode keeps it"
        );
    }

    #[test]
    fn the_nearest_skill_grants_wins() {
        let mut base = PermissionSettings::default();
        assert_eq!(base.skill_grants, None, "off unless set");
        base.merge(PermissionSettings {
            skill_grants: Some(true),
            ..PermissionSettings::default()
        });
        base.merge(PermissionSettings::default());
        assert_eq!(
            base.skill_grants,
            Some(true),
            "a table without it keeps the farther one"
        );
        base.merge(PermissionSettings {
            skill_grants: Some(false),
            ..PermissionSettings::default()
        });
        assert_eq!(base.skill_grants, Some(false));
    }
}
