//! The configuration file's schema, how it's read, and which keys a level may set.
//!
//! Each level has one `config.toml` and its `config.local.toml` (this machine's values, never shared). Every table of
//! the file is a part ([`Table`]) parsed on its own, so a broken `[tracker]` never stops permission checks and a typo
//! in `[tailnet]` never stops a sandboxed launch; the daemon's top-level run keys (`socket`, `log`, `[pi]`, `[env]`,
//! `[stop]`, `[tailnet]`) are one part. A syntax error or an unknown top-level key fails the whole file. Every table
//! is strict: unknown keys are refused. Values are optional so a nearer file can leave a key to a farther one; defaults
//! belong to the code that uses a value.
//!
//! MCP servers stay in `mcp[.local].json`, the format other harnesses read.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{LevelKind, Origin, SettingsError};
use crate::project::validate_relative;

/// The configuration file of a level.
pub const CONFIG_FILE: &str = "config.toml";
/// Its companion with this machine's values.
pub const CONFIG_LOCAL_FILE: &str = "config.local.toml";

/// Which file of a level's pair: the one that's shared (committed, or through dotfiles) or this machine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sharing {
    /// `config.toml`.
    Shared,
    /// `config.local.toml`.
    Local,
}

impl Sharing {
    pub fn file_name(self) -> &'static str {
        match self {
            Sharing::Shared => CONFIG_FILE,
            Sharing::Local => CONFIG_LOCAL_FILE,
        }
    }

    pub fn other(self) -> Sharing {
        match self {
            Sharing::Shared => Sharing::Local,
            Sharing::Local => Sharing::Shared,
        }
    }
}

/// A level's `config.toml` and `config.local.toml`, existing or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPair {
    pub shared: PathBuf,
    pub local: PathBuf,
}

impl ConfigPair {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            shared: dir.join(Sharing::Shared.file_name()),
            local: dir.join(Sharing::Local.file_name()),
        }
    }

    pub fn path(&self, sharing: Sharing) -> &Path {
        match sharing {
            Sharing::Shared => &self.shared,
            Sharing::Local => &self.local,
        }
    }
}

/// How the daemon runs: `socket`, `log`, `[pi]`, `[env]`, `[stop]`, `[tailnet]`. Global tier only.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(default, skip_serializing_if = "PiTable::is_empty")]
    pub pi: PiTable,
    #[serde(default, skip_serializing_if = "EnvTable::is_empty")]
    pub env: EnvTable,
    #[serde(default, skip_serializing_if = "StopTable::is_empty")]
    pub stop: StopTable,
    #[serde(default, skip_serializing_if = "TailnetTable::is_empty")]
    pub tailnet: TailnetTable,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PiTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_shell: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mise: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StopTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_grace_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TailnetTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_ms: Option<u64>,
}

/// `[sandbox]`: grants for sandboxed pis. Grant lists only add across levels; the base profile, extra groups,
/// bypasses, the nono command, and the network profile belong to the global tier.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxTable {
    /// nono executable and leading arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// The backend's built-in base profile (nono: `default`, `mise-dev`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Extra built-in groups of the backend (nono: `git_config`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Read and write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_file: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_file: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Lifts a deny of the base profile for a path granted above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unix_socket: Vec<String>,
    #[serde(default, skip_serializing_if = "NetworkTable::is_empty")]
    pub network: NetworkTable,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTable {
    /// Localhost ports the sandbox may connect to and listen on (`0`: every port, macOS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_port: Vec<u16>,
    /// The proxy's network profile: a name, or `false` for no proxy filtering. Absent: the base profile's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<NetworkProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum NetworkProfile {
    Named(String),
    /// Only `false` is accepted: no proxy filtering.
    Off(bool),
}

/// `[workers]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersTable {
    /// Paths relative to the source not copied into a new worker. Union across levels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
}

/// `[host]`, global tier only: this host's preferences the apps change.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostTable {
    /// Where the apps may browse and start sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorites: Option<Vec<String>>,
    /// Host logging default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<crate::diagnostics::Mode>,
    /// Mode for `piton attach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_mode: Option<crate::wire::LaunchMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_grace_ms: Option<u64>,
}

/// `[client]`, global tier only: preferences the host serves to every client.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_font: Option<String>,
}

/// `[development]`, global tier only: switches that apply only to a development install (the pi package running from a
/// git checkout of piton). Documented in docs/development.md.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentTable {
    /// A shipped Deliverable ends with an analysis of the session for workflow problems. Absent: on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_analysis: Option<bool>,
}

/// `[permissions]`: pi's tool rules, and `[permissions.host]` / `[permissions.sandbox]` for one launch mode.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Keybindings::is_empty")]
    pub keybindings: Keybindings,
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

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Keybindings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_accept_edits: Option<String>,
}

/// `mcp[.local].json`. Server entries are passed to pi unchanged; pi validates them. File order is kept
/// (serde_json's `preserve_order`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpFile {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: serde_json::Map<String, serde_json::Value>,
    #[serde(rename = "autoEnableCodemode", default, skip_serializing_if = "Option::is_none")]
    pub auto_enable_codemode: Option<bool>,
}

/// `[web_search]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// `[branches]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchesTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<BranchFormat>,
    /// Personal: `config.local.toml` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum BranchFormat {
    #[serde(rename = "identifier-title")]
    IdentifierTitle,
    #[serde(rename = "username/identifier-title")]
    UsernameIdentifierTitle,
    #[serde(rename = "tracker")]
    Tracker,
}

macro_rules! is_empty {
    ($($ty:ty),*) => {$(
        impl $ty {
            pub fn is_empty(&self) -> bool {
                *self == Self::default()
            }
        }
    )*};
}
is_empty!(
    PiTable,
    EnvTable,
    StopTable,
    TailnetTable,
    NetworkTable,
    PermissionRules,
    Keybindings
);

/// The parts of `config.toml`: each table, and the daemon's run keys together. Each part is parsed on its own.
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
            Table::Run => &["socket", "log", "pi", "env", "stop", "tailnet"],
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

    fn of(key: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|table| table.keys().contains(&key))
    }
}

/// A part's value: where its keys may be set, and the checks the schema alone can't express.
pub trait PartValue: serde::de::DeserializeOwned {
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

fn present(keys: &[(&'static str, bool)]) -> Vec<&'static str> {
    keys.iter().filter(|(_, set)| *set).map(|(key, _)| *key).collect()
}

impl PartValue for RunTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[
            ("socket", self.socket.is_some()),
            ("log", self.log.is_some()),
            ("pi", !self.pi.is_empty()),
            ("env", !self.env.is_empty()),
            ("stop", !self.stop.is_empty()),
            ("tailnet", !self.tailnet.is_empty()),
        ])
    }

    fn check(&self) -> Result<(), String> {
        match &self.pi.command {
            Some(command) if command.is_empty() => Err("pi.command must not be empty".into()),
            Some(_) | None => Ok(()),
        }
    }
}

impl PartValue for SandboxTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[
            ("sandbox.command", self.command.is_some()),
            ("sandbox.base", self.base.is_some()),
            ("sandbox.groups", !self.groups.is_empty()),
            ("sandbox.bypass", !self.bypass.is_empty()),
            ("sandbox.network.profile", self.network.profile.is_some()),
        ])
    }

    fn check(&self) -> Result<(), String> {
        if self.command.as_ref().is_some_and(Vec::is_empty) {
            return Err("sandbox.command must not be empty".into());
        }
        for (key, paths) in [
            ("allow", &self.allow),
            ("read", &self.read),
            ("read_file", &self.read_file),
            ("allow_file", &self.allow_file),
            ("deny", &self.deny),
            ("bypass", &self.bypass),
            ("unix_socket", &self.unix_socket),
        ] {
            if let Some(grant) = paths.iter().find(|grant| !is_grant_path(grant)) {
                return Err(format!(
                    "sandbox.{key} {grant:?}: must be absolute, or start with `~/` or `$`"
                ));
            }
        }
        match self.network.profile {
            Some(NetworkProfile::Off(true)) => {
                Err("sandbox.network.profile: a profile name, or `false` for no filtering".into())
            }
            Some(NetworkProfile::Off(false) | NetworkProfile::Named(_)) | None => Ok(()),
        }
    }
}

impl PartValue for WorkersTable {
    fn check(&self) -> Result<(), String> {
        for skip in &self.skip {
            validate_relative(skip).map_err(|reason| format!("workers.skip {skip:?}: {reason}"))?;
        }
        Ok(())
    }
}

// Global-only tables: a part exists only when a file sets its table, so setting it at all outside the global tier is
// refused.
impl PartValue for HostTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        vec!["host"]
    }
}

impl PartValue for ClientTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        vec!["client"]
    }
}

impl PartValue for DevelopmentTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        vec!["development"]
    }
}

impl PartValue for PermissionsTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[("permissions.keybindings", !self.keybindings.is_empty())])
    }
}

impl PartValue for BranchesTable {
    fn local_only_keys(&self) -> Vec<&'static str> {
        present(&[("branches.username", self.username.is_some())])
    }
}

impl PartValue for WebSearchTable {}

/// `[tracker]` and `[forge]` as written: the pi package validates them.
impl PartValue for toml::Table {}

/// One `config[.local].toml`, syntax-checked, its top-level keys known, ready to read part by part.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    pub origin: Origin,
    pub sharing: Sharing,
    table: toml::Table,
}

impl ConfigDoc {
    /// Parses `text` as the file at `path`: TOML, and only top-level keys a part owns.
    pub fn parse(text: &str, path: &Path, level: LevelKind, sharing: Sharing) -> Result<Self, SettingsError> {
        let invalid = |reason: String| SettingsError::Invalid {
            path: path.to_path_buf(),
            reason,
        };
        let table: toml::Table = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if let Some(key) = table.keys().find(|key| Table::of(key).is_none()) {
            return Err(invalid(format!("unknown key `{key}`")));
        }
        Ok(Self {
            origin: Origin {
                path: path.to_path_buf(),
                level,
            },
            sharing,
            table,
        })
    }

    /// One part of the file, strictly, with the level's and the file's scope checked; `None` when the file doesn't
    /// set it.
    pub fn part<T: PartValue>(&self, table: Table) -> Result<Option<T>, SettingsError> {
        let path = &self.origin.path;
        let invalid = |reason: String| SettingsError::Invalid {
            path: path.clone(),
            reason,
        };
        let value = match table {
            // The run keys sit at the top of the file: the part is their subset.
            Table::Run => {
                let subset: toml::Table = self
                    .table
                    .iter()
                    .filter(|(key, _)| table.keys().contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                if subset.is_empty() {
                    return Ok(None);
                }
                // serde reads an empty array as an empty struct: a key that names a table must hold one.
                if let Some((key, _)) = subset
                    .iter()
                    .find(|(key, value)| !matches!(key.as_str(), "socket" | "log") && !value.is_table())
                {
                    return Err(invalid(format!("`{key}` must be a table")));
                }
                toml::Value::Table(subset)
            }
            Table::Sandbox
            | Table::Workers
            | Table::Host
            | Table::Client
            | Table::Development
            | Table::Permissions
            | Table::Branches
            | Table::Tracker
            | Table::Forge
            | Table::WebSearch => {
                let key = table.keys()[0];
                let Some(value) = self.table.get(key) else {
                    return Ok(None);
                };
                if !value.is_table() {
                    return Err(invalid(format!("`{key}` must be a table")));
                }
                value.clone()
            }
        };
        let value: T = value.try_into().map_err(|e: toml::de::Error| {
            invalid(match table {
                Table::Run => e.to_string(),
                Table::Sandbox
                | Table::Workers
                | Table::Host
                | Table::Client
                | Table::Development
                | Table::Permissions
                | Table::Branches
                | Table::Tracker
                | Table::Forge
                | Table::WebSearch => format!("[{}] {e}", table.keys()[0]),
            })
        })?;
        value.check().map_err(invalid)?;
        if self.origin.level != LevelKind::Global
            && let Some(key) = value.global_only_keys().first()
        {
            return Err(SettingsError::GlobalOnly {
                path: path.clone(),
                key: (*key).to_string(),
                global: super::global_tier(),
            });
        }
        match self.sharing {
            Sharing::Local => {}
            Sharing::Shared => {
                if let Some(key) = value.local_only_keys().first() {
                    return Err(SettingsError::LocalOnly {
                        path: path.clone(),
                        key: (*key).to_string(),
                    });
                }
            }
        }
        Ok(Some(value))
    }
}

/// Reads one level's file; `None` when it doesn't exist.
pub fn read_config(path: &Path, level: LevelKind, sharing: Sharing) -> Result<Option<ConfigDoc>, SettingsError> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    ConfigDoc::parse(&text, path, level, sharing).map(Some)
}

/// A file's text, or `None` when it doesn't exist. A folder or an unreadable file is an error.
pub fn read_text(path: &Path) -> Result<Option<String>, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Err(SettingsError::Invalid {
            path: path.to_path_buf(),
            reason: "must be a file".into(),
        }),
        Err(source) => Err(SettingsError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads `mcp[.local].json`; `None` when it doesn't exist.
pub fn load_mcp(path: &Path) -> Result<Option<McpFile>, SettingsError> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    let invalid = |reason: String| SettingsError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    // serde reads a JSON array into a struct too; the file must be an object.
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    if !value.is_object() {
        return Err(invalid("must be a JSON object".into()));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| invalid(e.to_string()))
}

/// Every part of a file, checked: for a write that must leave a file every reader accepts.
pub fn validate_config(doc: &ConfigDoc) -> Result<(), SettingsError> {
    for table in Table::ALL {
        match table {
            Table::Run => doc.part::<RunTable>(table).map(drop_value)?,
            Table::Sandbox => doc.part::<SandboxTable>(table).map(drop_value)?,
            Table::Workers => doc.part::<WorkersTable>(table).map(drop_value)?,
            Table::Host => doc.part::<HostTable>(table).map(drop_value)?,
            Table::Client => doc.part::<ClientTable>(table).map(drop_value)?,
            Table::Development => doc.part::<DevelopmentTable>(table).map(drop_value)?,
            Table::Permissions => doc.part::<PermissionsTable>(table).map(drop_value)?,
            Table::Branches => doc.part::<BranchesTable>(table).map(drop_value)?,
            Table::Tracker | Table::Forge => doc.part::<toml::Table>(table).map(drop_value)?,
            Table::WebSearch => doc.part::<WebSearchTable>(table).map(drop_value)?,
        }
    }
    Ok(())
}

fn drop_value<T>(_: Option<T>) {}

fn is_grant_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with("~/") || path == "~" || path.starts_with('$')
}

/// Expands a leading `~` or `$HOME` with the user's home folder (from the user database, not the environment a
/// repository can set). Other `$VARS` are left for the sandbox backend to expand.
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" || path == "$HOME" {
        return home.to_path_buf();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("$HOME/")) {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str, level: LevelKind, sharing: Sharing) -> Result<ConfigDoc, SettingsError> {
        ConfigDoc::parse(text, Path::new("config.toml"), level, sharing)
    }

    #[test]
    fn unknown_keys_are_refused() {
        let err = doc("[permisions]\nallow = []\n", LevelKind::Repository, Sharing::Shared).unwrap_err();
        assert!(err.to_string().contains("permisions"), "{err}");
        let d = doc("[sandbox]\nallw = []\n", LevelKind::Repository, Sharing::Shared).unwrap();
        let err = d.part::<SandboxTable>(Table::Sandbox).unwrap_err();
        assert!(err.to_string().contains("allw"), "{err}");
        let d = doc(
            "[permissions.host]\nkeybindings = {}\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<PermissionsTable>(Table::Permissions).is_err());
        // A table key holding anything else, even an empty list (which serde reads as an empty table), is refused.
        let d = doc("permissions = []\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<PermissionsTable>(Table::Permissions).is_err());
        let d = doc("tracker = 1\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<toml::Table>(Table::Tracker).is_err());
        let d = doc("stop = []\n", LevelKind::Global, Sharing::Shared).unwrap();
        assert!(d.part::<RunTable>(Table::Run).is_err());
    }

    /// A broken table fails only its part: a typo in `[tailnet]` leaves `[sandbox]` readable, and a bad `[client]`
    /// leaves `[host]`.
    #[test]
    fn parts_are_parsed_on_their_own() {
        let d = doc(
            "[tracker]\nprovider = 1\n[branches]\nformt = \"x\"\n[permissions]\nallow = [\"read\"]\n[tailnet]\nprot = 1\n[sandbox]\nread = [\"/r\"]\n[client]\ntext_font = 1\n[host]\nroots = [\"~/p\"]\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<BranchesTable>(Table::Branches).is_err());
        assert!(d.part::<RunTable>(Table::Run).is_err());
        assert!(d.part::<ClientTable>(Table::Client).is_err());
        assert_eq!(
            d.part::<PermissionsTable>(Table::Permissions).unwrap().unwrap().allow,
            ["read"]
        );
        assert_eq!(d.part::<SandboxTable>(Table::Sandbox).unwrap().unwrap().read, ["/r"]);
        assert!(d.part::<HostTable>(Table::Host).unwrap().is_some());
        assert!(d.part::<toml::Table>(Table::Tracker).unwrap().is_some());
        assert!(d.part::<toml::Table>(Table::Forge).unwrap().is_none());
    }

    #[test]
    fn global_only_and_local_only_keys() {
        let d = doc("[sandbox]\nbase = \"default\"\n", LevelKind::Parent, Sharing::Shared).unwrap();
        let err = d.part::<SandboxTable>(Table::Sandbox).unwrap_err();
        assert!(matches!(err, SettingsError::GlobalOnly { ref key, .. } if key == "sandbox.base"));
        let refused = |text: &str, table: Table| {
            let d = doc(text, LevelKind::Repository, Sharing::Local).unwrap();
            match table {
                Table::Host => d.part::<HostTable>(table).is_err(),
                Table::Development => d.part::<DevelopmentTable>(table).is_err(),
                Table::Permissions => d.part::<PermissionsTable>(table).is_err(),
                Table::Run => d.part::<RunTable>(table).is_err(),
                Table::Sandbox
                | Table::Workers
                | Table::Client
                | Table::Branches
                | Table::Tracker
                | Table::Forge
                | Table::WebSearch => false,
            }
        };
        for (text, table) in [
            ("[host]\nroots = []\n", Table::Host),
            ("[development]\nsession_analysis = true\n", Table::Development),
            (
                "[permissions.keybindings]\nauto_accept_edits = \"ctrl+a\"\n",
                Table::Permissions,
            ),
            ("[tailnet]\nport = 1\n", Table::Run),
        ] {
            assert!(refused(text, table), "{text}");
            assert!(validate_config(&doc(text, LevelKind::Global, Sharing::Shared).unwrap()).is_ok());
        }

        let text = "[branches]\nusername = \"me\"\n";
        let err = doc(text, LevelKind::Global, Sharing::Shared)
            .unwrap()
            .part::<BranchesTable>(Table::Branches)
            .unwrap_err();
        assert!(matches!(err, SettingsError::LocalOnly { .. }), "{err}");
        assert!(
            doc(text, LevelKind::Repository, Sharing::Local)
                .unwrap()
                .part::<BranchesTable>(Table::Branches)
                .is_ok()
        );
    }

    #[test]
    fn missing_files_are_none_and_folders_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        assert!(
            read_config(&path, LevelKind::Global, Sharing::Shared)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(&path).unwrap();
        assert!(read_config(&path, LevelKind::Global, Sharing::Shared).is_err());
        assert!(load_mcp(&dir.path().join("mcp.json")).unwrap().is_none());
        std::fs::write(dir.path().join("mcp.json"), "[]").unwrap();
        assert!(load_mcp(&dir.path().join("mcp.json")).is_err());
    }

    #[test]
    fn values_are_checked() {
        let mut sandbox = SandboxTable {
            allow: vec!["relative/path".into()],
            ..SandboxTable::default()
        };
        assert!(sandbox.check().is_err());
        sandbox.allow = vec!["~/x".into(), "/abs".into(), "$SSH_AUTH_SOCK".into()];
        assert!(sandbox.check().is_ok());
        sandbox.network.profile = Some(NetworkProfile::Off(true));
        assert!(sandbox.check().is_err());
        sandbox.network.profile = Some(NetworkProfile::Off(false));
        assert!(sandbox.check().is_ok());
        let workers = WorkersTable {
            skip: vec!["../x".into()],
        };
        assert!(workers.check().is_err());
        let run = RunTable {
            pi: PiTable {
                command: Some(Vec::new()),
                args: None,
            },
            ..RunTable::default()
        };
        assert!(run.check().is_err());
        let toml: SandboxTable = toml::from_str("[network]\nprofile = false\n").unwrap();
        assert_eq!(toml.network.profile, Some(NetworkProfile::Off(false)));
    }

    #[test]
    fn home_expansion() {
        let home = Path::new("/Users/a");
        assert_eq!(expand_home("~/x", home), PathBuf::from("/Users/a/x"));
        assert_eq!(expand_home("$HOME/x", home), PathBuf::from("/Users/a/x"));
        assert_eq!(expand_home("~", home), PathBuf::from("/Users/a"));
        assert_eq!(expand_home("/abs", home), PathBuf::from("/abs"));
        assert_eq!(expand_home("$SSH_AUTH_SOCK", home), PathBuf::from("$SSH_AUTH_SOCK"));
    }
}
