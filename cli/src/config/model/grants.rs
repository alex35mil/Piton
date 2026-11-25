//! What a repository's committed `config.toml` grants, and which of it applies.
//!
//! Every entry of that file is a restriction (always applies) or a grant (applies only once the user trusts the
//! repository, `crate::trust`). A grant is named by one canonical line of text, the same each time the file is read:
//! what the trust question shows, what the state database stores, and what `reconfirm = "on-change"` compares.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// One granting entry, as one line: `sandbox.filesystem."~/.cargo" = "write"`, `tools.permissions.allow "read"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantEntry(String);

impl GrantEntry {
    /// `<key> = <value>`: a key that holds one value (a map entry, a boolean, the profile).
    pub fn set(key: &str, value: impl Into<toml::Value>) -> Self {
        Self(format!("{key} = {}", value.into()))
    }

    /// `<key> <item>`: one item of a list (a preset, a rule, a command entry).
    pub fn item(key: &str, item: impl Into<toml::Value>) -> Self {
        Self(format!("{key} {}", item.into()))
    }

    /// `<table>.<quoted key> = <value>`: one entry of a map, its key always quoted.
    pub fn entry(table: &str, key: &str, value: &str) -> Self {
        Self::set(&format!("{table}.{}", toml::Value::String(key.into())), value)
    }

    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GrantEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A repository's grants, sorted.
pub type Grants = BTreeSet<GrantEntry>;

/// Which of a repository's grants a resolution keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "grants", rename_all = "snake_case")]
pub enum GrantFilter {
    /// Every grant: the repository is trusted, and `reconfirm = "never"`.
    All,
    /// Only these: the grants the user trusted (`reconfirm = "on-change"`), or none at all when nothing was trusted
    /// yet (an empty set, so a grant committed later stays out).
    Only(Grants),
    /// No grant: the repository was declined, or isn't decided.
    None,
}

impl GrantFilter {
    pub fn keeps(&self, grant: &GrantEntry) -> bool {
        match self {
            GrantFilter::All => true,
            GrantFilter::Only(grants) => grants.contains(grant),
            GrantFilter::None => false,
        }
    }

    /// Keeps the items of `items` this filter keeps, adding the rest to `left_out`.
    pub(in crate::config) fn retain<T>(
        &self,
        items: &mut Vec<T>,
        name: impl Fn(&T) -> GrantEntry,
        left_out: &mut Vec<GrantEntry>,
    ) {
        items.retain(|item| {
            let grant = name(item);
            let keep = self.keeps(&grant);
            if !keep {
                left_out.push(grant);
            }
            keep
        });
    }

    /// Keeps an optional value this filter keeps (or that isn't a grant), adding a dropped one to `left_out`.
    pub(in crate::config) fn retain_option<T>(
        &self,
        value: &mut Option<T>,
        name: impl Fn(&T) -> Option<GrantEntry>,
        left_out: &mut Vec<GrantEntry>,
    ) {
        if let Some(grant) = value.as_ref().and_then(&name)
            && !self.keeps(&grant)
        {
            left_out.push(grant);
            *value = None;
        }
    }

    /// Keeps the map entries this filter keeps (or that aren't grants), adding the rest to `left_out`.
    pub(in crate::config) fn retain_map<K: Ord, V>(
        &self,
        map: &mut std::collections::BTreeMap<K, V>,
        name: impl Fn(&K, &V) -> Option<GrantEntry>,
        left_out: &mut Vec<GrantEntry>,
    ) {
        map.retain(|key, value| match name(key, value) {
            Some(grant) if !self.keeps(&grant) => {
                left_out.push(grant);
                false
            }
            Some(_) | None => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_one_stable_line() {
        assert_eq!(
            GrantEntry::entry("sandbox.filesystem", "~/.cargo", "write").as_str(),
            "sandbox.filesystem.\"~/.cargo\" = \"write\""
        );
        assert_eq!(
            GrantEntry::set("sandbox.allow_tcp_listening", true).as_str(),
            "sandbox.allow_tcp_listening = true"
        );
        assert_eq!(
            GrantEntry::item("tools.permissions.allow", "bash(git *)").as_str(),
            "tools.permissions.allow \"bash(git *)\""
        );
    }

    #[test]
    fn filters_keep_what_they_name() {
        let a = GrantEntry::item("sandbox.presets", "rust");
        let b = GrantEntry::item("sandbox.presets", "swift");
        assert!(GrantFilter::All.keeps(&a));
        assert!(!GrantFilter::None.keeps(&a));
        let only = GrantFilter::Only([a.clone()].into());
        assert!(only.keeps(&a));
        assert!(!only.keeps(&b));
        assert!(!GrantFilter::Only(Grants::new()).keeps(&a));
        let mut items = vec!["rust", "swift"];
        let mut left_out = Vec::new();
        only.retain(&mut items, |p| GrantEntry::item("sandbox.presets", *p), &mut left_out);
        assert_eq!(items, ["rust"]);
        assert_eq!(left_out, [b]);
    }

    #[test]
    fn filters_round_trip_as_json() {
        for filter in [
            GrantFilter::All,
            GrantFilter::None,
            GrantFilter::Only([GrantEntry::item("sandbox.presets", "rust")].into()),
        ] {
            let json = serde_json::to_string(&filter).unwrap();
            assert_eq!(serde_json::from_str::<GrantFilter>(&json).unwrap(), filter, "{json}");
        }
    }
}
