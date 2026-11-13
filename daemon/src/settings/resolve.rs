//! Reading a whole chain into merged settings.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::chain::{Chain, Level, LevelKind, check_level};
use super::files::{
    BranchesFile, ConfigDoc, DaemonFile, DevelopmentFile, IntegrationsFile, Keybindings, PermissionMode,
    PermissionRules, PermissionsFile, Scoped, SettingsFile, Sharing, Table, WebSearchFile, load_mcp, read_config,
    read_text, validate_daemon,
};
use super::{AGENTS_DIR, LOCAL_DIR, OldPath, Origin, PITON_DIR, SettingsError, merge};
use crate::project::Workspace;

pub use super::files::{Branches, DevelopmentTable, WebSearchTable};

/// Where pi's permission rules apply: everywhere, or only in one launch mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Host,
    Sandbox,
}

/// One part of a workspace's configuration: its merged value, or why its files can't be used. Each consumer fails only
/// on the parts it reads, so a broken `[tracker]` never stops permission checks. Serialized as `{"ok": value}` or
/// `{"error": {"message": ...}}`.
pub type Section<T> = Result<T, SettingsError>;

/// The parts of the configuration, for scoping old-layout refusals to the parts whose files moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Daemon,
    Settings,
    Development,
    Permissions,
    Mcp,
    /// The global tier's MCP servers alone.
    McpGlobal,
    WebSearch,
    Branches,
    Integrations,
    Policies,
    Instructions,
}

/// Everything resolved for one workspace.
#[derive(Debug, Serialize)]
pub struct Resolved {
    pub workspace: PathBuf,
    /// The checkout whose `.piton/` was read (a worker's source).
    pub source: PathBuf,
    /// The workspace's own checkout.
    pub checkout: PathBuf,
    pub global: PathBuf,
    /// The user's home folder (from the user database), for `~` in path values.
    pub home: PathBuf,
    pub runtime: Runtime,
    #[serde(serialize_with = "section")]
    pub daemon: Section<DaemonFile>,
    #[serde(serialize_with = "section")]
    pub settings: Section<SettingsFile>,
    #[serde(serialize_with = "section")]
    pub development: Section<DevelopmentTable>,
    #[serde(serialize_with = "section")]
    pub permissions: Section<ResolvedPermissions>,
    /// Every level's servers.
    #[serde(serialize_with = "section")]
    pub mcp: Section<ResolvedMcp>,
    /// The global tier's servers alone: what pi loads in a project it doesn't trust, so an untrusted project's files
    /// can't break them.
    #[serde(serialize_with = "section")]
    pub mcp_global: Section<ResolvedMcp>,
    #[serde(serialize_with = "section")]
    pub web_search: Section<WebSearchTable>,
    #[serde(serialize_with = "section")]
    pub branches: Section<Branches>,
    /// The nearest `[tracker]` and `[forge]`, each whole and unchanged (the pi package validates them).
    #[serde(serialize_with = "section")]
    pub integrations: Section<Integrations>,
    /// Every `policies.md`, farthest first.
    #[serde(serialize_with = "section")]
    pub policies: Section<Vec<FileValue<String>>>,
    /// Rule and skill folders from the `.agents/` chain.
    #[serde(serialize_with = "section")]
    pub instructions: Section<Instructions>,
    /// Workflow artifacts: the source's `.piton/.local`.
    pub artifacts: PathBuf,
    /// Where each level's configuration is written, nearest last: for tools that change it.
    pub config_files: Vec<ConfigFiles>,
    /// Every file read, with its level.
    pub files: Vec<FileOrigin>,
    /// Every path whose appearance, change, or removal changes the result: each level folder and each candidate
    /// file, existing or not. Clients cache the result until one of these changes.
    pub watch: Vec<PathBuf>,
}

impl Resolved {
    /// Every part that can't be used, by its name in the output.
    pub fn errors(&self) -> Vec<(&'static str, &SettingsError)> {
        let Resolved {
            daemon,
            settings,
            development,
            permissions,
            mcp,
            mcp_global,
            web_search,
            branches,
            integrations,
            policies,
            instructions,
            workspace: _,
            source: _,
            checkout: _,
            global: _,
            home: _,
            runtime: _,
            artifacts: _,
            config_files: _,
            files: _,
            watch: _,
        } = self;
        [
            ("daemon", daemon.as_ref().err()),
            ("settings", settings.as_ref().err()),
            ("development", development.as_ref().err()),
            ("permissions", permissions.as_ref().err()),
            ("mcp", mcp.as_ref().err()),
            ("mcp_global", mcp_global.as_ref().err()),
            ("web_search", web_search.as_ref().err()),
            ("branches", branches.as_ref().err()),
            ("integrations", integrations.as_ref().err()),
            ("policies", policies.as_ref().err()),
            ("instructions", instructions.as_ref().err()),
        ]
        .into_iter()
        .filter_map(|(name, error)| error.map(|error| (name, error)))
        .collect()
    }
}

fn section<T: Serialize, S: serde::Serializer>(value: &Section<T>, serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    #[derive(Serialize)]
    struct Error {
        message: String,
    }
    let mut map = serializer.serialize_map(Some(1))?;
    match value {
        Ok(value) => map.serialize_entry("ok", value)?,
        Err(error) => map.serialize_entry(
            "error",
            &Error {
                message: error.to_string(),
            },
        )?,
    }
    map.end()
}

/// Permission rules: `[permissions]` merged with the current runtime's `[permissions.<runtime>]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PermissionSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    #[serde(skip_serializing_if = "Keybindings::is_empty")]
    pub keybindings: Keybindings,
}

impl From<PermissionRules> for PermissionSettings {
    fn from(rules: PermissionRules) -> Self {
        let PermissionRules {
            default_mode,
            allow,
            ask,
            deny,
        } = rules;
        Self {
            default_mode,
            allow,
            ask,
            deny,
            keybindings: Keybindings::default(),
        }
    }
}

/// The merged permission rules, and each table they came from.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPermissions {
    pub merged: PermissionSettings,
    /// Each permission table of the current runtime that sets something, in merge order, with its own rules: for
    /// inspection (which file sets a rule).
    pub sources: Vec<PermissionSource>,
}

/// The `.agents/` chain's folders. Every candidate is listed, existing or not: clients watch them.
#[derive(Debug, Clone, Serialize)]
pub struct Instructions {
    /// Rule folders, farthest first: `<level>/rules`.
    pub rules: Vec<PathBuf>,
    /// Skill folders piton adds to pi's own discovery, farthest first (pi discovers the repository's own).
    pub skills: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionSource {
    pub path: PathBuf,
    pub level: LevelKind,
    /// `shared` (`[permissions]`) or the runtime (`[permissions.<runtime>]`).
    pub scope: PermissionScope,
    /// From `config.local.toml`.
    pub local: bool,
    pub settings: PermissionSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    Shared,
    Host,
    Sandbox,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileValue<T> {
    pub path: PathBuf,
    pub value: T,
}

/// The nearest `[tracker]` and `[forge]`, as JSON.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Integrations {
    pub tracker: Option<FileValue<serde_json::Value>>,
    pub forge: Option<FileValue<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileOrigin {
    pub path: PathBuf,
    pub level: LevelKind,
}

/// One level's configuration files, existing or not.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigFiles {
    pub level: LevelKind,
    pub shared: PathBuf,
    pub local: PathBuf,
}

/// MCP servers by name, the nearest entry replacing the whole entry; names that differ only by `-` and `_` conflict.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResolvedMcp {
    pub servers: Vec<McpServer>,
    #[serde(rename = "autoEnableCodemode", skip_serializing_if = "Option::is_none")]
    pub auto_enable_codemode: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServer {
    pub name: String,
    pub config: serde_json::Map<String, serde_json::Value>,
    pub path: PathBuf,
    /// The level of the file the entry came from: pi loads only global entries in a project it doesn't trust.
    pub level: LevelKind,
}

/// A level's `config.toml` or `config.local.toml` as read: its document, or why it can't be used. The error is kept
/// as text so every part that reads the file can report it.
enum Doc {
    Read(ConfigDoc),
    Failed { path: PathBuf, error: FailedRead },
}

enum FailedRead {
    Io(std::io::ErrorKind, String),
    Invalid(String),
}

impl Doc {
    fn get(&self) -> Result<&ConfigDoc, SettingsError> {
        match self {
            Doc::Read(doc) => Ok(doc),
            Doc::Failed { path, error } => Err(match error {
                FailedRead::Io(kind, message) => SettingsError::Read {
                    path: path.clone(),
                    source: std::io::Error::new(*kind, message.clone()),
                },
                FailedRead::Invalid(reason) => SettingsError::Invalid {
                    path: path.clone(),
                    reason: reason.clone(),
                },
            }),
        }
    }
}

/// Every level's configuration files, read once, in merge order: per level the shared file, then the local one.
struct Docs {
    docs: Vec<Doc>,
}

impl Docs {
    fn read(levels: &[Level], files: &mut Vec<FileOrigin>) -> Self {
        let mut docs = Vec::new();
        for level in levels {
            for sharing in [Sharing::Shared, Sharing::Local] {
                let path = level.dir.join(sharing.file_name());
                match read_config(&path, level.kind, sharing) {
                    Ok(None) => {}
                    Ok(Some(doc)) => {
                        files.push(FileOrigin {
                            path,
                            level: level.kind,
                        });
                        docs.push(Doc::Read(doc));
                    }
                    Err(error) => {
                        let error = match error {
                            SettingsError::Read { source, .. } => FailedRead::Io(source.kind(), source.to_string()),
                            SettingsError::Invalid { reason, .. } => FailedRead::Invalid(reason),
                            other @ (SettingsError::GlobalOnly { .. }
                            | SettingsError::LocalOnly { .. }
                            | SettingsError::Conflict { .. }
                            | SettingsError::OldLayout(_)
                            | SettingsError::Project(_)) => FailedRead::Invalid(other.to_string()),
                        };
                        docs.push(Doc::Failed { path, error });
                    }
                }
            }
        }
        Self { docs }
    }

    /// Each file's `table` part that sets something, in merge order. A file that can't be read fails every part.
    fn parts<T>(&self, table: Table) -> Result<Vec<(T, &ConfigDoc)>, SettingsError>
    where
        T: serde::de::DeserializeOwned + Scoped,
    {
        let mut parts = Vec::new();
        for doc in &self.docs {
            let doc = doc.get()?;
            if doc.has(table) {
                parts.push((doc.part::<T>(table)?, doc));
            }
        }
        Ok(parts)
    }
}

/// The machine-wide paths resolution depends on.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub global: PathBuf,
    /// pi's agent folder, checked for files piton used to keep there.
    pub pi_agent_dir: PathBuf,
}

impl Paths {
    /// From this process: the user database's home, the global tier, and pi's agent folder (`PI_CODING_AGENT_DIR`,
    /// else `~/.pi/agent`).
    pub fn current() -> Self {
        let home = super::home();
        let pi_agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi/agent"));
        Self {
            global: super::global_tier(),
            home,
            pi_agent_dir,
        }
    }
}

/// Resolves a workspace: both chains, each part on its own. An old-layout file fails only the parts that read it.
/// Fails as a whole only when a level folder exists but can't be listed, since skipping it could drop a deny rule.
pub fn resolve(workspace: &Workspace, paths: &Paths, runtime: Runtime) -> Result<Resolved, SettingsError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    let old = old_layout(&chain, paths);
    read_chain(&chain, &workspace.path, &paths.home, runtime, &old)
}

/// Only the global tier: the daemon's own settings and the apps' preferences, which never depend on a project.
pub fn resolve_global(paths: &Paths) -> Result<Resolved, SettingsError> {
    let chain = Chain::global_only(&paths.global);
    read_chain(&chain, &paths.global, &paths.home, Runtime::Host, &old_global(paths))
}

/// The daemon's part of a workspace's configuration.
#[derive(Debug, Clone)]
pub struct DaemonSettings {
    /// The merged daemon tables of the `.piton/` chain: sandbox grants, workers.
    pub daemon: DaemonFile,
    /// The checkout whose `.piton/` was read (a worker's source).
    pub source: PathBuf,
    /// The workspace's own checkout, where pi reads `.pi/` and `.agents/skills/`.
    pub checkout: PathBuf,
    /// The existing level folders of both chains, farthest first: a sandbox reads its whole configuration.
    pub levels: Vec<PathBuf>,
}

/// The daemon's part of a workspace's configuration: the merged daemon tables of the `.piton/` chain and the chains'
/// existing folders. Refuses only the old files the daemon itself read; the pi package's parts are checked by
/// [`resolve`] in the pi package, so they never block a launch or a worker.
pub fn resolve_daemon(workspace: &Workspace, paths: &Paths) -> Result<DaemonSettings, SettingsError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    let old: Vec<OldPath> = old_layout(&chain, paths)
        .into_iter()
        .filter(|old| old.parts.contains(&Part::Daemon))
        .collect();
    if !old.is_empty() {
        return Err(SettingsError::OldLayout(old));
    }
    let mut piton = Vec::new();
    let mut levels = Vec::new();
    for level in &chain.piton {
        if check_level(level)? {
            piton.push(level.clone());
            levels.push(level.dir.clone());
        }
    }
    for level in &chain.agents {
        if check_level(level)? && !levels.contains(&level.dir) {
            levels.push(level.dir.clone());
        }
    }
    let docs = Docs::read(&piton, &mut Vec::new());
    Ok(DaemonSettings {
        daemon: read_daemon(&docs)?,
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        levels,
    })
}

fn read_daemon(docs: &Docs) -> Result<DaemonFile, SettingsError> {
    let mut daemon = DaemonFile::default();
    for (file, doc) in docs.parts::<DaemonFile>(Table::Daemon)? {
        validate_daemon(&file, &doc.origin.path)?;
        merge::daemon(&mut daemon, file);
    }
    Ok(daemon)
}

/// One part: refused when old-layout files hold its configuration, else read.
fn part<T>(old: &[OldPath], part: Part, read: impl FnOnce() -> Result<T, SettingsError>) -> Section<T> {
    let moved: Vec<OldPath> = old.iter().filter(|old| old.parts.contains(&part)).cloned().collect();
    if !moved.is_empty() {
        return Err(SettingsError::OldLayout(moved));
    }
    read()
}

fn read_chain(
    chain: &Chain,
    workspace: &Path,
    home: &Path,
    runtime: Runtime,
    old: &[OldPath],
) -> Result<Resolved, SettingsError> {
    let mut watch = Vec::new();
    let mut files = Vec::new();
    let mut piton = Vec::new();
    for level in &chain.piton {
        watch.push(level.dir.clone());
        if check_level(level)? {
            piton.push(level.clone());
        }
    }
    for level in &chain.agents {
        watch.push(level.dir.clone());
        check_level(level)?;
    }
    // Every candidate, whether or not its part can be read: fixing a broken or moved file changes the result.
    watch.extend(old.iter().map(|old| old.path.clone()));
    for level in &piton {
        for name in [
            Sharing::Shared.file_name(),
            Sharing::Local.file_name(),
            "mcp.json",
            "mcp.local.json",
            "policies.md",
        ] {
            watch.push(level.dir.join(name));
        }
    }
    let config_files = chain
        .piton
        .iter()
        .map(|level| ConfigFiles {
            level: level.kind,
            shared: level.dir.join(Sharing::Shared.file_name()),
            local: level.dir.join(Sharing::Local.file_name()),
        })
        .collect();
    let global_levels: Vec<Level> = piton
        .iter()
        .filter(|level| level.kind == LevelKind::Global)
        .cloned()
        .collect();
    let docs = Docs::read(&piton, &mut files);

    let daemon = part(old, Part::Daemon, || read_daemon(&docs));

    // Global-only parts: a nearer level that sets one is refused by the part's scope check.
    let settings = part(old, Part::Settings, || {
        let mut settings = SettingsFile::default();
        for (file, _) in docs.parts::<SettingsFile>(Table::Settings)? {
            merge::settings(&mut settings, file);
        }
        Ok(settings)
    });
    let development = part(old, Part::Development, || {
        let mut development = DevelopmentTable::default();
        for (file, _) in docs.parts::<DevelopmentFile>(Table::Development)? {
            merge::development(&mut development, file.development);
        }
        Ok(development)
    });

    let permissions = part(old, Part::Permissions, || read_permissions(&docs, runtime));
    let mcp = part(old, Part::Mcp, || read_mcp(&mut files, &piton));
    let mcp_global = part(old, Part::McpGlobal, || read_mcp(&mut Vec::new(), &global_levels));

    let web_search = part(old, Part::WebSearch, || {
        let mut web_search = WebSearchTable::default();
        for (file, _) in docs.parts::<WebSearchFile>(Table::WebSearch)? {
            merge::web_search(&mut web_search, file.web_search);
        }
        Ok(web_search)
    });

    let branches = part(old, Part::Branches, || {
        let mut branches = Branches::default();
        for (file, _) in docs.parts::<BranchesFile>(Table::Branches)? {
            merge::branches(&mut branches, file.branches);
        }
        Ok(branches)
    });

    let integrations = part(old, Part::Integrations, || {
        let mut integrations = Integrations::default();
        for (file, doc) in docs.parts::<IntegrationsFile>(Table::Integrations)? {
            let json = |table: toml::Table| -> Result<FileValue<serde_json::Value>, SettingsError> {
                let value = serde_json::to_value(table).map_err(|e| SettingsError::Invalid {
                    path: doc.origin.path.clone(),
                    reason: e.to_string(),
                })?;
                Ok(FileValue {
                    path: doc.origin.path.clone(),
                    value,
                })
            };
            // Each table whole: a nearer level's replaces a farther one's.
            if let Some(tracker) = file.tracker {
                integrations.tracker = Some(json(tracker)?);
            }
            if let Some(forge) = file.forge {
                integrations.forge = Some(json(forge)?);
            }
        }
        Ok(integrations)
    });

    let policies = part(old, Part::Policies, || {
        let mut policies = Vec::new();
        for level in &piton {
            let path = level.dir.join("policies.md");
            if let Some(text) = read_text(&path)? {
                files.push(FileOrigin {
                    path: path.clone(),
                    level: level.kind,
                });
                policies.push(FileValue { path, value: text });
            }
        }
        Ok(policies)
    });

    // Every candidate folder, existing or not: clients watch them, and a missing one simply has no files.
    let instructions = part(old, Part::Instructions, || {
        Ok(Instructions {
            rules: chain.agents.iter().map(|level| level.dir.join("rules")).collect(),
            skills: chain
                .agents
                .iter()
                .filter(|level| level.kind != LevelKind::Repository)
                .map(|level| level.dir.join("skills"))
                .collect(),
        })
    });

    Ok(Resolved {
        workspace: workspace.to_path_buf(),
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        global: chain.global.clone(),
        home: home.to_path_buf(),
        runtime,
        daemon,
        settings,
        development,
        permissions,
        mcp,
        mcp_global,
        web_search,
        branches,
        integrations,
        policies,
        instructions,
        artifacts: chain.source.join(LOCAL_DIR),
        config_files,
        files,
        watch,
    })
}

/// Per file: `[permissions]`, then the runtime's `[permissions.<runtime>]`; files in merge order.
fn read_permissions(docs: &Docs, runtime: Runtime) -> Result<ResolvedPermissions, SettingsError> {
    let mut merged = PermissionSettings::default();
    let mut sources = Vec::new();
    for (file, doc) in docs.parts::<PermissionsFile>(Table::Permissions)? {
        let table = file.permissions;
        let (rules, scope) = match runtime {
            Runtime::Host => (table.host, PermissionScope::Host),
            Runtime::Sandbox => (table.sandbox, PermissionScope::Sandbox),
        };
        let shared = PermissionSettings {
            default_mode: table.default_mode,
            allow: table.allow,
            ask: table.ask,
            deny: table.deny,
            keybindings: table.keybindings,
        };
        let local = match doc.sharing {
            Sharing::Shared => false,
            Sharing::Local => true,
        };
        for (settings, scope) in [
            (shared, PermissionScope::Shared),
            (PermissionSettings::from(rules), scope),
        ] {
            if settings == PermissionSettings::default() {
                continue;
            }
            sources.push(PermissionSource {
                path: doc.origin.path.clone(),
                level: doc.origin.level,
                scope,
                local,
                settings: settings.clone(),
            });
            merge::permissions(&mut merged, settings);
        }
    }
    Ok(ResolvedPermissions { merged, sources })
}

fn read_mcp(files: &mut Vec<FileOrigin>, levels: &[Level]) -> Result<ResolvedMcp, SettingsError> {
    let mut mcp = ResolvedMcp::default();
    for level in levels {
        for name in ["mcp.json", "mcp.local.json"] {
            let Some(file) = load_mcp(&level.dir.join(name), level.kind)? else {
                continue;
            };
            files.push(FileOrigin {
                path: file.origin.path.clone(),
                level: level.kind,
            });
            if file.value.auto_enable_codemode.is_some() {
                mcp.auto_enable_codemode = file.value.auto_enable_codemode;
            }
            for (name, config) in file.value.mcp_servers {
                let serde_json::Value::Object(config) = config else {
                    return Err(SettingsError::Invalid {
                        path: file.origin.path.clone(),
                        reason: format!("mcpServers.{name} must be an object"),
                    });
                };
                add_mcp_server(&mut mcp, name, config, &file.origin)?;
            }
        }
    }
    Ok(mcp)
}

fn add_mcp_server(
    mcp: &mut ResolvedMcp,
    name: String,
    config: serde_json::Map<String, serde_json::Value>,
    origin: &Origin,
) -> Result<(), SettingsError> {
    let key = name.replace('-', "_");
    if let Some(clash) = mcp
        .servers
        .iter()
        .find(|server| server.name != name && server.name.replace('-', "_") == key)
    {
        return Err(SettingsError::Conflict {
            path: origin.path.clone(),
            key: format!("mcpServers.{name}"),
            reason: format!("conflicts with server {:?} from {}", clash.name, clash.path.display()),
        });
    }
    let server = McpServer {
        name: name.clone(),
        config,
        path: origin.path.clone(),
        level: origin.level,
    };
    match mcp.servers.iter_mut().find(|existing| existing.name == name) {
        Some(existing) => *existing = server,
        None => mcp.servers.push(server),
    }
    Ok(())
}

const DAEMON: &[Part] = &[Part::Daemon];
const SETTINGS: &[Part] = &[Part::Settings];
const PERMISSIONS: &[Part] = &[Part::Permissions];
const MCP: &[Part] = &[Part::Mcp];
const WEB_SEARCH: &[Part] = &[Part::WebSearch];
const BRANCHES: &[Part] = &[Part::Branches, Part::Development];
const INTEGRATIONS: &[Part] = &[Part::Integrations];

/// The files of earlier layouts in a `.piton/` folder (the source's, a parent's, or the global tier), now tables of
/// `config.toml`.
const PITON_FILES: &[(&str, &[Part])] = &[
    ("daemon.toml", DAEMON),
    ("daemon.local.toml", DAEMON),
    ("settings.toml", SETTINGS),
    ("settings.local.toml", SETTINGS),
    ("workflows.json", BRANCHES),
    ("workflows.local.json", BRANCHES),
    ("integrations.json", INTEGRATIONS),
    ("permissions.json", PERMISSIONS),
    ("permissions.local.json", PERMISSIONS),
    ("permissions.host.json", PERMISSIONS),
    ("permissions.sandbox.json", PERMISSIONS),
    ("permissions.host.local.json", PERMISSIONS),
    ("permissions.sandbox.local.json", PERMISSIONS),
    ("web-search.json", WEB_SEARCH),
    ("web-search.local.json", WEB_SEARCH),
    ("nono.json", DAEMON),
    ("nono.local.json", DAEMON),
];

fn old_in_folder(dir: &Path, found: &mut Vec<OldPath>) {
    for (name, parts) in PITON_FILES {
        let path = dir.join(name);
        if path.exists() {
            found.push(OldPath {
                path,
                now: "config.toml (or config.local.toml) in the same folder",
                parts,
            });
        }
    }
}

/// Files of earlier layouts in the folders whose files are read: the source checkout and its parent folders. A
/// worker's own copies are never read (its settings come from its source), so a worker cloned before its source was
/// migrated doesn't block anything.
fn old_layout(chain: &Chain, paths: &Paths) -> Vec<OldPath> {
    let mut found = old_global(paths);
    let mut dirs: Vec<&Path> = vec![&chain.source];
    for level in &chain.piton {
        if level.kind == LevelKind::Parent
            && let Some(parent) = level.dir.parent()
        {
            dirs.push(parent);
        }
    }
    const PROJECT: &[Part] = &[Part::Branches, Part::Integrations, Part::Policies];
    for dir in dirs {
        for (name, now, parts) in [
            (".piton.toml", ".piton/config.toml [workers]", DAEMON),
            (".nono.json", ".piton/config.toml [sandbox]", DAEMON),
            (".nono.local.json", ".piton/config.local.toml [sandbox]", DAEMON),
            (
                ".project",
                ".piton/ (config.toml [branches] [tracker] [forge], policies.md, .local/)",
                PROJECT,
            ),
        ] {
            let path = dir.join(name);
            if path.symlink_metadata().is_ok() {
                found.push(OldPath { path, now, parts });
            }
        }
        for (name, parts) in [
            ("permissions.json", PERMISSIONS),
            ("permissions.local.json", PERMISSIONS),
            ("permissions.host.json", PERMISSIONS),
            ("permissions.sandbox.json", PERMISSIONS),
            ("permissions.host.local.json", PERMISSIONS),
            ("permissions.sandbox.local.json", PERMISSIONS),
            ("mcp.json", MCP),
            ("mcp.local.json", MCP),
            ("web-search.json", WEB_SEARCH),
            ("web-search.local.json", WEB_SEARCH),
        ] {
            let path = dir.join(AGENTS_DIR).join(name);
            if path.exists() {
                found.push(OldPath {
                    path,
                    now: ".piton/ (mcp.json as is; the rest in config.toml)",
                    parts,
                });
            }
        }
        old_in_folder(&dir.join(PITON_DIR), &mut found);
    }
    found
}

/// Global files of earlier layouts: the global tier's separate files and the files piton kept in pi's agent folder.
fn old_global(paths: &Paths) -> Vec<OldPath> {
    let mut found = Vec::new();
    old_in_folder(&paths.global, &mut found);
    let pi_dir = &paths.pi_agent_dir;
    let both_mcp: &'static [Part] = &[Part::Mcp, Part::McpGlobal];
    for (name, parts) in [
        ("permissions.json", PERMISSIONS),
        ("permissions.host.json", PERMISSIONS),
        ("permissions.sandbox.json", PERMISSIONS),
        ("mcp.json", both_mcp),
        ("web-search.json", WEB_SEARCH),
        ("workflows.json", BRANCHES),
    ] {
        let path = pi_dir.join(name);
        if path.exists() {
            found.push(OldPath {
                path,
                now: "the global tier (mcp.json as is; the rest in config.toml)",
                parts,
            });
        }
    }
    let rules = pi_dir.join("rules");
    if rules.is_dir() {
        found.push(OldPath {
            path: rules,
            now: "agents/rules/ in the global tier",
            parts: &[Part::Instructions],
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Relation;
    use crate::settings::files::BranchFormat;

    struct Tree {
        _dir: tempfile::TempDir,
        home: PathBuf,
        global: PathBuf,
        paths: Paths,
    }

    fn tree() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let global = home.join(".config/piton");
        std::fs::create_dir_all(&global).unwrap();
        let paths = Paths {
            home: home.clone(),
            global: global.clone(),
            pi_agent_dir: home.join(".pi/agent"),
        };
        Tree {
            _dir: dir,
            home,
            global,
            paths,
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn source(path: PathBuf) -> Workspace {
        Workspace {
            path: path.clone(),
            checkout: path,
            relation: Relation::Source,
        }
    }

    fn run(t: &Tree, ws: &Workspace, runtime: Runtime) -> Result<Resolved, SettingsError> {
        resolve(ws, &t.paths, runtime)
    }

    /// The whole chain, every part: global tier, a parent folder, and the repository.
    #[test]
    fn merges_the_chain() {
        let t = tree();
        let parent = t.home.join("Projects/work");
        let repo = parent.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        write(
            &t.global.join("config.toml"),
            r#"
[sandbox]
base = "mise-dev"
groups = ["git_config"]
allow = ["~/.cargo/registry"]
[sandbox.network]
open_port = [0]
profile = false

[permissions]
deny = ["bash(rm -rf *)"]
ask = ["write"]
[permissions.sandbox]
default_mode = "allow"
ask = ["bash(git push *)"]
[permissions.host]
default_mode = "ask"

[development]
session_analysis = false

[host]
roots = ["~/Projects"]
"#,
        );
        write(
            &t.global.join("config.local.toml"),
            "[host]\nroots = [\"~/Projects/personal\"]\n[branches]\nusername = \"me\"\n",
        );
        write(
            &t.global.join("mcp.json"),
            r#"{"mcpServers":{"github":{"url":"https://a"},"docs":{"command":"d"}}}"#,
        );
        write(&t.global.join("policies.md"), "global policy\n");
        write(&t.global.join("agents/rules/a.md"), "rule\n");
        write(
            &parent.join(".piton/config.toml"),
            "[sandbox]\nallow = [\"~/Projects/work/.pi\"]\n[tracker]\nprovider = \"github\"\n[forge]\nprovider = \"github\"\n",
        );
        write(
            &repo.join(".piton/config.toml"),
            r#"
[workers]
skip = ["apps/.build"]
[sandbox]
read = ["/r"]
[permissions]
allow = ["read"]
default_mode = "ask"
[branches]
format = "username/identifier-title"
[tracker]
provider = "linear"
mcp_server = "linear"
[web_search]
api_key = "k"
"#,
        );
        write(
            &repo.join(".piton/config.local.toml"),
            "[sandbox]\nallow = [\"/local\"]\n",
        );
        write(
            &repo.join(".piton/mcp.local.json"),
            r#"{"mcpServers":{"github":{"url":"https://b"}}}"#,
        );
        write(&repo.join(".piton/policies.md"), "repo policy\n");

        let r = run(&t, &source(repo.clone()), Runtime::Sandbox).unwrap();
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        let daemon = r.daemon.as_ref().unwrap();
        let s = &daemon.sandbox;
        assert_eq!(s.base.as_deref(), Some("mise-dev"));
        assert_eq!(s.groups, ["git_config"]);
        assert_eq!(s.allow, ["~/.cargo/registry", "~/Projects/work/.pi", "/local"]);
        assert_eq!(s.read, ["/r"]);
        assert_eq!(s.network.open_port, [0]);
        assert_eq!(daemon.workers.skip, ["apps/.build"]);

        // Sandbox runtime: the host table isn't read; the repository's default_mode is nearest.
        let permissions = r.permissions.as_ref().unwrap();
        assert_eq!(permissions.merged.deny, ["bash(rm -rf *)"]);
        assert_eq!(permissions.merged.ask, ["write", "bash(git push *)"]);
        assert_eq!(permissions.merged.allow, ["read"]);
        assert_eq!(permissions.merged.default_mode, Some(PermissionMode::Ask));
        let scopes: Vec<_> = permissions.sources.iter().map(|s| (s.level, s.scope)).collect();
        assert_eq!(
            scopes,
            [
                (LevelKind::Global, PermissionScope::Shared),
                (LevelKind::Global, PermissionScope::Sandbox),
                (LevelKind::Repository, PermissionScope::Shared),
            ]
        );

        // MCP: the nearer entry replaces the whole entry, order kept; the global tier's alone stay apart.
        let mcp = r.mcp.as_ref().unwrap();
        let names: Vec<_> = mcp.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["github", "docs"]);
        assert_eq!(mcp.servers[0].config["url"], "https://b");
        assert_eq!(r.mcp_global.as_ref().unwrap().servers[0].config["url"], "https://a");

        // Branches merge per key; the username comes from a local file.
        let branches = r.branches.as_ref().unwrap();
        assert_eq!(branches.format, Some(BranchFormat::UsernameIdentifierTitle));
        assert_eq!(branches.username.as_deref(), Some("me"));
        assert_eq!(r.development.as_ref().unwrap().session_analysis, Some(false));
        assert_eq!(
            r.settings.as_ref().unwrap().host.roots.as_deref(),
            Some(&["~/Projects/personal".to_string()][..])
        );
        assert_eq!(r.web_search.as_ref().unwrap().api_key.as_deref(), Some("k"));

        // [tracker] is replaced whole by the nearer level; [forge] comes from the parent.
        let integrations = r.integrations.as_ref().unwrap();
        let tracker = integrations.tracker.as_ref().unwrap();
        assert_eq!(tracker.path, repo.join(".piton/config.toml"));
        assert_eq!(
            tracker.value,
            serde_json::json!({"provider": "linear", "mcp_server": "linear"})
        );
        assert_eq!(
            integrations.forge.as_ref().unwrap().path,
            parent.join(".piton/config.toml")
        );

        let policies: Vec<_> = r.policies.as_ref().unwrap().iter().map(|p| p.value.as_str()).collect();
        assert_eq!(policies, ["global policy\n", "repo policy\n"]);
        let instructions = r.instructions.as_ref().unwrap();
        assert_eq!(instructions.rules.first().unwrap(), &t.global.join("agents/rules"));
        assert_eq!(instructions.rules.last().unwrap(), &repo.join(".agents/rules"));
        assert!(instructions.skills.contains(&t.global.join("agents/skills")));
        assert!(
            !instructions.skills.contains(&repo.join(".agents/skills")),
            "pi discovers the repository's own"
        );
        assert_eq!(r.artifacts, repo.join(".piton/.local"));
        assert!(r.watch.contains(&repo.join(".piton/config.local.toml")));
        assert!(r.files.iter().any(|f| f.path == repo.join(".piton/config.local.toml")));
        let last = r.config_files.last().unwrap();
        assert_eq!(
            (last.level, &last.shared),
            (LevelKind::Repository, &repo.join(".piton/config.toml"))
        );

        let host = run(&t, &source(repo), Runtime::Host).unwrap();
        let permissions = &host.permissions.as_ref().unwrap().merged;
        assert_eq!(permissions.ask, ["write"]);
        assert_eq!(permissions.default_mode, Some(PermissionMode::Ask));
    }

    #[test]
    fn scope_rules_are_enforced() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".piton/config.toml"), "[sandbox]\nbase = \"default\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.daemon, Err(SettingsError::GlobalOnly { .. })));
        assert!(r.permissions.is_ok());
        // Global-only tables in a repository fail their part, even though only the global tier's are used.
        write(&repo.join(".piton/config.toml"), "[host]\nroots = []\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.settings, Err(SettingsError::GlobalOnly { .. })));
        // A username in a shared file.
        write(&repo.join(".piton/config.toml"), "[branches]\nusername = \"me\"\n");
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(SettingsError::LocalOnly { .. })));
    }

    #[test]
    fn mcp_names_differing_by_dash_conflict() {
        let t = tree();
        let repo = t.home.join("repo");
        write(
            &t.global.join("mcp.json"),
            r#"{"mcpServers":{"my-server":{"command":"a"}}}"#,
        );
        write(
            &repo.join(".piton/mcp.json"),
            r#"{"mcpServers":{"my_server":{"command":"b"}}}"#,
        );
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.mcp, Err(SettingsError::Conflict { .. })));
        // An untrusted project's files can't break the global servers pi loads there.
        assert_eq!(r.mcp_global.unwrap().servers[0].name, "my-server");
    }

    /// A broken table fails only its part; a file that isn't TOML fails every part that reads config.toml.
    #[test]
    fn a_broken_table_fails_only_its_part() {
        let t = tree();
        let repo = t.home.join("repo");
        write(
            &repo.join(".piton/config.toml"),
            "[branches]\nunknown = 1\n[permissions]\nallow = [\"read\"]\n",
        );
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(SettingsError::Invalid { .. })));
        assert_eq!(r.permissions.as_ref().unwrap().merged.allow, ["read"]);
        assert!(r.integrations.is_ok() && r.mcp.is_ok() && r.policies.is_ok() && r.instructions.is_ok());

        write(&repo.join(".piton/config.toml"), "[permissions\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.permissions, Err(SettingsError::Invalid { .. })));
        assert!(matches!(r.daemon, Err(SettingsError::Invalid { .. })));
        assert!(r.mcp.is_ok() && r.policies.is_ok());
        // The broken file stays watched, so fixing it invalidates a client's cache.
        assert!(r.watch.contains(&repo.join(".piton/config.toml")));
    }

    #[test]
    fn earlier_layouts_are_refused_by_the_parts_that_read_them() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".nono.json"), "{}");
        write(&repo.join(".project/config.json"), "{}");
        write(&repo.join(".agents/permissions.json"), "{}");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        let Err(err) = &r.permissions else {
            panic!("permissions read the old layout")
        };
        let SettingsError::OldLayout(paths) = err else {
            panic!("{err}")
        };
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.path.strip_prefix(&repo).unwrap().to_owned())
            .collect();
        assert_eq!(names, [PathBuf::from(".agents/permissions.json")]);
        assert!(err.to_string().contains("piton migrate"));
        assert!(matches!(r.daemon, Err(SettingsError::OldLayout(_))));
        assert!(matches!(r.branches, Err(SettingsError::OldLayout(_))));
        assert!(matches!(r.integrations, Err(SettingsError::OldLayout(_))));
        assert!(r.mcp.is_ok() && r.web_search.is_ok() && r.instructions.is_ok());
        assert!(r.watch.contains(&repo.join(".nono.json")));
        for old in [".nono.json", ".agents/permissions.json"] {
            std::fs::remove_file(repo.join(old)).unwrap();
        }
        std::fs::remove_dir_all(repo.join(".project")).unwrap();
        // Rules and skills in .agents/ are the current layout.
        write(&repo.join(".agents/rules/r.md"), "x");
        assert!(
            run(&t, &source(repo.clone()), Runtime::Host)
                .unwrap()
                .errors()
                .is_empty()
        );

        // The separate files of the layout before config.toml.
        write(&repo.join(".piton/integrations.json"), "{}");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.integrations, Err(SettingsError::OldLayout(_))));
        assert!(r.permissions.is_ok());
        std::fs::remove_file(repo.join(".piton/integrations.json")).unwrap();

        write(&t.global.join("settings.toml"), "[host]\n");
        let global = resolve_global(&t.paths).unwrap();
        assert!(matches!(global.settings, Err(SettingsError::OldLayout(_))));
        assert!(global.daemon.is_ok());
        std::fs::remove_file(t.global.join("settings.toml")).unwrap();
        // Files piton kept in pi's agent folder refuse only their own part, never the daemon's.
        write(&t.paths.pi_agent_dir.join("permissions.json"), "{}");
        let global = resolve_global(&t.paths).unwrap();
        assert!(global.daemon.is_ok() && global.settings.is_ok());
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.permissions, Err(SettingsError::OldLayout(_))));
        assert!(r.branches.is_ok());
    }

    /// A worker reads `.piton/` from its source, never its own copy.
    #[test]
    fn a_worker_reads_its_sources_settings() {
        let t = tree();
        let src = t.home.join("p/repo");
        let worker = t.home.join("p/repo--w");
        write(&src.join(".piton/config.toml"), "[permissions]\ndeny = [\"bash(*)\"]\n");
        write(
            &worker.join(".piton/config.toml"),
            "[permissions]\nallow = [\"bash(*)\"]\n",
        );
        let ws = Workspace {
            path: worker.clone(),
            checkout: worker.clone(),
            relation: Relation::Worker {
                source: src.clone(),
                name: "w".into(),
            },
        };
        // A worker cloned before its source was migrated still has the old files; they're never read.
        write(&worker.join(".nono.json"), "{}");
        write(&worker.join(".piton.toml"), "[workers]\n");
        write(&worker.join(".project/config.json"), "{}");
        write(&worker.join(".agents/permissions.json"), "{}");
        write(&worker.join(".piton/daemon.toml"), "");
        assert!(resolve_daemon(&ws, &t.paths).is_ok());
        let r = run(&t, &ws, Runtime::Sandbox).unwrap();
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        let permissions = &r.permissions.as_ref().unwrap().merged;
        assert_eq!(permissions.deny, ["bash(*)"]);
        assert!(permissions.allow.is_empty());
        assert_eq!(r.artifacts, src.join(".piton/.local"));
        assert_eq!(
            r.instructions.as_ref().unwrap().rules.last().unwrap(),
            &worker.join(".agents/rules")
        );
    }

    #[test]
    fn an_unreadable_level_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let t = tree();
        let repo = t.home.join("repo");
        let dir = repo.join(".piton");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = run(&t, &source(repo), Runtime::Host);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(SettingsError::Read { .. })));
    }
}
