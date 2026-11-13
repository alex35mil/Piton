//! Reading a whole chain into merged settings.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::chain::{Chain, Level, LevelKind, check_level};
use super::files::{
    DaemonFile, Format, Loaded, McpFile, PermissionsFile, Scoped, SettingsFile, WebSearchFile, WorkflowsFile, load,
    read_text, validate_daemon,
};
use super::{AGENTS_DIR, LOCAL_DIR, OldPath, Origin, PITON_DIR, SettingsError, merge};
use crate::project::Workspace;

/// Where pi's permission rules apply: everywhere, or only in one launch mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Host,
    Sandbox,
}

impl Runtime {
    fn name(self) -> &'static str {
        match self {
            Runtime::Host => "host",
            Runtime::Sandbox => "sandbox",
        }
    }
}

/// One part of a workspace's configuration: its merged value, or why its files can't be used. Each consumer fails only
/// on the parts it reads, so a broken `integrations.json` never stops permission checks. Serialized as `{"ok": value}`
/// or `{"error": {"message": ...}}`.
pub type Section<T> = Result<T, SettingsError>;

/// The parts of the configuration, for scoping old-layout refusals to the parts whose files moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Daemon,
    Settings,
    Permissions,
    Mcp,
    /// The global tier's MCP servers alone.
    McpGlobal,
    WebSearch,
    Workflows,
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
    pub permissions: Section<ResolvedPermissions>,
    /// Every level's servers.
    #[serde(serialize_with = "section")]
    pub mcp: Section<ResolvedMcp>,
    /// The global tier's servers alone: what pi loads in a project it doesn't trust, so an untrusted project's files
    /// can't break them.
    #[serde(serialize_with = "section")]
    pub mcp_global: Section<ResolvedMcp>,
    #[serde(serialize_with = "section")]
    pub web_search: Section<WebSearchFile>,
    #[serde(serialize_with = "section")]
    pub workflows: Section<WorkflowsFile>,
    /// The nearest `integrations.json`, unchanged (the pi package validates it).
    #[serde(serialize_with = "section")]
    pub integrations: Section<Option<FileValue<serde_json::Value>>>,
    /// Every `policies.md`, farthest first.
    #[serde(serialize_with = "section")]
    pub policies: Section<Vec<FileValue<String>>>,
    /// Rule and skill folders from the `.agents/` chain.
    #[serde(serialize_with = "section")]
    pub instructions: Section<Instructions>,
    /// Workflow artifacts: the source's `.piton/.local`.
    pub artifacts: PathBuf,
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
            permissions,
            mcp,
            mcp_global,
            web_search,
            workflows,
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
            files: _,
            watch: _,
        } = self;
        [
            ("daemon", daemon.as_ref().err()),
            ("settings", settings.as_ref().err()),
            ("permissions", permissions.as_ref().err()),
            ("mcp", mcp.as_ref().err()),
            ("mcp_global", mcp_global.as_ref().err()),
            ("web_search", web_search.as_ref().err()),
            ("workflows", workflows.as_ref().err()),
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

/// The merged permission rules, and each file they came from.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPermissions {
    pub merged: PermissionsFile,
    /// Each permission file of the current runtime that exists, in merge order, with its own contents: for
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
    /// `shared` (`permissions[.local].json`) or the runtime (`permissions.<runtime>[.local].json`).
    pub scope: PermissionScope,
    /// A `.local` file.
    pub local: bool,
    pub settings: PermissionsFile,
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

#[derive(Debug, Clone, Serialize)]
pub struct FileOrigin {
    pub path: PathBuf,
    pub level: LevelKind,
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

/// Collects files in merge order and remembers what to watch.
struct Reader {
    files: Vec<FileOrigin>,
    watch: Vec<PathBuf>,
}

impl Reader {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            watch: Vec::new(),
        }
    }

    /// The level's files `<stem>[.<runtime>].<ext>` then `<stem>[.<runtime>].local.<ext>`, in merge order.
    fn names(stem: &str, ext: &str, runtime: Option<Runtime>) -> Vec<String> {
        let mut names = vec![format!("{stem}.{ext}")];
        if let Some(runtime) = runtime {
            names.push(format!("{stem}.{}.{ext}", runtime.name()));
        }
        names.push(format!("{stem}.local.{ext}"));
        if let Some(runtime) = runtime {
            names.push(format!("{stem}.{}.local.{ext}", runtime.name()));
        }
        names
    }

    /// Watches every candidate of a file kind in every level, existing or not, whether or not the kind can be read:
    /// fixing a broken or moved file must invalidate a client's cache.
    fn watch_kind(&mut self, levels: &[Level], stem: &str, format: Format, runtime: Option<Runtime>) {
        for level in levels {
            for name in Self::names(stem, ext(format), runtime) {
                self.watch.push(level.dir.join(name));
            }
        }
    }

    fn read<T>(
        &mut self,
        levels: &[Level],
        stem: &str,
        format: Format,
        runtime: Option<Runtime>,
    ) -> Result<Vec<Loaded<T>>, SettingsError>
    where
        T: serde::de::DeserializeOwned + Scoped,
    {
        let mut loaded = Vec::new();
        for level in levels {
            for name in Self::names(stem, ext(format), runtime) {
                let path = level.dir.join(name);
                if let Some(file) = load::<T>(&path, level.kind, format)? {
                    self.files.push(FileOrigin {
                        path: path.clone(),
                        level: level.kind,
                    });
                    loaded.push(file);
                }
            }
        }
        Ok(loaded)
    }

    fn text(&mut self, path: PathBuf, level: LevelKind) -> Result<Option<String>, SettingsError> {
        let text = read_text(&path)?;
        if text.is_some() {
            self.files.push(FileOrigin { path, level });
        }
        Ok(text)
    }
}

fn ext(format: Format) -> &'static str {
    match format {
        Format::Toml => "toml",
        Format::Json => "json",
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

/// Only the global tier: the daemon's own settings and the pi package's personal ones (`workflowProject`,
/// `sessionAnalysis`), which never depend on a project.
pub fn resolve_global(paths: &Paths) -> Result<Resolved, SettingsError> {
    let chain = Chain::global_only(&paths.global);
    read_chain(&chain, &paths.global, &paths.home, Runtime::Host, &old_global(paths))
}

/// The daemon's part of a workspace's configuration.
#[derive(Debug, Clone)]
pub struct DaemonSettings {
    /// The merged `daemon[.local].toml` of the `.piton/` chain: sandbox grants, workers.
    pub daemon: DaemonFile,
    /// The checkout whose `.piton/` was read (a worker's source).
    pub source: PathBuf,
    /// The existing level folders of both chains, farthest first: a sandbox reads its whole configuration.
    pub levels: Vec<PathBuf>,
}

/// The daemon's part of a workspace's configuration: the merged `daemon[.local].toml` of the `.piton/` chain and the
/// chains' existing folders. Refuses only the old files the daemon itself read; the pi package's files are checked by
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
    let mut reader = Reader::new();
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
    let mut daemon = DaemonFile::default();
    for file in reader.read::<DaemonFile>(&piton, "daemon", Format::Toml, None)? {
        validate_daemon(&file.value, &file.origin.path)?;
        merge::daemon(&mut daemon, file.value);
    }
    Ok(DaemonSettings {
        daemon,
        source: chain.source.clone(),
        levels,
    })
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
    let mut reader = Reader::new();
    let mut piton = Vec::new();
    for level in &chain.piton {
        reader.watch.push(level.dir.clone());
        if check_level(level)? {
            piton.push(level.clone());
        }
    }
    for level in &chain.agents {
        reader.watch.push(level.dir.clone());
        check_level(level)?;
    }
    // Every candidate, whether or not its part can be read: fixing a broken or moved file changes the result.
    reader.watch.extend(old.iter().map(|old| old.path.clone()));
    for (stem, format, by_runtime) in [
        ("daemon", Format::Toml, None),
        ("settings", Format::Toml, None),
        ("permissions", Format::Json, Some(runtime)),
        ("mcp", Format::Json, None),
        ("web-search", Format::Json, None),
        ("workflows", Format::Json, None),
    ] {
        reader.watch_kind(&piton, stem, format, by_runtime);
    }
    for level in &piton {
        reader.watch.push(level.dir.join("integrations.json"));
        reader.watch.push(level.dir.join("policies.md"));
    }
    let global_levels: Vec<Level> = piton
        .iter()
        .filter(|level| level.kind == LevelKind::Global)
        .cloned()
        .collect();

    let daemon = part(old, Part::Daemon, || {
        let mut daemon = DaemonFile::default();
        for file in reader.read::<DaemonFile>(&piton, "daemon", Format::Toml, None)? {
            validate_daemon(&file.value, &file.origin.path)?;
            merge::daemon(&mut daemon, file.value);
        }
        Ok(daemon)
    });

    let settings = part(old, Part::Settings, || {
        for level in piton.iter().filter(|level| level.kind != LevelKind::Global) {
            for name in ["settings.toml", "settings.local.toml"] {
                let path = level.dir.join(name);
                if path.exists() {
                    return Err(SettingsError::GlobalOnly {
                        path,
                        key: name.into(),
                        global: chain.global.clone(),
                    });
                }
            }
        }
        let mut settings = SettingsFile::default();
        for file in reader.read::<SettingsFile>(&global_levels, "settings", Format::Toml, None)? {
            merge::settings(&mut settings, file.value);
        }
        Ok(settings)
    });

    let permissions = part(old, Part::Permissions, || {
        read_permissions(&mut reader, &piton, runtime)
    });
    let mcp = part(old, Part::Mcp, || read_mcp(&mut reader, &piton));
    let mcp_global = part(old, Part::McpGlobal, || read_mcp(&mut reader, &global_levels));

    let web_search = part(old, Part::WebSearch, || {
        let mut web_search = WebSearchFile::default();
        for file in reader.read::<WebSearchFile>(&piton, "web-search", Format::Json, None)? {
            merge::web_search(&mut web_search, file.value);
        }
        Ok(web_search)
    });

    let workflows = part(old, Part::Workflows, || {
        let mut workflows = WorkflowsFile::default();
        for file in reader.read::<WorkflowsFile>(&piton, "workflows", Format::Json, None)? {
            merge::workflows(&mut workflows, file.value);
        }
        Ok(workflows)
    });

    let integrations = part(old, Part::Integrations, || {
        let mut nearest = None;
        for level in &piton {
            let path = level.dir.join("integrations.json");
            if let Some(text) = reader.text(path.clone(), level.kind)? {
                let value = serde_json::from_str(&text).map_err(|e| SettingsError::Invalid {
                    path: path.clone(),
                    reason: e.to_string(),
                })?;
                nearest = Some(FileValue { path, value });
            }
        }
        Ok(nearest)
    });

    let policies = part(old, Part::Policies, || {
        let mut policies = Vec::new();
        for level in &piton {
            let path = level.dir.join("policies.md");
            if let Some(text) = reader.text(path.clone(), level.kind)? {
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
        permissions,
        mcp,
        mcp_global,
        web_search,
        workflows,
        integrations,
        policies,
        instructions,
        artifacts: chain.source.join(LOCAL_DIR),
        files: reader.files,
        watch: reader.watch,
    })
}

fn read_permissions(
    reader: &mut Reader,
    levels: &[Level],
    runtime: Runtime,
) -> Result<ResolvedPermissions, SettingsError> {
    let mut merged = PermissionsFile::default();
    let mut sources = Vec::new();
    for file in reader.read::<PermissionsFile>(levels, "permissions", Format::Json, Some(runtime))? {
        let name = file
            .origin
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let scope = match runtime {
            Runtime::Host if name.starts_with("permissions.host.") => PermissionScope::Host,
            Runtime::Sandbox if name.starts_with("permissions.sandbox.") => PermissionScope::Sandbox,
            Runtime::Host | Runtime::Sandbox => PermissionScope::Shared,
        };
        sources.push(PermissionSource {
            path: file.origin.path.clone(),
            level: file.origin.level,
            scope,
            local: name.ends_with(".local.json"),
            settings: file.value.clone(),
        });
        merge::permissions(&mut merged, file.value);
    }
    Ok(ResolvedPermissions { merged, sources })
}

fn read_mcp(reader: &mut Reader, levels: &[Level]) -> Result<ResolvedMcp, SettingsError> {
    let mut mcp = ResolvedMcp::default();
    for file in reader.read::<McpFile>(levels, "mcp", Format::Json, None)? {
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

/// Files of the layout before `.piton/` in the folders whose files are read: the source checkout and its parent
/// folders. A worker's own copies are never read (its settings come from its source), so a worker cloned before its
/// source was migrated doesn't block anything.
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
    const DAEMON: &[Part] = &[Part::Daemon];
    const PROJECT: &[Part] = &[Part::Workflows, Part::Integrations, Part::Policies];
    const PERMISSIONS: &[Part] = &[Part::Permissions];
    const MCP: &[Part] = &[Part::Mcp];
    const WEB_SEARCH: &[Part] = &[Part::WebSearch];
    for dir in dirs {
        for (name, now, parts) in [
            (".piton.toml", ".piton/daemon.toml [workers]", DAEMON),
            (".nono.json", ".piton/daemon.toml [sandbox]", DAEMON),
            (".nono.local.json", ".piton/daemon.local.toml [sandbox]", DAEMON),
            (
                ".project",
                ".piton/ (workflows.json, integrations.json, policies.md, .local/)",
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
                    now: "the same name in .piton/",
                    parts,
                });
            }
        }
        for name in ["nono.json", "nono.local.json"] {
            let path = dir.join(PITON_DIR).join(name);
            if path.exists() {
                found.push(OldPath {
                    path,
                    now: ".piton/daemon[.local].toml [sandbox]",
                    parts: DAEMON,
                });
            }
        }
    }
    found
}

/// The global files before the global tier: `config.toml` and the files piton kept in pi's agent folder.
fn old_global(paths: &Paths) -> Vec<OldPath> {
    let mut found = Vec::new();
    let config = paths.global.join("config.toml");
    if config.exists() {
        found.push(OldPath {
            path: config,
            now: "daemon.toml (how the daemon runs) and settings.toml [host] (roots, logging)",
            parts: &[Part::Daemon, Part::Settings],
        });
    }
    let pi_dir = &paths.pi_agent_dir;
    let both_mcp: &'static [Part] = &[Part::Mcp, Part::McpGlobal];
    for (name, parts) in [
        ("permissions.json", &[Part::Permissions][..]),
        ("permissions.host.json", &[Part::Permissions][..]),
        ("permissions.sandbox.json", &[Part::Permissions][..]),
        ("mcp.json", both_mcp),
        ("web-search.json", &[Part::WebSearch][..]),
        ("workflows.json", &[Part::Workflows][..]),
    ] {
        let path = pi_dir.join(name);
        if path.exists() {
            found.push(OldPath {
                path,
                now: "the same name in the global tier",
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

    /// The whole chain, every file kind: global tier, a parent folder, and the repository.
    #[test]
    fn merges_the_chain() {
        let t = tree();
        let parent = t.home.join("Projects/work");
        let repo = parent.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        write(
            &t.global.join("daemon.toml"),
            "[sandbox]\nbase = \"mise-dev\"\ngroups = [\"git_config\"]\nallow = [\"~/.cargo/registry\"]\n[sandbox.network]\nopen_port = [0]\nprofile = false\n",
        );
        write(
            &t.global.join("permissions.json"),
            r#"{"deny":["bash(rm -rf *)"],"ask":["write"]}"#,
        );
        write(
            &t.global.join("permissions.sandbox.json"),
            r#"{"defaultMode":"allow","ask":["bash(git push *)"]}"#,
        );
        write(&t.global.join("permissions.host.json"), r#"{"defaultMode":"ask"}"#);
        write(
            &t.global.join("mcp.json"),
            r#"{"mcpServers":{"github":{"url":"https://a"},"docs":{"command":"d"}}}"#,
        );
        write(&t.global.join("workflows.json"), r#"{"sessionAnalysis":true}"#);
        write(&t.global.join("settings.toml"), "[host]\nroots = [\"~/Projects\"]\n");
        write(
            &t.global.join("settings.local.toml"),
            "[host]\nroots = [\"~/Projects/personal\"]\n",
        );
        write(&t.global.join("policies.md"), "global policy\n");
        write(&t.global.join("agents/rules/a.md"), "rule\n");
        write(
            &parent.join(".piton/daemon.toml"),
            "[sandbox]\nallow = [\"~/Projects/work/.pi\"]\n",
        );
        write(
            &parent.join(".piton/integrations.json"),
            r#"{"tracker":{"provider":"github"}}"#,
        );
        write(
            &repo.join(".piton/daemon.toml"),
            "[workers]\nskip = [\"apps/.build\"]\n[sandbox]\nread = [\"/r\"]\n",
        );
        write(
            &repo.join(".piton/daemon.local.toml"),
            "[sandbox]\nallow = [\"/local\"]\n",
        );
        write(
            &repo.join(".piton/permissions.json"),
            r#"{"allow":["read"],"defaultMode":"ask"}"#,
        );
        write(
            &repo.join(".piton/mcp.local.json"),
            r#"{"mcpServers":{"github":{"url":"https://b"}}}"#,
        );
        write(
            &repo.join(".piton/workflows.json"),
            r#"{"branches":{"format":"identifier-title"}}"#,
        );
        write(
            &repo.join(".piton/integrations.json"),
            r#"{"tracker":{"provider":"linear"}}"#,
        );
        write(&repo.join(".piton/policies.md"), "repo policy\n");

        let r = run(&t, &source(repo.clone()), Runtime::Sandbox).unwrap();
        assert!(r.errors().is_empty());
        let daemon = r.daemon.as_ref().unwrap();
        let s = &daemon.sandbox;
        assert_eq!(s.base.as_deref(), Some("mise-dev"));
        assert_eq!(s.groups, ["git_config"]);
        assert_eq!(s.allow, ["~/.cargo/registry", "~/Projects/work/.pi", "/local"]);
        assert_eq!(s.read, ["/r"]);
        assert_eq!(s.network.open_port, [0]);
        assert_eq!(daemon.workers.skip, ["apps/.build"]);

        // Sandbox runtime: the host file isn't read; the repository's defaultMode is nearest.
        let permissions = &r.permissions.as_ref().unwrap().merged;
        assert_eq!(permissions.deny, ["bash(rm -rf *)"]);
        assert_eq!(permissions.ask, ["write", "bash(git push *)"]);
        assert_eq!(permissions.allow, ["read"]);
        assert_eq!(
            permissions.default_mode,
            Some(crate::settings::files::PermissionMode::Ask)
        );

        // MCP: the nearer entry replaces the whole entry, order kept; the global tier's alone stay apart.
        let mcp = r.mcp.as_ref().unwrap();
        let names: Vec<_> = mcp.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["github", "docs"]);
        assert_eq!(mcp.servers[0].config["url"], "https://b");
        assert_eq!(r.mcp_global.as_ref().unwrap().servers[0].config["url"], "https://a");

        let workflows = r.workflows.as_ref().unwrap();
        assert_eq!(workflows.session_analysis, Some(true));
        assert_eq!(
            workflows.branches.format,
            Some(crate::settings::files::BranchFormat::IdentifierTitle)
        );
        assert_eq!(
            r.settings.as_ref().unwrap().host.roots.as_deref(),
            Some(&["~/Projects/personal".to_string()][..])
        );
        let integrations = r.integrations.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(integrations.path, repo.join(".piton/integrations.json"));
        assert_eq!(integrations.value["tracker"]["provider"], "linear");
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
        assert!(r.watch.contains(&repo.join(".piton/permissions.sandbox.local.json")));
        assert!(r.files.iter().any(|f| f.path == repo.join(".piton/daemon.local.toml")));

        let host = run(&t, &source(repo), Runtime::Host).unwrap();
        let permissions = &host.permissions.as_ref().unwrap().merged;
        assert_eq!(permissions.ask, ["write"]);
        assert_eq!(
            permissions.default_mode,
            Some(crate::settings::files::PermissionMode::Ask)
        );
    }

    #[test]
    fn global_only_keys_in_a_repository_are_refused() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".piton/daemon.toml"), "[sandbox]\nbase = \"default\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.daemon, Err(SettingsError::GlobalOnly { .. })));
        std::fs::remove_file(repo.join(".piton/daemon.toml")).unwrap();
        write(&repo.join(".piton/settings.toml"), "[host]\n");
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.settings, Err(SettingsError::GlobalOnly { .. })));
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

    /// A broken file fails only its own part: a malformed `integrations.json` leaves permissions usable.
    #[test]
    fn a_broken_file_fails_only_its_part() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".piton/integrations.json"), "{ broken");
        write(&repo.join(".piton/workflows.json"), r#"{"unknown":1}"#);
        write(&repo.join(".piton/permissions.json"), r#"{"allow":["read"]}"#);
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.integrations, Err(SettingsError::Invalid { .. })));
        assert!(matches!(r.workflows, Err(SettingsError::Invalid { .. })));
        assert_eq!(r.permissions.unwrap().merged.allow, ["read"]);
        assert!(r.mcp.is_ok() && r.policies.is_ok() && r.instructions.is_ok());
        // The broken files stay watched, so fixing them invalidates a client's cache.
        assert!(r.watch.contains(&repo.join(".piton/integrations.json")));
        assert!(r.watch.contains(&repo.join(".piton/workflows.json")));
    }

    #[test]
    fn the_old_layout_is_refused_by_the_parts_that_read_it() {
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
        assert!(matches!(r.workflows, Err(SettingsError::OldLayout(_))));
        assert!(matches!(r.integrations, Err(SettingsError::OldLayout(_))));
        assert!(r.mcp.is_ok() && r.web_search.is_ok() && r.instructions.is_ok());
        assert!(r.watch.contains(&repo.join(".nono.json")));
        // Rules and skills in .agents/ are the new layout, not old.
        for old in [".nono.json", ".agents/permissions.json"] {
            std::fs::remove_file(repo.join(old)).unwrap();
        }
        std::fs::remove_dir_all(repo.join(".project")).unwrap();
        write(&repo.join(".agents/rules/r.md"), "x");
        assert!(run(&t, &source(repo), Runtime::Host).unwrap().errors().is_empty());

        write(&t.global.join("config.toml"), "roots = []\n");
        let global = resolve_global(&t.paths).unwrap();
        assert!(matches!(global.daemon, Err(SettingsError::OldLayout(_))));
        assert!(matches!(global.settings, Err(SettingsError::OldLayout(_))));
        std::fs::remove_file(t.global.join("config.toml")).unwrap();
        // Files piton kept in pi's agent folder refuse only their own part, never the daemon's.
        write(&t.paths.pi_agent_dir.join("permissions.json"), "{}");
        let global = resolve_global(&t.paths).unwrap();
        assert!(global.daemon.is_ok() && global.settings.is_ok());
        let repo = t.home.join("clean");
        std::fs::create_dir_all(&repo).unwrap();
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.permissions, Err(SettingsError::OldLayout(_))));
        assert!(r.workflows.is_ok());
    }

    /// A worker reads `.piton/` from its source, never its own copy.
    #[test]
    fn a_worker_reads_its_sources_settings() {
        let t = tree();
        let src = t.home.join("p/repo");
        let worker = t.home.join("p/repo--w");
        write(&src.join(".piton/permissions.json"), r#"{"deny":["bash(*)"]}"#);
        write(&worker.join(".piton/permissions.json"), r#"{"allow":["bash(*)"]}"#);
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
