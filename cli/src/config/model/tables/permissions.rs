//! `[tools.permissions]`: pi's tool rules, with `[tools.permissions.host]` / `[tools.permissions.sandbox]` for one launch
//! mode. Each file contributes `[tools.permissions]` and then the current runtime's table; rule lists combine across
//! levels (deny and ask beat allow when evaluated), and `default_mode` and `skill_grants` come from the nearest table
//! that sets them.

use serde::{Deserialize, Serialize};

use super::nearer;
use crate::config::model::grants::{GrantEntry, GrantFilter, Grants};

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

impl PermissionsTable {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// What the table grants: `allow` rules, `default_mode = "allow"`, and `skill_grants = true`, in each of its three
    /// tables. `ask`, `deny`, and the stricter modes only restrict.
    pub fn grants(&self) -> Grants {
        let mut grants = Grants::new();
        for (key, rules) in self.tables() {
            grants.extend(rules.grants(key));
        }
        grants
    }

    /// The table with only the grants `filter` keeps; restrictions stay. Returns the grants left out.
    pub fn filtered(self, filter: &GrantFilter) -> (Self, Vec<GrantEntry>) {
        let PermissionsTable {
            default_mode,
            skill_grants,
            allow,
            ask,
            deny,
            host,
            sandbox,
        } = self;
        let mut out = Vec::new();
        let shared = PermissionRules {
            default_mode,
            skill_grants,
            allow,
            ask,
            deny,
        }
        .filtered("tools.permissions", filter, &mut out);
        let host = host.filtered("tools.permissions.host", filter, &mut out);
        let sandbox = sandbox.filtered("tools.permissions.sandbox", filter, &mut out);
        let PermissionRules {
            default_mode,
            skill_grants,
            allow,
            ask,
            deny,
        } = shared;
        (
            PermissionsTable {
                default_mode,
                skill_grants,
                allow,
                ask,
                deny,
                host,
                sandbox,
            },
            out,
        )
    }

    /// The three tables by their key, the shared one as rules.
    fn tables(&self) -> [(&'static str, PermissionRules); 3] {
        [
            (
                "tools.permissions",
                PermissionRules {
                    default_mode: self.default_mode,
                    skill_grants: self.skill_grants,
                    allow: self.allow.clone(),
                    ask: self.ask.clone(),
                    deny: self.deny.clone(),
                },
            ),
            ("tools.permissions.host", self.host.clone()),
            ("tools.permissions.sandbox", self.sandbox.clone()),
        ]
    }
}

fn mode_grant(key: &str, mode: PermissionMode) -> Option<GrantEntry> {
    match mode {
        PermissionMode::Allow => Some(GrantEntry::set(&format!("{key}.default_mode"), "allow")),
        PermissionMode::Ask | PermissionMode::Deny => None,
    }
}

fn skill_grants_grant(key: &str, on: bool) -> Option<GrantEntry> {
    match on {
        true => Some(GrantEntry::set(&format!("{key}.skill_grants"), true)),
        false => None,
    }
}

impl PermissionRules {
    fn grants(&self, key: &str) -> Grants {
        let mut grants: Grants = self
            .allow
            .iter()
            .map(|rule| GrantEntry::item(&format!("{key}.allow"), rule.as_str()))
            .collect();
        grants.extend(self.default_mode.and_then(|mode| mode_grant(key, mode)));
        grants.extend(self.skill_grants.and_then(|on| skill_grants_grant(key, on)));
        grants
    }

    fn filtered(mut self, key: &str, filter: &GrantFilter, out: &mut Vec<GrantEntry>) -> Self {
        let allow_key = format!("{key}.allow");
        filter.retain(&mut self.allow, |rule| GrantEntry::item(&allow_key, rule.as_str()), out);
        filter.retain_option(&mut self.default_mode, |mode| mode_grant(key, *mode), out);
        filter.retain_option(&mut self.skill_grants, |on| skill_grants_grant(key, *on), out);
        self
    }
}

/// The rules in effect for one runtime: `[tools.permissions]` and `[tools.permissions.<runtime>]` of every file, merged.
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

    /// A file's `[tools.permissions]`, without its runtime tables.
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
    fn grants_are_allow_rules_and_the_modes_that_open() {
        let table: PermissionsTable = toml::from_str(
            r#"
allow = ["read"]
deny = ["bash(rm *)"]
default_mode = "allow"
[sandbox]
allow = ["bash(git *)"]
skill_grants = true
[host]
default_mode = "ask"
skill_grants = false
"#,
        )
        .unwrap();
        let grants: Vec<String> = table.grants().into_iter().map(|g| g.to_string()).collect();
        assert_eq!(
            grants,
            [
                r#"tools.permissions.allow "read""#,
                r#"tools.permissions.default_mode = "allow""#,
                r#"tools.permissions.sandbox.allow "bash(git *)""#,
                "tools.permissions.sandbox.skill_grants = true",
            ]
        );
        // Nothing kept: the restrictions stay.
        let (none, out) = table.clone().filtered(&GrantFilter::None);
        assert_eq!(out.len(), 4);
        assert!(none.allow.is_empty() && none.sandbox.allow.is_empty());
        assert_eq!(none.default_mode, None);
        assert_eq!(none.deny, ["bash(rm *)"]);
        assert_eq!(none.host.default_mode, Some(PermissionMode::Ask));
        assert_eq!(none.host.skill_grants, Some(false));
        assert_eq!(none.sandbox.skill_grants, None);
        // Only what was trusted.
        let only = GrantFilter::Only([GrantEntry::item("tools.permissions.allow", "read")].into());
        let (some, out) = table.clone().filtered(&only);
        assert_eq!(some.allow, ["read"]);
        assert_eq!(out.len(), 3);
        assert_eq!(table.clone().filtered(&GrantFilter::All).0, table);
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
