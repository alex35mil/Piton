//! The configuration file's schema, how it's read, and which keys a level may set.
//!
//! Each level has one `config.toml` and its `config.local.toml` (this machine's values, never shared). The file's
//! top-level keys belong to parts ([`Table`]), each parsed on its own, so a broken `[tracker]` never stops permission
//! checks; a syntax error or an unknown top-level key fails the whole file. Every table is strict: unknown keys are
//! refused. Values are optional so a nearer file can leave a key to a farther one; defaults belong to the code that uses
//! a value.
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

/// How the daemon runs (global tier only) and per-project `[sandbox]` and `[workers]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonFile {
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
    #[serde(default, skip_serializing_if = "SandboxTable::is_empty")]
    pub sandbox: SandboxTable,
    #[serde(default, skip_serializing_if = "WorkersTable::is_empty")]
    pub workers: WorkersTable,
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

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersTable {
    /// Paths relative to the source not copied into a new worker. Union across levels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
}

/// `[host]` and `[client]`, global tier only: preferences the apps can change.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsFile {
    #[serde(default, skip_serializing_if = "HostPrefs::is_empty")]
    pub host: HostPrefs,
    #[serde(default, skip_serializing_if = "ClientPrefs::is_empty")]
    pub client: ClientPrefs,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostPrefs {
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

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPrefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_font: Option<String>,
}

/// `[development]`, global tier only: switches that apply only to a development install (the pi package running from a
/// git checkout of piton). Documented in docs/development.md.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentFile {
    #[serde(default, skip_serializing_if = "DevelopmentTable::is_empty")]
    pub development: DevelopmentTable,
}

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
pub struct PermissionsFile {
    #[serde(default, skip_serializing_if = "PermissionsTable::is_empty")]
    pub permissions: PermissionsTable,
}

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
pub struct WebSearchFile {
    #[serde(default, skip_serializing_if = "WebSearchTable::is_empty")]
    pub web_search: WebSearchTable,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// `[branches]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchesFile {
    #[serde(default, skip_serializing_if = "Branches::is_empty")]
    pub branches: Branches,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Branches {
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

/// `[tracker]` and `[forge]`, unchanged: the pi package validates them. A nearer level's table replaces a farther one's
/// whole.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<toml::Table>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<toml::Table>,
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
    SandboxTable,
    NetworkTable,
    WorkersTable,
    HostPrefs,
    ClientPrefs,
    DevelopmentTable,
    PermissionsTable,
    PermissionRules,
    Keybindings,
    WebSearchTable,
    Branches
);

/// The parts of `config.toml`, by the top-level keys each owns. Each part is parsed on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Daemon,
    Settings,
    Development,
    Permissions,
    Branches,
    Integrations,
    WebSearch,
}

impl Table {
    pub const ALL: [Table; 7] = [
        Table::Daemon,
        Table::Settings,
        Table::Development,
        Table::Permissions,
        Table::Branches,
        Table::Integrations,
        Table::WebSearch,
    ];

    /// The top-level keys this part owns.
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Table::Daemon => &["socket", "log", "pi", "env", "stop", "tailnet", "sandbox", "workers"],
            Table::Settings => &["host", "client"],
            Table::Development => &["development"],
            Table::Permissions => &["permissions"],
            Table::Branches => &["branches"],
            Table::Integrations => &["tracker", "forge"],
            Table::WebSearch => &["web_search"],
        }
    }

    fn of(key: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|table| table.keys().contains(&key))
    }
}

/// Top-level keys that hold a value, not a table.
const SCALAR_KEYS: &[&str] = &["socket", "log"];

/// Where a key may be set.
pub trait Scoped {
    /// Keys a non-global level must not set.
    fn global_only_keys(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// Personal keys a shared `config.toml` must not set.
    fn local_only_keys(&self) -> Vec<&'static str> {
        Vec::new()
    }
}

fn present(keys: &[(&'static str, bool)]) -> Vec<&'static str> {
    keys.iter().filter(|(_, set)| *set).map(|(key, _)| *key).collect()
}

impl Scoped for DaemonFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[
            ("socket", self.socket.is_some()),
            ("log", self.log.is_some()),
            ("pi", !self.pi.is_empty()),
            ("env", !self.env.is_empty()),
            ("stop", !self.stop.is_empty()),
            ("tailnet", !self.tailnet.is_empty()),
            ("sandbox.command", self.sandbox.command.is_some()),
            ("sandbox.base", self.sandbox.base.is_some()),
            ("sandbox.groups", !self.sandbox.groups.is_empty()),
            ("sandbox.bypass", !self.sandbox.bypass.is_empty()),
            ("sandbox.network.profile", self.sandbox.network.profile.is_some()),
        ])
    }
}

impl Scoped for SettingsFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[("host", !self.host.is_empty()), ("client", !self.client.is_empty())])
    }
}

impl Scoped for DevelopmentFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[("development", !self.development.is_empty())])
    }
}

impl Scoped for PermissionsFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[("permissions.keybindings", !self.permissions.keybindings.is_empty())])
    }
}

impl Scoped for BranchesFile {
    fn local_only_keys(&self) -> Vec<&'static str> {
        present(&[("branches.username", self.branches.username.is_some())])
    }
}

impl Scoped for IntegrationsFile {}
impl Scoped for WebSearchFile {}
impl Scoped for McpFile {}

/// A file that was read, with where it came from.
#[derive(Debug, Clone)]
pub struct Loaded<T> {
    pub value: T,
    pub origin: Origin,
}

/// Which file of a level's pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    /// `config.toml`: committed, or shared through dotfiles.
    Shared,
    /// `config.local.toml`: this machine's values.
    Local,
}

impl Sharing {
    pub fn file_name(self) -> &'static str {
        match self {
            Sharing::Shared => CONFIG_FILE,
            Sharing::Local => CONFIG_LOCAL_FILE,
        }
    }
}

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

    /// Whether the file sets any key of `table`.
    pub fn has(&self, table: Table) -> bool {
        table.keys().iter().any(|key| self.table.contains_key(*key))
    }

    /// One part of the file, strictly, with the level's and the file's scope checked.
    pub fn part<T>(&self, table: Table) -> Result<T, SettingsError>
    where
        T: serde::de::DeserializeOwned + Scoped,
    {
        let path = &self.origin.path;
        let subset: toml::Table = self
            .table
            .iter()
            .filter(|(key, _)| table.keys().contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        // serde reads an empty array as an empty struct: a key that names a table must hold one.
        if let Some((key, _)) = subset
            .iter()
            .find(|(key, value)| !SCALAR_KEYS.contains(&key.as_str()) && !value.is_table())
        {
            return Err(SettingsError::Invalid {
                path: path.clone(),
                reason: format!("`{key}` must be a table"),
            });
        }
        let value: T = toml::Value::Table(subset)
            .try_into()
            .map_err(|e: toml::de::Error| SettingsError::Invalid {
                path: path.clone(),
                reason: e.to_string(),
            })?;
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
        Ok(value)
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

/// Reads `mcp[.local].json` at one level.
pub fn load_mcp(path: &Path, level: LevelKind) -> Result<Option<Loaded<McpFile>>, SettingsError> {
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
    let value = serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
    Ok(Some(Loaded {
        value,
        origin: Origin {
            path: path.to_path_buf(),
            level,
        },
    }))
}

/// Checks values that the schema alone can't: non-empty commands, relative skip paths, absolute grant paths, and the
/// network profile's `false`.
pub fn validate_daemon(file: &DaemonFile, path: &Path) -> Result<(), SettingsError> {
    let invalid = |reason: String| SettingsError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    if file.pi.command.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("pi.command must not be empty".into()));
    }
    if file.sandbox.command.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("sandbox.command must not be empty".into()));
    }
    for skip in &file.workers.skip {
        validate_relative(skip).map_err(|reason| invalid(format!("workers.skip {skip:?}: {reason}")))?;
    }
    let s = &file.sandbox;
    for (key, paths) in [
        ("allow", &s.allow),
        ("read", &s.read),
        ("read_file", &s.read_file),
        ("allow_file", &s.allow_file),
        ("deny", &s.deny),
        ("bypass", &s.bypass),
        ("unix_socket", &s.unix_socket),
    ] {
        for grant in paths {
            if !is_grant_path(grant) {
                return Err(invalid(format!(
                    "sandbox.{key} {grant:?}: must be absolute, or start with `~/` or `$`"
                )));
            }
        }
    }
    if s.network.profile == Some(NetworkProfile::Off(true)) {
        return Err(invalid(
            "sandbox.network.profile: a profile name, or `false` for no filtering".into(),
        ));
    }
    Ok(())
}

/// Every part of a file, checked: for a write that must leave a file every reader accepts.
pub fn validate_config(doc: &ConfigDoc) -> Result<(), SettingsError> {
    for table in Table::ALL {
        match table {
            Table::Daemon => validate_daemon(&doc.part::<DaemonFile>(table)?, &doc.origin.path)?,
            Table::Settings => doc.part::<SettingsFile>(table).map(|_| ())?,
            Table::Development => doc.part::<DevelopmentFile>(table).map(|_| ())?,
            Table::Permissions => doc.part::<PermissionsFile>(table).map(|_| ())?,
            Table::Branches => doc.part::<BranchesFile>(table).map(|_| ())?,
            Table::Integrations => doc.part::<IntegrationsFile>(table).map(|_| ())?,
            Table::WebSearch => doc.part::<WebSearchFile>(table).map(|_| ())?,
        }
    }
    Ok(())
}

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
        let err = d.part::<DaemonFile>(Table::Daemon).unwrap_err();
        assert!(err.to_string().contains("allw"), "{err}");
        let d = doc(
            "[permissions.host]\nkeybindings = {}\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<PermissionsFile>(Table::Permissions).is_err());
        // A table key holding anything else, even an empty list (which serde reads as an empty table), is refused.
        for (text, table) in [("permissions = []\n", Table::Permissions), ("tracker = 1\n", Table::Integrations)] {
            let d = doc(text, LevelKind::Repository, Sharing::Shared).unwrap();
            let refused = match table {
                Table::Permissions => d.part::<PermissionsFile>(table).is_err(),
                Table::Integrations => d.part::<IntegrationsFile>(table).is_err(),
                Table::Daemon | Table::Settings | Table::Development | Table::Branches | Table::WebSearch => false,
            };
            assert!(refused, "{text}");
        }
    }

    /// A broken table fails only its part.
    #[test]
    fn parts_are_parsed_on_their_own() {
        let d = doc(
            "[tracker]\nprovider = 1\n[branches]\nformt = \"x\"\n[permissions]\nallow = [\"read\"]\n",
            LevelKind::Repository,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<BranchesFile>(Table::Branches).is_err());
        assert_eq!(
            d.part::<PermissionsFile>(Table::Permissions).unwrap().permissions.allow,
            ["read"]
        );
        assert!(
            d.part::<IntegrationsFile>(Table::Integrations)
                .unwrap()
                .tracker
                .is_some()
        );
    }

    #[test]
    fn global_only_and_local_only_keys() {
        let d = doc("[sandbox]\nbase = \"default\"\n", LevelKind::Parent, Sharing::Shared).unwrap();
        let err = d.part::<DaemonFile>(Table::Daemon).unwrap_err();
        assert!(matches!(err, SettingsError::GlobalOnly { ref key, .. } if key == "sandbox.base"));
        for text in [
            "[host]\nroots = []\n",
            "[development]\nsession_analysis = true\n",
            "[permissions.keybindings]\nauto_accept_edits = \"ctrl+a\"\n",
        ] {
            let d = doc(text, LevelKind::Repository, Sharing::Local).unwrap();
            let refused = match Table::ALL.into_iter().find(|t| d.has(*t)).unwrap() {
                Table::Settings => d.part::<SettingsFile>(Table::Settings).is_err(),
                Table::Development => d.part::<DevelopmentFile>(Table::Development).is_err(),
                Table::Permissions => d.part::<PermissionsFile>(Table::Permissions).is_err(),
                Table::Daemon | Table::Branches | Table::Integrations | Table::WebSearch => false,
            };
            assert!(refused, "{text}");
            assert!(validate_config(&doc(text, LevelKind::Global, Sharing::Shared).unwrap()).is_ok());
        }

        let text = "[branches]\nusername = \"me\"\n";
        let err = doc(text, LevelKind::Global, Sharing::Shared)
            .unwrap()
            .part::<BranchesFile>(Table::Branches)
            .unwrap_err();
        assert!(matches!(err, SettingsError::LocalOnly { .. }), "{err}");
        assert!(
            doc(text, LevelKind::Repository, Sharing::Local)
                .unwrap()
                .part::<BranchesFile>(Table::Branches)
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
        assert!(
            load_mcp(&dir.path().join("mcp.json"), LevelKind::Global)
                .unwrap()
                .is_none()
        );
        std::fs::write(dir.path().join("mcp.json"), "[]").unwrap();
        assert!(load_mcp(&dir.path().join("mcp.json"), LevelKind::Global).is_err());
    }

    #[test]
    fn daemon_values_are_validated() {
        let p = Path::new("config.toml");
        let mut file = DaemonFile::default();
        file.sandbox.allow = vec!["relative/path".into()];
        assert!(validate_daemon(&file, p).is_err());
        file.sandbox.allow = vec!["~/x".into(), "/abs".into(), "$SSH_AUTH_SOCK".into()];
        assert!(validate_daemon(&file, p).is_ok());
        file.workers.skip = vec!["../x".into()];
        assert!(validate_daemon(&file, p).is_err());
        file.workers.skip = Vec::new();
        file.sandbox.network.profile = Some(NetworkProfile::Off(true));
        assert!(validate_daemon(&file, p).is_err());
        file.sandbox.network.profile = Some(NetworkProfile::Off(false));
        assert!(validate_daemon(&file, p).is_ok());
        let toml: DaemonFile = toml::from_str("[sandbox.network]\nprofile = false\n").unwrap();
        assert_eq!(toml.sandbox.network.profile, Some(NetworkProfile::Off(false)));
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
