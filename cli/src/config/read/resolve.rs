//! A chain of folders into one merged value, or one error, per part: each table of `config.toml`, the MCP servers,
//! policies, and instruction folders.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::parse::{ConfigDoc, FileOrigin, load_mcp, read_config, read_text};
use crate::{
    config::{
        ConfigError,
        model::{
            grants::{GrantEntry, GrantFilter, Grants},
            key::Location,
            level::{Chain, Level, LevelKind},
            paths::{FilePair, Paths, Sharing},
            tables::{
                BranchesTable, Merge, Origin, PermissionSettings, RoleTable, SandboxSettings, SandboxTable, Table,
                TableValue, ToolsTable, TrustTable, WebSearchTable, WorkersTable, forge::ForgeProvider,
                tracker::TrackerProvider,
            },
        },
    },
    project::Workspace,
};

/// Where pi's permission rules apply: everywhere, or only in one launch mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Host,
    Sandbox,
}

/// An integration role (`[tracker]` or `[forge]`) as the nearest table sets it. Serialized as `{"state": "on", "path",
/// "config"}`, `{"state": "off", "reason": "absent"}`, or `{"state": "off", "reason": "disabled", "path"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Role<P> {
    On { path: PathBuf, config: P },
    Off(RoleOff),
}

/// Why a role is off.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RoleOff {
    /// No table in the chain.
    Absent,
    /// `provider = "none"` in the nearest table.
    Disabled { path: PathBuf },
}

/// One part of a workspace's configuration: its merged value, or why its files can't be used. Each consumer fails only
/// on the parts it reads, so a broken `[tracker]` never stops permission checks. Serialized as `{"ok": value}` or
/// `{"error": {"message": ...}}`.
pub type Section<T> = Result<T, ConfigError>;

/// Everything resolved for one workspace: one part per table of `config.toml`, the MCP servers, policies, and
/// instruction folders. The global tier's host and client files aren't part of a workspace (see `read::global`).
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
    pub sandbox: Section<SandboxTable>,
    #[serde(serialize_with = "section")]
    pub workers: Section<WorkersTable>,
    #[serde(serialize_with = "section")]
    pub tools: Section<ResolvedTools>,
    #[serde(serialize_with = "section")]
    pub branches: Section<BranchesTable>,
    /// The tracker: the nearest `[tracker]`, whole and validated.
    #[serde(serialize_with = "section")]
    pub tracker: Section<Role<TrackerProvider>>,
    /// The forge: the nearest `[forge]`, likewise.
    #[serde(serialize_with = "section")]
    pub forge: Section<Role<ForgeProvider>>,
    #[serde(serialize_with = "section")]
    pub web_search: Section<WebSearchTable>,
    /// `[trust]` of the user's own files.
    #[serde(serialize_with = "section")]
    pub trust: Section<TrustTable>,
    /// Every level's servers.
    #[serde(serialize_with = "section")]
    pub mcp: Section<ResolvedMcp>,
    /// The global tier's servers alone: what pi loads in a project it doesn't trust, so an untrusted project's files
    /// can't break them.
    #[serde(serialize_with = "section")]
    pub mcp_global: Section<ResolvedMcp>,
    /// Every `policies.md`, farthest first.
    #[serde(serialize_with = "section")]
    pub policies: Section<Vec<FileValue<String>>>,
    /// Rule and skill folders from the `.agents/` chain.
    #[serde(serialize_with = "section")]
    pub instructions: Section<Instructions>,
    /// The repository's committed grants left out because the user hasn't trusted them (`crate::trust`).
    pub untrusted: Vec<GrantEntry>,
    /// Every file read, with its level.
    pub files: Vec<FileOrigin>,
    /// Every path whose appearance, change, or removal changes the result: each level folder and each candidate
    /// file, existing or not. Clients cache the result until one of these changes.
    pub watch: Vec<PathBuf>,
}

impl Resolved {
    /// Every part that can't be used, by its name in the output.
    pub fn errors(&self) -> Vec<(&'static str, &ConfigError)> {
        let Resolved {
            sandbox,
            workers,
            tools,
            branches,
            tracker,
            forge,
            web_search,
            trust,
            mcp,
            mcp_global,
            policies,
            instructions,
            untrusted: _,
            workspace: _,
            source: _,
            checkout: _,
            global: _,
            home: _,
            runtime: _,
            files: _,
            watch: _,
        } = self;
        [
            ("sandbox", sandbox.as_ref().err()),
            ("workers", workers.as_ref().err()),
            ("tools", tools.as_ref().err()),
            ("branches", branches.as_ref().err()),
            ("tracker", tracker.as_ref().err()),
            ("forge", forge.as_ref().err()),
            ("web_search", web_search.as_ref().err()),
            ("trust", trust.as_ref().err()),
            ("mcp", mcp.as_ref().err()),
            ("mcp_global", mcp_global.as_ref().err()),
            ("policies", policies.as_ref().err()),
            ("instructions", instructions.as_ref().err()),
        ]
        .into_iter()
        .filter_map(|(name, error)| error.map(|error| (name, error)))
        .collect()
    }
}

pub(super) fn section<T: Serialize, S: serde::Serializer>(
    value: &Section<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
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

/// `[tools]` resolved: its permission rules.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedTools {
    pub permissions: ResolvedPermissions,
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
    /// `shared` (`[tools.permissions]`) or the runtime (`[tools.permissions.<runtime>]`).
    pub scope: PermissionScope,
    /// Which file of the level's pair.
    pub sharing: Sharing,
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
    Invalid { at: Option<Location>, reason: String },
}

impl Doc {
    fn get(&self) -> Result<&ConfigDoc, ConfigError> {
        match self {
            Doc::Read(doc) => Ok(doc),
            Doc::Failed { path, error } => Err(match error {
                FailedRead::Io(kind, message) => ConfigError::Read {
                    path: path.clone(),
                    source: std::io::Error::new(*kind, message.clone()),
                },
                FailedRead::Invalid { at, reason } => ConfigError::Invalid {
                    path: path.clone(),
                    at: at.clone(),
                    reason: reason.clone(),
                },
            }),
        }
    }
}

/// Every level's configuration files, read once, in merge order: per level the shared file, then the local one. The
/// repository's committed file's grants are kept only as `filter` says (`crate::trust` decides it).
struct Docs<'a> {
    docs: Vec<Doc>,
    filter: &'a GrantFilter,
    /// What the filter left out, per part: what `piton sandbox show` and `piton config check` name.
    left_out: std::cell::RefCell<Vec<GrantEntry>>,
}

/// Whether a file's grants are gated by trust: only a repository's committed `config.toml`. Parent folders' files, the
/// global tier, and every `config.local.toml` are the user's own.
fn gated(doc: &ConfigDoc) -> bool {
    match (doc.origin.level, doc.sharing) {
        (LevelKind::Repository, Sharing::Shared) => true,
        (LevelKind::Global | LevelKind::Parent, Sharing::Shared | Sharing::Local)
        | (LevelKind::Repository, Sharing::Local) => false,
    }
}

impl<'a> Docs<'a> {
    fn read(levels: &[Level], files: &mut Vec<FileOrigin>, filter: &'a GrantFilter) -> Self {
        let mut docs = Vec::new();
        for level in levels {
            let pair = FilePair::project(&level.dir);
            for (sharing, path) in pair.paths() {
                let path = path.to_path_buf();
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
                            ConfigError::Read { source, .. } => FailedRead::Io(source.kind(), source.to_string()),
                            ConfigError::Invalid { at, reason, .. } => FailedRead::Invalid { at, reason },
                            other @ (ConfigError::NotInProjectConfig { .. }
                            | ConfigError::ProjectOnly { .. }
                            | ConfigError::LocalOnly { .. }
                            | ConfigError::Conflict { .. }) => FailedRead::Invalid {
                                at: None,
                                reason: other.to_string(),
                            },
                        };
                        docs.push(Doc::Failed { path, error });
                    }
                }
            }
        }
        Self {
            docs,
            filter,
            left_out: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Each file's `table` part that it sets, in merge order, the repository's committed one gated. A file that can't
    /// be read fails every part.
    fn parts<T: TableValue>(&self, table: Table) -> Result<Vec<(T, &ConfigDoc)>, ConfigError> {
        let mut parts = Vec::new();
        for doc in &self.docs {
            let doc = doc.get()?;
            if let Some(value) = doc.part::<T>(table)? {
                let value = match gated(doc) {
                    true => {
                        let (value, out) = value.gated(self.filter);
                        self.left_out.borrow_mut().extend(out);
                        value
                    }
                    false => value,
                };
                parts.push((value, doc));
            }
        }
        Ok(parts)
    }

    /// What the repository's committed file grants, in every gated part: what trust is asked about. A part that can't
    /// be read grants nothing here; its error is the part's own.
    fn grants(&self) -> Grants {
        let mut grants = Grants::new();
        for doc in &self.docs {
            let Ok(doc) = doc.get() else { continue };
            if !gated(doc) {
                continue;
            }
            if let Ok(Some(table)) = doc.part::<SandboxTable>(Table::Sandbox) {
                grants.extend(table.grants());
            }
            if let Ok(Some(table)) = doc.part::<ToolsTable>(Table::Tools) {
                grants.extend(TableValue::grants(&table));
            }
            if let Ok(Some(table)) = doc.part::<WorkersTable>(Table::Workers) {
                grants.extend(TableValue::grants(&table));
            }
        }
        grants
    }

    /// One part merged across the chain, farthest first, by its table's merge rule.
    fn merged<T: TableValue + Merge>(&self, table: Table) -> Section<T> {
        let mut merged = T::default();
        for (value, _) in self.parts::<T>(table)? {
            merged.merge(value);
        }
        Ok(merged)
    }

    /// An integration role: the nearest file's table, whole.
    fn role<P>(&self, table: Table) -> Section<Role<P>>
    where
        RoleTable<P>: TableValue,
    {
        Ok(match self.parts::<RoleTable<P>>(table)?.pop() {
            None => Role::Off(RoleOff::Absent),
            Some((RoleTable::Off(_), doc)) => Role::Off(RoleOff::Disabled {
                path: doc.origin.path.clone(),
            }),
            Some((RoleTable::On(config), doc)) => Role::On {
                path: doc.origin.path.clone(),
                config,
            },
        })
    }
}

/// Resolves a workspace: both chains, each part on its own, the repository's committed grants kept as `filter` says.
/// Fails as a whole only when a level folder exists but can't be listed, since skipping it could drop a deny rule.
pub fn resolve(
    workspace: &Workspace,
    paths: &Paths,
    runtime: Runtime,
    filter: &GrantFilter,
) -> Result<Resolved, ConfigError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    read_chain(&chain, &workspace.path, &paths.home, runtime, filter)
}

/// The global tier's `config.toml` alone: the defaults every project inherits, without any project's files.
pub fn resolve_global_project(paths: &Paths) -> Result<Resolved, ConfigError> {
    let chain = Chain::global_only(&paths.global);
    read_chain(&chain, &paths.global, &paths.home, Runtime::Host, &GrantFilter::All)
}

/// What trust is decided from, for a workspace: the repository's committed grants, and the nearest `[trust]` of the
/// user's own files. Reads no other part, and never filters.
#[derive(Debug, Clone)]
pub struct TrustInputs {
    /// The checkout whose `.piton/` was read (a worker's source): what the decision is keyed on.
    pub source: PathBuf,
    /// The workspace's own checkout, where pi reads `.pi/` and `.agents/skills/`.
    pub checkout: PathBuf,
    pub grants: Grants,
    pub trust: TrustTable,
    /// The repository's `.piton/mcp.json` servers, which pi loads only in a trusted project.
    pub mcp_servers: Vec<String>,
}

pub fn trust_inputs(workspace: &Workspace, paths: &Paths) -> Result<TrustInputs, ConfigError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    let (piton, _) = existing_levels(&chain)?;
    let docs = Docs::read(&piton, &mut Vec::new(), &GrantFilter::All);
    let repository: Vec<Level> = piton
        .iter()
        .filter(|level| level.kind == LevelKind::Repository)
        .cloned()
        .collect();
    // Only the committed file: `mcp.local.json` is the user's own.
    let mcp_servers = repository
        .iter()
        .filter_map(|level| load_mcp(&level.dir.join("mcp.json")).ok().flatten())
        .flat_map(|file| file.mcp_servers.keys().cloned().collect::<Vec<_>>())
        .collect();
    Ok(TrustInputs {
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        grants: docs.grants(),
        trust: docs.merged(Table::Trust)?,
        mcp_servers,
    })
}

/// What a launch or a new worker needs from a workspace's configuration.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// The `[sandbox]` of the `.piton/` chain, each entry with the file it came from.
    pub sandbox: SandboxSettings,
    /// The merged `[workers]`.
    pub workers: WorkersTable,
    /// The checkout whose `.piton/` was read (a worker's source).
    pub source: PathBuf,
    /// The workspace's own checkout, where pi reads `.pi/` and `.agents/skills/`.
    pub checkout: PathBuf,
    /// The existing level folders of both chains, farthest first: a sandbox reads its whole configuration.
    pub levels: Vec<PathBuf>,
    /// The repository's committed `[sandbox]` grants the filter left out: what `piton sandbox show` names.
    pub untrusted: Vec<GrantEntry>,
}

/// The parts of a workspace's configuration a launch reads: `[sandbox]` and `[workers]` merged along the `.piton/`
/// chain, the repository's committed grants kept as `filter` says, and the chains' existing folders. Only these two
/// parts can refuse a launch; the pi package's parts are checked by [`resolve`] in the pi package.
pub fn resolve_launch(workspace: &Workspace, paths: &Paths, filter: &GrantFilter) -> Result<LaunchConfig, ConfigError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    let (piton, levels) = existing_levels(&chain)?;
    let docs = Docs::read(&piton, &mut Vec::new(), filter);
    let mut sandbox = SandboxSettings::default();
    for (table, doc) in docs.parts::<SandboxTable>(Table::Sandbox)? {
        let origin = match gated(doc) {
            true => Origin::Committed,
            false => Origin::UsersOwn,
        };
        sandbox.add(table, &doc.origin.path, origin);
    }
    let workers = docs.merged(Table::Workers)?;
    Ok(LaunchConfig {
        sandbox,
        workers,
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        levels,
        untrusted: docs.left_out.take(),
    })
}

/// A workspace's existing level folders of both chains, farthest first, without reading their files: what
/// [`resolve_launch`] closes in the sandbox, and what `piton config protected` protects outside a session.
pub fn launch_levels(workspace: &Workspace, paths: &Paths) -> Result<Vec<PathBuf>, ConfigError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    Ok(existing_levels(&chain)?.1)
}

/// The existing `.piton/` levels, and every existing level folder of both chains, farthest first.
fn existing_levels(chain: &Chain) -> Result<(Vec<Level>, Vec<PathBuf>), ConfigError> {
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
    Ok((piton, levels))
}

/// A level folder that exists but can't be listed is an error: skipping it would silently drop a deny rule.
fn check_level(level: &Level) -> Result<bool, ConfigError> {
    match std::fs::read_dir(&level.dir) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
            Err(ConfigError::invalid(&level.dir, "must be a folder"))
        }
        Err(source) => Err(ConfigError::Read {
            path: level.dir.clone(),
            source,
        }),
    }
}

fn read_chain(
    chain: &Chain,
    workspace: &Path,
    home: &Path,
    runtime: Runtime,
    filter: &GrantFilter,
) -> Result<Resolved, ConfigError> {
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
    // Every candidate, whether or not its part can be read: fixing a broken file changes the result.
    for level in &piton {
        let pair = FilePair::project(&level.dir);
        watch.extend(pair.paths().map(|(_, path)| path.to_path_buf()));
        for name in ["mcp.json", "mcp.local.json", "policies.md"] {
            watch.push(level.dir.join(name));
        }
    }
    let global_levels: Vec<Level> = piton
        .iter()
        .filter(|level| level.kind == LevelKind::Global)
        .cloned()
        .collect();
    let docs = Docs::read(&piton, &mut files, filter);

    let mcp = read_mcp(&mut files, &piton);
    let mcp_global = read_mcp(&mut Vec::new(), &global_levels);
    let policies = read_policies(&mut files, &piton);
    // Every candidate folder, existing or not: clients watch them, and a missing one simply has no files.
    let instructions = Ok(Instructions {
        rules: chain.agents.iter().map(|level| level.dir.join("rules")).collect(),
        skills: chain
            .agents
            .iter()
            .filter(|level| level.kind != LevelKind::Repository)
            .map(|level| level.dir.join("skills"))
            .collect(),
    });

    Ok(Resolved {
        workspace: workspace.to_path_buf(),
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        global: chain.global.clone(),
        home: home.to_path_buf(),
        runtime,
        sandbox: docs.merged(Table::Sandbox),
        workers: docs.merged(Table::Workers),
        tools: read_tools(&docs, runtime),
        branches: docs.merged(Table::Branches),
        // Each table whole: a nearer level's replaces a farther one's.
        tracker: docs.role::<TrackerProvider>(Table::Tracker),
        forge: docs.role::<ForgeProvider>(Table::Forge),
        web_search: docs.merged(Table::WebSearch),
        trust: docs.merged(Table::Trust),
        mcp,
        mcp_global,
        policies,
        instructions,
        untrusted: {
            let mut left_out = docs.left_out.take();
            left_out.sort();
            left_out.dedup();
            left_out
        },
        files,
        watch,
    })
}

fn read_policies(files: &mut Vec<FileOrigin>, levels: &[Level]) -> Section<Vec<FileValue<String>>> {
    let mut policies = Vec::new();
    for level in levels {
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
}

/// Per file: `[tools.permissions]`, then the runtime's `[tools.permissions.<runtime>]`; files in merge order.
fn read_tools(docs: &Docs, runtime: Runtime) -> Section<ResolvedTools> {
    let mut merged = PermissionSettings::default();
    let mut sources = Vec::new();
    for (ToolsTable { permissions: table }, doc) in docs.parts::<ToolsTable>(Table::Tools)? {
        let shared = PermissionSettings::shared(&table);
        let (rules, scope) = match runtime {
            Runtime::Host => (table.host, PermissionScope::Host),
            Runtime::Sandbox => (table.sandbox, PermissionScope::Sandbox),
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
                sharing: doc.sharing,
                settings: settings.clone(),
            });
            merged.merge(settings);
        }
    }
    Ok(ResolvedTools {
        permissions: ResolvedPermissions { merged, sources },
    })
}

fn read_mcp(files: &mut Vec<FileOrigin>, levels: &[Level]) -> Section<ResolvedMcp> {
    let mut mcp = ResolvedMcp::default();
    for level in levels {
        for name in ["mcp.json", "mcp.local.json"] {
            let path = level.dir.join(name);
            let Some(file) = load_mcp(&path)? else {
                continue;
            };
            let origin = FileOrigin {
                path,
                level: level.kind,
            };
            files.push(origin.clone());
            if file.auto_enable_codemode.is_some() {
                mcp.auto_enable_codemode = file.auto_enable_codemode;
            }
            for (name, config) in file.mcp_servers {
                let serde_json::Value::Object(config) = config else {
                    return Err(ConfigError::invalid(
                        &origin.path,
                        format!("mcpServers.{name} must be an object"),
                    ));
                };
                add_mcp_server(&mut mcp, name, config, &origin)?;
            }
        }
    }
    Ok(mcp)
}

fn add_mcp_server(
    mcp: &mut ResolvedMcp,
    name: String,
    config: serde_json::Map<String, serde_json::Value>,
    origin: &FileOrigin,
) -> Result<(), ConfigError> {
    let key = name.replace('-', "_");
    if let Some(clash) = mcp
        .servers
        .iter()
        .find(|server| server.name != name && server.name.replace('-', "_") == key)
    {
        return Err(ConfigError::Conflict {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::model::tables::{branches::BranchFormat, permissions::PermissionMode},
        project::Relation,
    };

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

    fn run(t: &Tree, ws: &Workspace, runtime: Runtime) -> Result<Resolved, ConfigError> {
        resolve(ws, &t.paths, runtime, &GrantFilter::All)
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
presets = ["rust"]
[sandbox.filesystem]
"~/.ssh/config" = "read"
"~/.cargo/registry" = "write"
"/r" = "write"

[tools.permissions]
deny = ["bash(rm -rf *)"]
ask = ["write"]
[tools.permissions.sandbox]
default_mode = "allow"
ask = ["bash(git push *)"]
[tools.permissions.host]
default_mode = "ask"
"#,
        );
        write(&t.global.join("config.local.toml"), "[branches]\nusername = \"me\"\n");
        write(
            &t.global.join("mcp.json"),
            r#"{"mcpServers":{"github":{"url":"https://a"},"docs":{"command":"d"}}}"#,
        );
        write(&t.global.join("policies.md"), "global policy\n");
        write(&t.global.join("agents/rules/a.md"), "rule\n");
        write(
            &parent.join(".piton/config.toml"),
            "[sandbox.filesystem]\n\"~/Projects/work/.pi\" = \"write\"\n[tracker]\nprovider = \"none\"\n[forge]\nprovider = \"github\"\nmcp_server = \"github\"\nrepository = { owner = \"o\", repo = \"r\" }\n",
        );
        write(
            &repo.join(".piton/config.toml"),
            r#"
[workers]
skip = ["apps/.build"]
[sandbox]
presets = ["swift"]
[sandbox.filesystem]
"/r" = "deny"
[tools.permissions]
allow = ["read"]
default_mode = "ask"
[branches]
format = "username/identifier-title"
[tracker]
provider = "linear"
mcp_server = "linear"
team = "ENG"
[tracker.statuses.issues]
backlog = "Backlog"
todo = "Todo"
in_progress = "In Progress"
in_review = "In Review"
done = "Done"
canceled = "Canceled"
[tracker.statuses.projects]
backlog = "Backlog"
todo = "Planned"
in_progress = "In Progress"
in_review = "In Review"
done = "Completed"
canceled = "Canceled"
"#,
        );
        write(
            &repo.join(".piton/config.local.toml"),
            "[sandbox.filesystem]\n\"/local\" = \"write\"\n[web_search]\napi_key = \"k\"\n",
        );
        write(
            &repo.join(".piton/mcp.local.json"),
            r#"{"mcpServers":{"github":{"url":"https://b"}}}"#,
        );
        write(&repo.join(".piton/policies.md"), "repo policy\n");

        let r = run(&t, &source(repo.clone()), Runtime::Sandbox).unwrap();
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        let s = r.sandbox.as_ref().unwrap();
        assert_eq!(s.presets, [crate::config::Preset::Rust, crate::config::Preset::Swift]);
        let fs = |key: &str| s.filesystem[&crate::config::PathKey::new(key).unwrap()];
        use crate::config::FileAccess::{Deny, Read, Write};
        assert_eq!(fs("~/.ssh/config"), Read);
        assert_eq!(fs("~/.cargo/registry"), Write);
        assert_eq!(fs("~/Projects/work/.pi"), Write);
        assert_eq!(fs("/local"), Write);
        assert_eq!(fs("/r"), Deny, "the nearer value wins");
        // A launch keeps each entry's file, and whether it's the user's own.
        let launch = resolve_launch(&source(repo.clone()), &t.paths, &GrantFilter::All)
            .unwrap()
            .sandbox;
        let entry = |key: &str| launch.filesystem[&crate::config::PathKey::new(key).unwrap()].clone();
        assert_eq!(entry("~/.cargo/registry").file, t.global.join("config.toml"));
        assert_eq!(entry("~/Projects/work/.pi").file, parent.join(".piton/config.toml"));
        // A parent folder's file is the user's own; only the repository's committed one is gated.
        assert_eq!(entry("~/Projects/work/.pi").origin, Origin::UsersOwn);
        assert_eq!(entry("/local").origin, Origin::UsersOwn);
        assert_eq!(entry("/r").origin, Origin::Committed);
        assert_eq!(r.workers.as_ref().unwrap().skip, ["apps/.build"]);

        // Sandbox runtime: the host table isn't read; the repository's default_mode is nearest.
        let permissions = &r.tools.as_ref().unwrap().permissions;
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
        assert_eq!(r.web_search.as_ref().unwrap().api_key.as_deref(), Some("k"));

        // [tracker] is replaced whole by the nearer level (the parent's turns it off); [forge] comes from the parent.
        let tracker = serde_json::to_value(r.tracker.as_ref().unwrap()).unwrap();
        assert_eq!(tracker["state"], "on");
        assert_eq!(tracker["path"], repo.join(".piton/config.toml").to_str().unwrap());
        assert_eq!(tracker["config"]["team"], "ENG");
        assert_eq!(tracker["config"]["statuses"]["projects"]["todo"], "Planned");
        assert_eq!(
            serde_json::to_value(r.forge.as_ref().unwrap()).unwrap(),
            serde_json::json!({
                "state": "on",
                "path": parent.join(".piton/config.toml"),
                "config": {"provider": "github", "mcp_server": "github", "repository": {"owner": "o", "repo": "r"}},
            })
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
        assert!(r.watch.contains(&repo.join(".piton/config.local.toml")));
        assert!(r.files.iter().any(|f| f.path == repo.join(".piton/config.local.toml")));

        let host = run(&t, &source(repo), Runtime::Host).unwrap();
        let permissions = &host.tools.as_ref().unwrap().permissions.merged;
        assert_eq!(permissions.ask, ["write"]);
        assert_eq!(permissions.default_mode, Some(PermissionMode::Ask));
    }

    #[test]
    fn scope_rules_are_enforced() {
        let t = tree();
        let repo = t.home.join("repo");
        // A committed grant is no longer a placement error: trust decides it (see `committed_grants_follow_the_filter`).
        write(
            &repo.join(".piton/config.toml"),
            "[sandbox.filesystem]\n\"~/.ssh\" = \"read\"\n",
        );
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(r.sandbox.is_ok());
        assert!(r.tools.is_ok());
        // How trust is re-checked is the user's own.
        write(&repo.join(".piton/config.toml"), "[trust]\nreconfirm = \"on-change\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.trust, Err(ConfigError::NotInProjectConfig { .. })));
        // Integrations are project only: a global [tracker] fails its part.
        write(&t.global.join("config.toml"), "[tracker]\nprovider = \"none\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.tracker, Err(ConfigError::ProjectOnly { .. })));
        std::fs::remove_file(t.global.join("config.toml")).unwrap();
        // No table anywhere: the role is off, absent; `provider = "none"` turns it off with its file.
        write(&repo.join(".piton/config.toml"), "[tracker]\nprovider = \"none\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert_eq!(r.forge.unwrap(), Role::Off(RoleOff::Absent));
        assert_eq!(
            r.tracker.unwrap(),
            Role::Off(RoleOff::Disabled {
                path: repo.join(".piton/config.toml")
            })
        );
        // A secret in a shared file.
        write(&repo.join(".piton/config.toml"), "[web_search]\napi_key = \"k\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.web_search, Err(ConfigError::LocalOnly { .. })));
        // A username in a shared file.
        write(&repo.join(".piton/config.toml"), "[branches]\nusername = \"me\"\n");
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(ConfigError::NotInProjectConfig { .. })));
    }

    /// The repository's committed grants apply only as the filter says; its restrictions, and every grant in the
    /// user's own files (a parent folder's, a `config.local.toml`), always.
    #[test]
    fn committed_grants_follow_the_filter() {
        let t = tree();
        let parent = t.home.join("work");
        let repo = parent.join("repo");
        write(
            &repo.join(".piton/config.toml"),
            r#"
[sandbox]
presets = ["rust", "swift"]
[sandbox.filesystem]
"/w" = "write"
"/d" = "deny"
[tools.permissions]
allow = ["bash(*)"]
deny = ["bash(rm *)"]
"#,
        );
        write(
            &repo.join(".piton/config.local.toml"),
            "[tools.permissions]\nallow = [\"read\"]\n",
        );
        write(&parent.join(".piton/config.toml"), "[sandbox]\npresets = [\"node\"]\n");
        let ws = source(repo.clone());
        let inputs = trust_inputs(&ws, &t.paths).unwrap();
        let names: Vec<String> = inputs.grants.iter().map(ToString::to_string).collect();
        assert_eq!(
            names,
            [
                r#"sandbox.filesystem."/w" = "write""#,
                r#"sandbox.presets "rust""#,
                r#"sandbox.presets "swift""#,
                r#"tools.permissions.allow "bash(*)""#,
            ],
            "only the repository's committed file"
        );

        let none = resolve(&ws, &t.paths, Runtime::Host, &GrantFilter::None).unwrap();
        let permissions = &none.tools.as_ref().unwrap().permissions.merged;
        assert_eq!(permissions.allow, ["read"], "the user's own allow stays");
        assert_eq!(permissions.deny, ["bash(rm *)"], "restrictions always apply");
        let sandbox = none.sandbox.as_ref().unwrap();
        assert_eq!(sandbox.presets, [crate::config::Preset::Node]);
        assert_eq!(
            sandbox.filesystem.get(&crate::config::PathKey::new("/d").unwrap()),
            Some(&crate::config::FileAccess::Deny)
        );
        assert!(
            !sandbox
                .filesystem
                .contains_key(&crate::config::PathKey::new("/w").unwrap())
        );
        assert_eq!(none.untrusted.len(), 4);

        let rust = inputs
            .grants
            .iter()
            .find(|g| g.as_str().contains("rust"))
            .unwrap()
            .clone();
        let launch = resolve_launch(&ws, &t.paths, &GrantFilter::Only([rust].into())).unwrap();
        let presets: Vec<_> = launch.sandbox.presets.iter().map(|p| p.value.name()).collect();
        assert_eq!(presets, ["node", "rust"]);
        assert_eq!(launch.untrusted.len(), 2, "swift and /w");

        let all = resolve(&ws, &t.paths, Runtime::Host, &GrantFilter::All).unwrap();
        assert_eq!(
            all.tools.as_ref().unwrap().permissions.merged.allow,
            ["bash(*)", "read"]
        );
        assert!(all.untrusted.is_empty());
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
        assert!(matches!(r.mcp, Err(ConfigError::Conflict { .. })));
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
            "[branches]\nunknown = 1\n[tools.permissions]\nallow = [\"read\"]\n",
        );
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(ConfigError::Invalid { .. })));
        assert_eq!(r.tools.as_ref().unwrap().permissions.merged.allow, ["read"]);
        assert!(r.tracker.is_ok() && r.mcp.is_ok() && r.policies.is_ok() && r.instructions.is_ok());

        // A typo in the global [branches] leaves [sandbox] readable, and launches with it.
        write(
            &t.global.join("config.toml"),
            "[branches]\nformt = 1\n[sandbox]\npresets = [\"rust\"]\n",
        );
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(ConfigError::Invalid { .. })));
        assert_eq!(r.sandbox.as_ref().unwrap().presets, [crate::config::Preset::Rust]);
        assert_eq!(
            resolve_launch(&source(repo.clone()), &t.paths, &GrantFilter::All)
                .unwrap()
                .sandbox
                .presets[0]
                .value,
            crate::config::Preset::Rust
        );
        std::fs::remove_file(t.global.join("config.toml")).unwrap();

        write(&repo.join(".piton/config.toml"), "[tools.permissions\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.tools, Err(ConfigError::Invalid { .. })));
        assert!(matches!(r.sandbox, Err(ConfigError::Invalid { .. })));
        assert!(r.mcp.is_ok() && r.policies.is_ok());
        // The broken file stays watched, so fixing it invalidates a client's cache.
        assert!(r.watch.contains(&repo.join(".piton/config.toml")));
    }

    /// A worker reads `.piton/` from its source, never its own copy.
    #[test]
    fn a_worker_reads_its_sources_settings() {
        let t = tree();
        let src = t.home.join("p/repo");
        let worker = t.home.join("p/repo--w");
        write(
            &src.join(".piton/config.toml"),
            "[tools.permissions]\ndeny = [\"bash(*)\"]\n",
        );
        write(
            &worker.join(".piton/config.toml"),
            "[tools.permissions]\nallow = [\"bash(*)\"]\n",
        );
        let ws = Workspace {
            path: worker.clone(),
            checkout: worker.clone(),
            relation: Relation::Worker {
                source: src.clone(),
                name: "w".into(),
            },
        };
        assert!(resolve_launch(&ws, &t.paths, &GrantFilter::All).is_ok());
        let r = run(&t, &ws, Runtime::Sandbox).unwrap();
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        let permissions = &r.tools.as_ref().unwrap().permissions.merged;
        assert_eq!(permissions.deny, ["bash(*)"]);
        assert!(permissions.allow.is_empty());
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
        assert!(matches!(result, Err(ConfigError::Read { .. })));
    }
}
