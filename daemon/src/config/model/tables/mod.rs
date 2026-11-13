//! The tables of `config.toml`, one file each: the table's type, which keys are global- or local-only, the checks the
//! schema alone can't express, and how it merges across levels.
//!
//! Values are optional so a nearer file can leave a key to a farther one; defaults belong to the code that uses a
//! value. Every table is strict: unknown keys are refused.

pub mod branches;
pub mod client;
pub mod development;
pub mod host;
pub mod permissions;
pub mod run;
pub mod sandbox;
pub mod web_search;
pub mod workers;

pub use branches::BranchesTable;
pub use client::ClientTable;
pub use development::DevelopmentTable;
pub use host::HostTable;
pub use permissions::{PermissionSettings, PermissionsTable};
pub use run::RunTable;
pub use sandbox::{NetworkProfile, SandboxTable};
pub use web_search::WebSearchTable;
pub use workers::WorkersTable;

/// The parts of `config.toml`: each table, and the daemon's run keys together. Each part is parsed on its own, so a
/// broken `[tracker]` never stops permission checks and a typo in `[tailnet]` never stops a sandboxed launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Run,
    Sandbox,
    Workers,
    Host,
    Client,
    Development,
    Permissions,
    Branches,
    Tracker,
    Forge,
    WebSearch,
}

impl Table {
    pub const ALL: [Table; 11] = [
        Table::Run,
        Table::Sandbox,
        Table::Workers,
        Table::Host,
        Table::Client,
        Table::Development,
        Table::Permissions,
        Table::Branches,
        Table::Tracker,
        Table::Forge,
        Table::WebSearch,
    ];

    /// The top-level keys this part owns.
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Table::Run => run::KEYS,
            Table::Sandbox => &["sandbox"],
            Table::Workers => &["workers"],
            Table::Host => &["host"],
            Table::Client => &["client"],
            Table::Development => &["development"],
            Table::Permissions => &["permissions"],
            Table::Branches => &["branches"],
            Table::Tracker => &["tracker"],
            Table::Forge => &["forge"],
            Table::WebSearch => &["web_search"],
        }
    }

    /// The part owning a top-level key.
    pub fn of(key: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|table| table.keys().contains(&key))
    }
}

/// A table's value: where its keys may be set, and the checks the schema can't express.
pub trait TableValue: serde::de::DeserializeOwned {
    /// Keys a non-global level must not set (dotted, from the top of the file).
    fn global_only_keys(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// Personal keys a shared `config.toml` must not set.
    fn local_only_keys(&self) -> Vec<&'static str> {
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

/// `[tracker]` and `[forge]` as written: the pi package validates them. The nearest table replaces a farther one whole.
impl TableValue for toml::Table {}

impl Merge for toml::Table {
    fn merge(&mut self, nearer: Self) {
        *self = nearer;
    }
}

/// The keys of `keys` that are set.
fn present(keys: &[(&'static str, bool)]) -> Vec<&'static str> {
    keys.iter().filter(|(_, set)| *set).map(|(key, _)| *key).collect()
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
fn nearer<T>(base: &mut Option<T>, nearer: Option<T>) {
    if nearer.is_some() {
        *base = nearer;
    }
}

/// Grant paths: absolute, home-relative, or an environment variable the sandbox expands.
fn is_grant_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with("~/") || path == "~" || path.starts_with('$')
}
