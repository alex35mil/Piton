//! The tables of a project's `config.toml` (at every level of the chain), one file each: the table's type, where each
//! key may be set, the checks the schema alone can't express, and how it merges across levels.
//!
//! Values are optional so a nearer file can leave a key to a farther one; defaults belong to the code that uses a
//! value. Every table is strict: unknown keys are refused.

pub mod branches;
pub mod forge;
pub mod integration;
pub mod permissions;
pub mod sandbox;
pub mod tools;
pub mod tracker;
pub mod trust;
pub mod web_search;
pub mod workers;

use toml::{Spanned, de::DeValue};

pub use self::{
    branches::BranchesTable,
    forge::ForgeTable,
    integration::RoleTable,
    permissions::{PermissionMode, PermissionSettings},
    sandbox::{
        AllGrant, ClaudeCodeOptions, ClaudeLogin, CommandGrant, CommandPattern, CommandRuntime, Effect, FileAccess,
        Grant, GrantEntries, Listed, NetworkTarget, Origin, PathKey, Preset, Profile, SandboxSettings, SandboxTable,
        ServiceKey, ServiceName, Silence, SilenceTarget,
    },
    tools::ToolsTable,
    tracker::TrackerTable,
    trust::{Reconfirm, TrustTable},
    web_search::WebSearchTable,
    workers::{WorkerPermission, WorkerPermissions, WorkersTable},
};
use crate::config::model::key::Issue;

/// The tables of `config.toml`. Each is parsed on its own, so a broken `[tracker]` never stops permission checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Sandbox,
    Workers,
    Tools,
    Branches,
    Tracker,
    Forge,
    WebSearch,
    Trust,
}

impl Table {
    pub const ALL: [Table; 8] = [
        Table::Sandbox,
        Table::Workers,
        Table::Tools,
        Table::Branches,
        Table::Tracker,
        Table::Forge,
        Table::WebSearch,
        Table::Trust,
    ];

    /// The table's name in the file.
    pub fn name(self) -> &'static str {
        match self {
            Table::Sandbox => "sandbox",
            Table::Workers => "workers",
            Table::Tools => "tools",
            Table::Branches => "branches",
            Table::Tracker => "tracker",
            Table::Forge => "forge",
            Table::WebSearch => "web_search",
            Table::Trust => "trust",
        }
    }

    /// The table named `key`.
    pub fn of(key: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|table| table.name() == key)
    }

    /// Where the whole table may be set, checked before its contents are read: a table in the wrong file is refused
    /// for being there, not for what it's missing. `None`: any file, with its keys' own rules.
    pub fn rule(self) -> Option<Rule> {
        match self {
            // Project only: a global one would become the role of every repository that has none.
            Table::Tracker | Table::Forge => Some(Rule::PROJECT),
            Table::Sandbox | Table::Workers | Table::Tools | Table::Branches | Table::WebSearch | Table::Trust => None,
        }
    }
}

/// Which levels may set a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Levels {
    Any,
    /// A project or parent folder's `.piton/`, never the global tier.
    Project,
}

/// Which file of a level's pair (`config.toml`, `config.local.toml`) may hold a key. A repository's `config.toml` is
/// committed, so it's written by whoever commits to the repository; its `config.local.toml`, parent folders' files, and
/// both files of the global tier are the user's own. (What a committed file grants is gated by trust, not by these
/// rules: `crate::trust`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Files {
    /// Either file, at every level.
    Either,
    /// `config.local.toml` only, at every level, the global tier included: a secret, kept out of any file that might
    /// be shared or synced (`web_search.api_key`).
    Local,
    /// Any file but a repository's committed `config.toml`: a value only the user may choose for themselves, which
    /// needn't be secret (`branches.username`, `trust.reconfirm`).
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

/// Why a table's value doesn't match its schema, and where in the file: the byte range of the value or key at fault,
/// when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub message: String,
    pub span: Option<std::ops::Range<usize>>,
}

impl ReadError {
    pub fn at(span: std::ops::Range<usize>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
        }
    }
}

impl From<toml::de::Error> for ReadError {
    fn from(error: toml::de::Error) -> Self {
        Self {
            message: error.message().to_string(),
            span: error.span(),
        }
    }
}

/// A table's value: how it's read, the keys it sets with their rules, and the checks the schema can't express.
pub trait TableValue: serde::de::DeserializeOwned {
    /// The table from its value in a parsed file, with each error's position. A table that serde reads directly
    /// keeps the default.
    fn read(value: Spanned<DeValue<'_>>) -> Result<Self, ReadError> {
        // serde reads an empty array as an empty struct: a table key must hold a table.
        match value.get_ref() {
            DeValue::Table(_) => {}
            DeValue::String(_)
            | DeValue::Integer(_)
            | DeValue::Float(_)
            | DeValue::Boolean(_)
            | DeValue::Datetime(_)
            | DeValue::Array(_) => return Err(ReadError::at(value.span(), "must be a table")),
        }
        Self::deserialize(toml::de::ValueDeserializer::from(value)).map_err(ReadError::from)
    }
    /// The keys this value sets (dotted, from the top of the file) with a rule; every other key may be set in any file
    /// at any level.
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        Vec::new()
    }
    /// Values the schema accepts but piton can't use.
    fn check(&self) -> Result<(), Issue> {
        Ok(())
    }
    /// What this table grants, in a repository's committed file. Tables that grant nothing have none.
    fn grants(&self) -> crate::config::model::grants::Grants {
        crate::config::model::grants::Grants::new()
    }
    /// The table with only the grants `filter` keeps, and the grants left out.
    fn gated(
        self,
        _filter: &crate::config::model::grants::GrantFilter,
    ) -> (Self, Vec<crate::config::model::grants::GrantEntry>) {
        (self, Vec::new())
    }
}

/// How a table merges across levels, farthest first. `[tools.permissions]` merges per runtime instead (see
/// [`PermissionSettings`]).
pub trait Merge: Default {
    /// Folds a nearer level's value into this one.
    fn merge(&mut self, nearer: Self);
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
