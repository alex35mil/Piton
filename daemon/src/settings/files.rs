//! The configuration files' schemas, how each is read, and which keys a level may set.
//!
//! Every file is strict: unknown keys are refused. Values are optional so a nearer file can leave a key to a farther
//! one; defaults belong to the code that uses a value.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{LevelKind, Origin, SettingsError};
use crate::project::validate_relative;

/// `daemon[.local].toml`: how the daemon runs (global tier only) and per-project `[sandbox]` and `[workers]`.
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

/// `settings[.local].toml`, global tier only: preferences the apps can change. `settings.local.toml` keeps this
/// host's values; a key set there wins over `settings.toml`.
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

/// `permissions[.<runtime>][.local].json`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsFile {
    #[serde(rename = "defaultMode", default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Keybindings::is_empty")]
    pub keybindings: Keybindings,
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
    #[serde(rename = "autoAcceptEdits", default, skip_serializing_if = "Option::is_none")]
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

/// `web-search[.local].json`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebSearchFile {
    #[serde(rename = "apiKey", default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// `workflows[.local].json`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowsFile {
    #[serde(default, skip_serializing_if = "Branches::is_empty")]
    pub branches: Branches,
    #[serde(rename = "workflowProject", default, skip_serializing_if = "Option::is_none")]
    pub workflow_project: Option<String>,
    #[serde(rename = "sessionAnalysis", default, skip_serializing_if = "Option::is_none")]
    pub session_analysis: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Branches {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<BranchFormat>,
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
    SandboxTable,
    NetworkTable,
    WorkersTable,
    HostPrefs,
    ClientPrefs,
    Keybindings,
    Branches
);

/// Keys a file at a non-global level must not set.
pub trait Scoped {
    fn global_only_keys(&self) -> Vec<&'static str>;
}

impl Scoped for DaemonFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        let set = |key, present: bool, keys: &mut Vec<&'static str>| {
            if present {
                keys.push(key);
            }
        };
        set("socket", self.socket.is_some(), &mut keys);
        set("log", self.log.is_some(), &mut keys);
        set("pi", !self.pi.is_empty(), &mut keys);
        set("env", !self.env.is_empty(), &mut keys);
        set("stop", !self.stop.is_empty(), &mut keys);
        set("tailnet", !self.tailnet.is_empty(), &mut keys);
        set("sandbox.command", self.sandbox.command.is_some(), &mut keys);
        set("sandbox.base", self.sandbox.base.is_some(), &mut keys);
        set("sandbox.groups", !self.sandbox.groups.is_empty(), &mut keys);
        set("sandbox.bypass", !self.sandbox.bypass.is_empty(), &mut keys);
        set(
            "sandbox.network.profile",
            self.sandbox.network.profile.is_some(),
            &mut keys,
        );
        keys
    }
}

impl Scoped for PermissionsFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        if self.keybindings.is_empty() {
            Vec::new()
        } else {
            vec!["keybindings"]
        }
    }
}

impl Scoped for WorkflowsFile {
    fn global_only_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.workflow_project.is_some() {
            keys.push("workflowProject");
        }
        if self.session_analysis.is_some() {
            keys.push("sessionAnalysis");
        }
        keys
    }
}

macro_rules! unscoped {
    ($($ty:ty),*) => {$(
        impl Scoped for $ty {
            fn global_only_keys(&self) -> Vec<&'static str> {
                Vec::new()
            }
        }
    )*};
}
unscoped!(McpFile, WebSearchFile, SettingsFile);

/// A file that was read, with where it came from.
#[derive(Debug, Clone)]
pub struct Loaded<T> {
    pub value: T,
    pub origin: Origin,
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

/// Keys that moved out of `daemon.toml`, with where they live now.
const MOVED_DAEMON_KEYS: &[(&str, &str)] = &[
    ("roots", "settings.toml [host] roots"),
    ("attach_mode", "settings.toml [host] attach_mode"),
    ("empty_grace_ms", "settings.toml [host] empty_grace_ms"),
    ("logging", "settings.toml [host] logging"),
    (
        "keep_awake",
        "the host's state database: it's state, not configuration; toggle it in the apps",
    ),
];

/// Reads one file at one level: parses it strictly and checks the level may set its keys.
pub fn load<T>(path: &Path, level: LevelKind, format: Format) -> Result<Option<Loaded<T>>, SettingsError>
where
    T: serde::de::DeserializeOwned + Scoped,
{
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    let value = parse(&text, path, level, format)?;
    Ok(Some(Loaded {
        value,
        origin: Origin {
            path: path.to_path_buf(),
            level,
        },
    }))
}

/// Parses one file's text strictly, as `path` at `level`: moved keys, unknown keys, and keys the level may not set are
/// refused.
pub fn parse<T>(text: &str, path: &Path, level: LevelKind, format: Format) -> Result<T, SettingsError>
where
    T: serde::de::DeserializeOwned + Scoped,
{
    let invalid = |reason: String| SettingsError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let value: T = match format {
        Format::Toml => {
            let table: toml::Table = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
            if let Some((key, now)) = MOVED_DAEMON_KEYS.iter().find(|(key, _)| table.contains_key(*key))
                && is_daemon_file(path)
            {
                return Err(invalid(format!("`{key}` moved to {now}")));
            }
            if is_daemon_file(path)
                && let Some(toml::Value::Table(sandbox)) = table.get("sandbox")
                && sandbox.contains_key("profile")
            {
                return Err(invalid(
                    "`sandbox.profile` is now `sandbox.base`, and its grants go in [sandbox]".into(),
                ));
            }
            toml::from_str(text).map_err(|e| invalid(e.to_string()))?
        }
        Format::Json => {
            // serde reads a JSON array into a struct too; a settings file must be an object.
            let value: serde_json::Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
            if !value.is_object() {
                return Err(invalid("must be a JSON object".into()));
            }
            serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?
        }
    };
    if level != LevelKind::Global
        && let Some(key) = value.global_only_keys().first()
    {
        return Err(SettingsError::GlobalOnly {
            path: path.to_path_buf(),
            key: (*key).to_string(),
            global: super::global_tier(),
        });
    }
    Ok(value)
}

fn is_daemon_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("daemon.toml" | "daemon.local.toml")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
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

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn unknown_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "daemon.toml", "[sandbox]\nallw = []\n");
        let err = load::<DaemonFile>(&path, LevelKind::Repository, Format::Toml).unwrap_err();
        assert!(err.to_string().contains("allw"), "{err}");
        let path = write(dir.path(), "permissions.json", r#"{"alow": []}"#);
        assert!(load::<PermissionsFile>(&path, LevelKind::Repository, Format::Json).is_err());
        for text in ["[]", "null", "1"] {
            let path = write(dir.path(), "permissions.json", text);
            assert!(
                load::<PermissionsFile>(&path, LevelKind::Repository, Format::Json).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn global_only_keys_are_refused_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "daemon.toml", "[sandbox]\nbase = \"default\"\n");
        let err = load::<DaemonFile>(&path, LevelKind::Parent, Format::Toml).unwrap_err();
        assert!(matches!(err, SettingsError::GlobalOnly { ref key, .. } if key == "sandbox.base"));
        assert!(load::<DaemonFile>(&path, LevelKind::Global, Format::Toml).is_ok());

        let path = write(dir.path(), "workflows.json", r#"{"sessionAnalysis": true}"#);
        assert!(matches!(
            load::<WorkflowsFile>(&path, LevelKind::Repository, Format::Json),
            Err(SettingsError::GlobalOnly { .. })
        ));
        let path = write(
            dir.path(),
            "permissions.json",
            r#"{"keybindings": {"autoAcceptEdits": "ctrl+a"}}"#,
        );
        assert!(load::<PermissionsFile>(&path, LevelKind::Repository, Format::Json).is_err());
    }

    #[test]
    fn moved_keys_say_where_they_went() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "daemon.toml", "roots = [\"~/p\"]\n");
        let err = load::<DaemonFile>(&path, LevelKind::Global, Format::Toml).unwrap_err();
        assert!(err.to_string().contains("settings.toml"), "{err}");
        let path = write(dir.path(), "daemon.toml", "[sandbox]\nprofile = \"piton\"\n");
        let err = load::<DaemonFile>(&path, LevelKind::Global, Format::Toml).unwrap_err();
        assert!(err.to_string().contains("sandbox.base"), "{err}");
    }

    #[test]
    fn missing_files_are_none_and_folders_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load::<McpFile>(&dir.path().join("mcp.json"), LevelKind::Global, Format::Json)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(dir.path().join("mcp.json")).unwrap();
        assert!(load::<McpFile>(&dir.path().join("mcp.json"), LevelKind::Global, Format::Json).is_err());
    }

    #[test]
    fn daemon_values_are_validated() {
        let p = Path::new("daemon.toml");
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
