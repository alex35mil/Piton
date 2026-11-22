//! The tables of a project's `config.toml` (at every level of the chain), one file each: the table's type, where each
//! key may be set, the checks the schema alone can't express, and how it merges across levels.
//!
//! Values are optional so a nearer file can leave a key to a farther one; defaults belong to the code that uses a
//! value. Every table is strict: unknown keys are refused.

pub mod branches;
pub mod permissions;
pub mod sandbox;
pub mod web_search;
pub mod workers;

pub use self::{
    branches::BranchesTable,
    permissions::{PermissionSettings, PermissionsTable},
    sandbox::{
        AllGrant, ClaudeCodeOptions, ClaudeLogin, CommandGrant, CommandPattern, CommandRuntime, Connect, Grant,
        GrantLists, Listed, NetworkTarget, Preset, Profile, SandboxSettings, SandboxTable, ServiceEntry, ServiceName,
        Silence, SilenceTarget,
    },
    web_search::WebSearchTable,
    workers::WorkersTable,
};

/// The tables of `config.toml`. Each is parsed on its own, so a broken `[tracker]` never stops permission checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Sandbox,
    Workers,
    Permissions,
    Branches,
    Tracker,
    Forge,
    WebSearch,
}

impl Table {
    pub const ALL: [Table; 7] = [
        Table::Sandbox,
        Table::Workers,
        Table::Permissions,
        Table::Branches,
        Table::Tracker,
        Table::Forge,
        Table::WebSearch,
    ];

    /// The table's name in the file.
    pub fn name(self) -> &'static str {
        match self {
            Table::Sandbox => "sandbox",
            Table::Workers => "workers",
            Table::Permissions => "permissions",
            Table::Branches => "branches",
            Table::Tracker => "tracker",
            Table::Forge => "forge",
            Table::WebSearch => "web_search",
        }
    }

    /// The table named `key`.
    pub fn of(key: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|table| table.name() == key)
    }
}

/// Which levels may set a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Levels {
    Any,
    /// A project or parent folder's `.piton/`, never the global tier.
    Project,
}

/// Which file of a level's pair (`config.toml`, `config.local.toml`) may hold a key. A project's `config.toml` is
/// committed, so it's written by whoever commits to the repository; its `config.local.toml`, and both files of the
/// global tier, are the user's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Files {
    /// Either file, at every level.
    Either,
    /// `config.local.toml` only, at every level, the global tier included: a secret, kept out of any file that might
    /// be shared or synced (`web_search.api_key`).
    Local,
    /// Any file but a project's committed `config.toml`: a value only the user may choose for themselves, which
    /// needn't be secret (`branches.username`, and every knob that loosens the sandbox: a repository can't open it for
    /// whoever clones it).
    UsersOwn,
}

/// Where a key may be set. Fixed by piton, not configurable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub levels: Levels,
    pub files: Files,
}

impl Rule {
    pub const PROJECT: Rule = Rule {
        levels: Levels::Project,
        files: Files::Either,
    };
    pub const LOCAL: Rule = Rule {
        levels: Levels::Any,
        files: Files::Local,
    };
    pub const USERS_OWN: Rule = Rule {
        levels: Levels::Any,
        files: Files::UsersOwn,
    };
}

/// A table's value: the keys it sets with their rules, and the checks the schema can't express.
pub trait TableValue: serde::de::DeserializeOwned {
    /// The keys this value sets (dotted, from the top of the file) with a rule; every other key may be set in any file
    /// at any level.
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        Vec::new()
    }
    /// Values the schema accepts but piton can't use.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

/// How a table merges across levels, farthest first. `[permissions]` merges per runtime instead (see
/// [`PermissionSettings`]).
pub trait Merge: Default {
    /// Folds a nearer level's value into this one.
    fn merge(&mut self, nearer: Self);
}

/// `[tracker]` or `[forge]` as written: the pi package validates it. The nearest table replaces a farther one whole.
/// Project only: a global one would become the tracker of every repository that has none.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct Integration(pub toml::Table);

impl TableValue for Integration {}

impl Merge for Integration {
    fn merge(&mut self, nearer: Self) {
        *self = nearer;
    }
}

/// The keys of `keys` that are set, with their rule.
fn present(rule: Rule, keys: &[(&'static str, bool)]) -> Vec<(&'static str, Rule)> {
    keys.iter()
        .filter(|(_, set)| *set)
        .map(|(key, _)| (*key, rule))
        .collect()
}

/// Lists that only add: the base's entries, then the nearer's new ones.
fn union<T: PartialEq>(base: &mut Vec<T>, nearer: Vec<T>) {
    for item in nearer {
        if !base.contains(&item) {
            base.push(item);
        }
    }
}

/// A key the nearer level sets replaces the farther value.
pub(in crate::config::model) fn nearer<T>(base: &mut Option<T>, nearer: Option<T>) {
    if nearer.is_some() {
        *base = nearer;
    }
}

/// Grant paths: absolute, home-relative, or an environment variable the sandbox expands.
fn is_grant_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with("~/") || path == "~" || path.starts_with('$')
}
