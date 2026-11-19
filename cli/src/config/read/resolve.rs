//! A chain of folders into one merged value, or one error, per part: each table of `config.toml`, the MCP servers,
//! policies, and instruction folders.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::parse::{ConfigDoc, FileOrigin, load_mcp, read_config, read_text};
use crate::{
    config::{
        ConfigError,
        model::{
            level::{Chain, Level, LevelKind},
            paths::{FilePair, Paths, Sharing},
            tables::{
                BranchesTable, Integration, Merge, PermissionSettings, PermissionsTable, SandboxTable, Table,
                TableValue, WebSearchTable, WorkersTable,
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
    pub permissions: Section<ResolvedPermissions>,
    #[serde(serialize_with = "section")]
    pub branches: Section<BranchesTable>,
    /// The nearest `[tracker]`, whole and as written (the pi package validates it).
    #[serde(serialize_with = "section")]
    pub tracker: Section<Option<FileValue<serde_json::Value>>>,
    /// The nearest `[forge]`, likewise.
    #[serde(serialize_with = "section")]
    pub forge: Section<Option<FileValue<serde_json::Value>>>,
    #[serde(serialize_with = "section")]
    pub web_search: Section<WebSearchTable>,
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
            permissions,
            branches,
            tracker,
            forge,
            web_search,
            mcp,
            mcp_global,
            policies,
            instructions,
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
            ("permissions", permissions.as_ref().err()),
            ("branches", branches.as_ref().err()),
            ("tracker", tracker.as_ref().err()),
            ("forge", forge.as_ref().err()),
            ("web_search", web_search.as_ref().err()),
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
    Invalid(String),
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
                FailedRead::Invalid(reason) => ConfigError::Invalid {
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
                            ConfigError::Invalid { reason, .. } => FailedRead::Invalid(reason),
                            other @ (ConfigError::NotInProjectConfig { .. }
                            | ConfigError::ProjectOnly { .. }
                            | ConfigError::LocalOnly { .. }
                            | ConfigError::Conflict { .. }) => FailedRead::Invalid(other.to_string()),
                        };
                        docs.push(Doc::Failed { path, error });
                    }
                }
            }
        }
        Self { docs }
    }

    /// Each file's `table` part that it sets, in merge order. A file that can't be read fails every part.
    fn parts<T: TableValue>(&self, table: Table) -> Result<Vec<(T, &ConfigDoc)>, ConfigError> {
        let mut parts = Vec::new();
        for doc in &self.docs {
            let doc = doc.get()?;
            if let Some(value) = doc.part::<T>(table)? {
                parts.push((value, doc));
            }
        }
        Ok(parts)
    }

    /// One part merged across the chain, farthest first, by its table's merge rule.
    fn merged<T: TableValue + Merge>(&self, table: Table) -> Section<T> {
        let mut merged = T::default();
        for (value, _) in self.parts::<T>(table)? {
            merged.merge(value);
        }
        Ok(merged)
    }

    /// The nearest file's table, whole, as JSON.
    fn nearest(&self, table: Table) -> Section<Option<FileValue<serde_json::Value>>> {
        let Some((Integration(value), doc)) = self.parts::<Integration>(table)?.pop() else {
            return Ok(None);
        };
        let value = serde_json::to_value(value).map_err(|e| ConfigError::Invalid {
            path: doc.origin.path.clone(),
            reason: e.to_string(),
        })?;
        Ok(Some(FileValue {
            path: doc.origin.path.clone(),
            value,
        }))
    }
}

/// Resolves a workspace: both chains, each part on its own. Fails as a whole only when a level folder exists but can't
/// be listed, since skipping it could drop a deny rule.
pub fn resolve(workspace: &Workspace, paths: &Paths, runtime: Runtime) -> Result<Resolved, ConfigError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    read_chain(&chain, &workspace.path, &paths.home, runtime)
}

/// The global tier's `config.toml` alone: the defaults every project inherits, without any project's files.
pub fn resolve_global_project(paths: &Paths) -> Result<Resolved, ConfigError> {
    let chain = Chain::global_only(&paths.global);
    read_chain(&chain, &paths.global, &paths.home, Runtime::Host)
}

/// What a launch or a new worker needs from a workspace's configuration.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// The merged `[sandbox]` of the `.piton/` chain.
    pub sandbox: SandboxTable,
    /// The merged `[workers]`.
    pub workers: WorkersTable,
    /// The checkout whose `.piton/` was read (a worker's source).
    pub source: PathBuf,
    /// The workspace's own checkout, where pi reads `.pi/` and `.agents/skills/`.
    pub checkout: PathBuf,
    /// The existing level folders of both chains, farthest first: a sandbox reads its whole configuration.
    pub levels: Vec<PathBuf>,
}

/// The parts of a workspace's configuration a launch reads: `[sandbox]` and `[workers]` merged along the `.piton/`
/// chain, and the chains' existing folders. Only these two parts can refuse a launch; the pi package's parts are
/// checked by [`resolve`] in the pi package.
pub fn resolve_launch(workspace: &Workspace, paths: &Paths) -> Result<LaunchConfig, ConfigError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
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
    Ok(LaunchConfig {
        sandbox: docs.merged(Table::Sandbox)?,
        workers: docs.merged(Table::Workers)?,
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        levels,
    })
}

/// A level folder that exists but can't be listed is an error: skipping it would silently drop a deny rule.
fn check_level(level: &Level) -> Result<bool, ConfigError> {
    match std::fs::read_dir(&level.dir) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Err(ConfigError::Invalid {
            path: level.dir.clone(),
            reason: "must be a folder".into(),
        }),
        Err(source) => Err(ConfigError::Read {
            path: level.dir.clone(),
            source,
        }),
    }
}

fn read_chain(chain: &Chain, workspace: &Path, home: &Path, runtime: Runtime) -> Result<Resolved, ConfigError> {
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
    let docs = Docs::read(&piton, &mut files);

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
        permissions: read_permissions(&docs, runtime),
        branches: docs.merged(Table::Branches),
        // Each table whole: a nearer level's replaces a farther one's.
        tracker: docs.nearest(Table::Tracker),
        forge: docs.nearest(Table::Forge),
        web_search: docs.merged(Table::WebSearch),
        mcp,
        mcp_global,
        policies,
        instructions,
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

/// Per file: `[permissions]`, then the runtime's `[permissions.<runtime>]`; files in merge order.
fn read_permissions(docs: &Docs, runtime: Runtime) -> Section<ResolvedPermissions> {
    let mut merged = PermissionSettings::default();
    let mut sources = Vec::new();
    for (table, doc) in docs.parts::<PermissionsTable>(Table::Permissions)? {
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
    Ok(ResolvedPermissions { merged, sources })
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
                    return Err(ConfigError::Invalid {
                        path: origin.path.clone(),
                        reason: format!("mcpServers.{name} must be an object"),
                    });
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
presets = ["rust"]
expose = ["~/.ssh/config"]
allow = ["~/.cargo/registry"]

[permissions]
deny = ["bash(rm -rf *)"]
ask = ["write"]
[permissions.sandbox]
default_mode = "allow"
ask = ["bash(git push *)"]
[permissions.host]
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
            "[sandbox]\nallow = [\"~/Projects/work/.pi\"]\n[tracker]\nprovider = \"github\"\n[forge]\nprovider = \"github\"\n",
        );
        write(
            &repo.join(".piton/config.toml"),
            r#"
[workers]
skip = ["apps/.build"]
[sandbox]
deny = ["/r"]
presets = ["swift"]
[permissions]
allow = ["read"]
default_mode = "ask"
[branches]
format = "username/identifier-title"
[tracker]
provider = "linear"
mcp_server = "linear"
"#,
        );
        write(
            &repo.join(".piton/config.local.toml"),
            "[sandbox]\nallow = [\"/local\"]\n[web_search]\napi_key = \"k\"\n",
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
        assert_eq!(s.expose, ["~/.ssh/config"]);
        assert_eq!(s.allow, ["~/.cargo/registry", "~/Projects/work/.pi", "/local"]);
        assert_eq!(s.deny, ["/r"]);
        assert_eq!(r.workers.as_ref().unwrap().skip, ["apps/.build"]);

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
        assert_eq!(r.web_search.as_ref().unwrap().api_key.as_deref(), Some("k"));

        // [tracker] is replaced whole by the nearer level; [forge] comes from the parent.
        let tracker = r.tracker.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(tracker.path, repo.join(".piton/config.toml"));
        assert_eq!(
            tracker.value,
            serde_json::json!({"provider": "linear", "mcp_server": "linear"})
        );
        assert_eq!(
            r.forge.as_ref().unwrap().as_ref().unwrap().path,
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
        assert!(r.watch.contains(&repo.join(".piton/config.local.toml")));
        assert!(r.files.iter().any(|f| f.path == repo.join(".piton/config.local.toml")));

        let host = run(&t, &source(repo), Runtime::Host).unwrap();
        let permissions = &host.permissions.as_ref().unwrap().merged;
        assert_eq!(permissions.ask, ["write"]);
        assert_eq!(permissions.default_mode, Some(PermissionMode::Ask));
    }

    #[test]
    fn scope_rules_are_enforced() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".piton/config.toml"), "[sandbox]\nexpose = [\"~/.ssh\"]\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.sandbox, Err(ConfigError::NotInProjectConfig { .. })));
        assert!(r.permissions.is_ok());
        // Integrations are project only: a global [tracker] fails its part.
        write(&t.global.join("config.toml"), "[tracker]\nprovider = \"github\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.tracker, Err(ConfigError::ProjectOnly { .. })));
        std::fs::remove_file(t.global.join("config.toml")).unwrap();
        // A secret in a shared file.
        write(&repo.join(".piton/config.toml"), "[web_search]\napi_key = \"k\"\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.web_search, Err(ConfigError::LocalOnly { .. })));
        // A username in a shared file.
        write(&repo.join(".piton/config.toml"), "[branches]\nusername = \"me\"\n");
        let r = run(&t, &source(repo), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(ConfigError::NotInProjectConfig { .. })));
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
            "[branches]\nunknown = 1\n[permissions]\nallow = [\"read\"]\n",
        );
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(ConfigError::Invalid { .. })));
        assert_eq!(r.permissions.as_ref().unwrap().merged.allow, ["read"]);
        assert!(r.tracker.is_ok() && r.mcp.is_ok() && r.policies.is_ok() && r.instructions.is_ok());

        // A typo in the global [branches] leaves [sandbox] readable, and launches with it.
        write(
            &t.global.join("config.toml"),
            "[branches]\nformt = 1\n[sandbox]\nallow = [\"/g\"]\n",
        );
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.branches, Err(ConfigError::Invalid { .. })));
        assert_eq!(r.sandbox.as_ref().unwrap().allow, ["/g"]);
        assert_eq!(
            resolve_launch(&source(repo.clone()), &t.paths).unwrap().sandbox.allow,
            ["/g"]
        );
        std::fs::remove_file(t.global.join("config.toml")).unwrap();

        write(&repo.join(".piton/config.toml"), "[permissions\n");
        let r = run(&t, &source(repo.clone()), Runtime::Host).unwrap();
        assert!(matches!(r.permissions, Err(ConfigError::Invalid { .. })));
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
        assert!(resolve_launch(&ws, &t.paths).is_ok());
        let r = run(&t, &ws, Runtime::Sandbox).unwrap();
        assert!(r.errors().is_empty(), "{:?}", r.errors());
        let permissions = &r.permissions.as_ref().unwrap().merged;
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
