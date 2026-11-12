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

/// Everything resolved for one workspace.
#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub workspace: PathBuf,
    /// The checkout whose `.piton/` was read (a worker's source).
    pub source: PathBuf,
    /// The workspace's own checkout.
    pub checkout: PathBuf,
    pub global: PathBuf,
    pub runtime: Runtime,
    pub daemon: DaemonFile,
    pub settings: SettingsFile,
    pub permissions: PermissionsFile,
    pub mcp: ResolvedMcp,
    pub web_search: WebSearchFile,
    pub workflows: WorkflowsFile,
    /// The nearest `integrations.json`, unchanged (the pi package validates it).
    pub integrations: Option<FileValue<serde_json::Value>>,
    /// Every `policies.md`, farthest first.
    pub policies: Vec<FileValue<String>>,
    /// Rule folders, farthest first: `<level>/rules`.
    pub rules: Vec<PathBuf>,
    /// Skill folders piton adds to pi's own discovery, farthest first.
    pub skills: Vec<PathBuf>,
    /// Workflow artifacts: the source's `.piton/.local`.
    pub artifacts: PathBuf,
    /// Every file read, with its level.
    pub files: Vec<FileOrigin>,
    /// Every path whose appearance, change, or removal changes the result: each level folder and each candidate
    /// file, existing or not. Clients cache the result until one of these changes.
    pub watch: Vec<PathBuf>,
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
        let ext = match format {
            Format::Toml => "toml",
            Format::Json => "json",
        };
        let mut loaded = Vec::new();
        for level in levels {
            for name in Self::names(stem, ext, runtime) {
                let path = level.dir.join(name);
                self.watch.push(path.clone());
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
        self.watch.push(path.clone());
        let text = read_text(&path)?;
        if text.is_some() {
            self.files.push(FileOrigin { path, level });
        }
        Ok(text)
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

/// Resolves a workspace: refuses the old layout, then reads both chains.
pub fn resolve(workspace: &Workspace, paths: &Paths, runtime: Runtime) -> Result<Resolved, SettingsError> {
    let chain = Chain::for_workspace(workspace, &paths.home, &paths.global);
    let old = old_layout(&chain, paths);
    if !old.is_empty() {
        return Err(SettingsError::OldLayout(old));
    }
    read_chain(&chain, &workspace.path, runtime)
}

/// Only the global tier: the daemon's own settings. Refuses only the daemon's own old file; the files piton used to
/// keep in pi's agent folder block the pi package (through [`resolve`]), not the daemon.
pub fn resolve_global(paths: &Paths) -> Result<Resolved, SettingsError> {
    let chain = Chain::global_only(&paths.global);
    let old = old_daemon_config(paths);
    if !old.is_empty() {
        return Err(SettingsError::OldLayout(old));
    }
    read_chain(&chain, &paths.global, Runtime::Host)
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
        .filter(|old| is_daemon_owned(&old.path))
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

/// Old files the daemon read: `config.toml`, `.piton.toml`, the nono overlays.
fn is_daemon_owned(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("config.toml" | ".piton.toml" | ".nono.json" | ".nono.local.json" | "nono.json" | "nono.local.json")
    )
}

fn read_chain(chain: &Chain, workspace: &Path, runtime: Runtime) -> Result<Resolved, SettingsError> {
    let mut reader = Reader::new();
    let mut piton = Vec::new();
    for level in &chain.piton {
        reader.watch.push(level.dir.clone());
        if check_level(level)? {
            piton.push(level.clone());
        }
    }
    let mut agents = Vec::new();
    for level in &chain.agents {
        reader.watch.push(level.dir.clone());
        if check_level(level)? {
            agents.push(level.clone());
        }
    }

    let mut daemon = DaemonFile::default();
    for file in reader.read::<DaemonFile>(&piton, "daemon", Format::Toml, None)? {
        validate_daemon(&file.value, &file.origin.path)?;
        merge::daemon(&mut daemon, file.value);
    }

    let global_levels: Vec<Level> = piton
        .iter()
        .filter(|level| level.kind == LevelKind::Global)
        .cloned()
        .collect();
    let mut settings = SettingsFile::default();
    for file in reader.read::<SettingsFile>(&global_levels, "settings", Format::Toml, None)? {
        merge::settings(&mut settings, file.value);
    }
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

    let mut permissions = PermissionsFile::default();
    for file in reader.read::<PermissionsFile>(&piton, "permissions", Format::Json, Some(runtime))? {
        merge::permissions(&mut permissions, file.value);
    }

    let mut mcp = ResolvedMcp::default();
    for file in reader.read::<McpFile>(&piton, "mcp", Format::Json, None)? {
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

    let mut web_search = WebSearchFile::default();
    for file in reader.read::<WebSearchFile>(&piton, "web-search", Format::Json, None)? {
        merge::web_search(&mut web_search, file.value);
    }

    let mut workflows = WorkflowsFile::default();
    for file in reader.read::<WorkflowsFile>(&piton, "workflows", Format::Json, None)? {
        merge::workflows(&mut workflows, file.value);
    }

    let mut integrations = None;
    let mut policies = Vec::new();
    for level in &piton {
        let path = level.dir.join("integrations.json");
        if let Some(text) = reader.text(path.clone(), level.kind)? {
            let value = serde_json::from_str(&text).map_err(|e| SettingsError::Invalid {
                path: path.clone(),
                reason: e.to_string(),
            })?;
            integrations = Some(FileValue { path, value });
        }
        let path = level.dir.join("policies.md");
        if let Some(text) = reader.text(path.clone(), level.kind)? {
            policies.push(FileValue { path, value: text });
        }
    }

    // Every candidate folder, existing or not: clients watch them, and a missing one simply has no files.
    let rules = chain.agents.iter().map(|level| level.dir.join("rules")).collect();
    let skills = chain
        .agents
        .iter()
        .filter(|level| level.kind != LevelKind::Repository)
        .map(|level| level.dir.join("skills"))
        .collect();

    Ok(Resolved {
        workspace: workspace.to_path_buf(),
        source: chain.source.clone(),
        checkout: chain.checkout.clone(),
        global: chain.global.clone(),
        runtime,
        daemon,
        settings,
        permissions,
        mcp,
        web_search,
        workflows,
        integrations,
        policies,
        rules,
        skills,
        artifacts: chain.source.join(LOCAL_DIR),
        files: reader.files,
        watch: reader.watch,
    })
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
    };
    match mcp.servers.iter_mut().find(|existing| existing.name == name) {
        Some(existing) => *existing = server,
        None => mcp.servers.push(server),
    }
    Ok(())
}

/// Files of the layout before `.piton/` in the chain's folders.
fn old_layout(chain: &Chain, paths: &Paths) -> Vec<OldPath> {
    let mut found = old_global(paths);
    let mut dirs: Vec<&Path> = vec![&chain.source];
    if chain.checkout != chain.source {
        dirs.push(&chain.checkout);
    }
    for level in &chain.piton {
        if level.kind == LevelKind::Parent
            && let Some(parent) = level.dir.parent()
        {
            dirs.push(parent);
        }
    }
    for dir in dirs {
        for (name, now) in [
            (".piton.toml", ".piton/daemon.toml [workers]"),
            (".nono.json", ".piton/daemon.toml [sandbox]"),
            (".nono.local.json", ".piton/daemon.local.toml [sandbox]"),
            (
                ".project",
                ".piton/ (workflows.json, integrations.json, policies.md, .local/)",
            ),
        ] {
            let path = dir.join(name);
            if path.symlink_metadata().is_ok() {
                found.push(OldPath { path, now });
            }
        }
        for name in [
            "permissions.json",
            "permissions.local.json",
            "permissions.host.json",
            "permissions.sandbox.json",
            "permissions.host.local.json",
            "permissions.sandbox.local.json",
            "mcp.json",
            "mcp.local.json",
            "web-search.json",
            "web-search.local.json",
        ] {
            let path = dir.join(AGENTS_DIR).join(name);
            if path.exists() {
                found.push(OldPath {
                    path,
                    now: "the same name in .piton/",
                });
            }
        }
        for name in ["nono.json", "nono.local.json"] {
            let path = dir.join(PITON_DIR).join(name);
            if path.exists() {
                found.push(OldPath {
                    path,
                    now: ".piton/daemon[.local].toml [sandbox]",
                });
            }
        }
    }
    found
}

/// The global files before the global tier: `config.toml` and the files piton kept in pi's agent folder.
fn old_daemon_config(paths: &Paths) -> Vec<OldPath> {
    let config = paths.global.join("config.toml");
    if config.exists() {
        vec![OldPath {
            path: config,
            now: "daemon.toml (how the daemon runs) and settings.toml [host] (roots, logging)",
        }]
    } else {
        Vec::new()
    }
}

fn old_global(paths: &Paths) -> Vec<OldPath> {
    let mut found = old_daemon_config(paths);
    let pi_dir = &paths.pi_agent_dir;
    for name in [
        "permissions.json",
        "permissions.host.json",
        "permissions.sandbox.json",
        "mcp.json",
        "web-search.json",
        "workflows.json",
    ] {
        let path = pi_dir.join(name);
        if path.exists() {
            found.push(OldPath {
                path,
                now: "the same name in the global tier",
            });
        }
    }
    let rules = pi_dir.join("rules");
    if rules.is_dir() {
        found.push(OldPath {
            path: rules,
            now: "agents/rules/ in the global tier",
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
        let s = &r.daemon.sandbox;
        assert_eq!(s.base.as_deref(), Some("mise-dev"));
        assert_eq!(s.groups, ["git_config"]);
        assert_eq!(s.allow, ["~/.cargo/registry", "~/Projects/work/.pi", "/local"]);
        assert_eq!(s.read, ["/r"]);
        assert_eq!(s.network.open_port, [0]);
        assert_eq!(r.daemon.workers.skip, ["apps/.build"]);

        // Sandbox runtime: the host file isn't read; the repository's defaultMode is nearest.
        assert_eq!(r.permissions.deny, ["bash(rm -rf *)"]);
        assert_eq!(r.permissions.ask, ["write", "bash(git push *)"]);
        assert_eq!(r.permissions.allow, ["read"]);
        assert_eq!(
            r.permissions.default_mode,
            Some(crate::settings::files::PermissionMode::Ask)
        );

        // MCP: the nearer entry replaces the whole entry, order kept.
        let names: Vec<_> = r.mcp.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["github", "docs"]);
        assert_eq!(r.mcp.servers[0].config["url"], "https://b");

        assert_eq!(r.workflows.session_analysis, Some(true));
        assert_eq!(
            r.workflows.branches.format,
            Some(crate::settings::files::BranchFormat::IdentifierTitle)
        );
        assert_eq!(
            r.settings.host.roots.as_deref(),
            Some(&["~/Projects/personal".to_string()][..])
        );
        let integrations = r.integrations.unwrap();
        assert_eq!(integrations.path, repo.join(".piton/integrations.json"));
        assert_eq!(integrations.value["tracker"]["provider"], "linear");
        let policies: Vec<_> = r.policies.iter().map(|p| p.value.as_str()).collect();
        assert_eq!(policies, ["global policy\n", "repo policy\n"]);
        assert_eq!(r.rules.first().unwrap(), &t.global.join("agents/rules"));
        assert_eq!(r.rules.last().unwrap(), &repo.join(".agents/rules"));
        assert!(r.skills.contains(&t.global.join("agents/skills")));
        assert!(
            !r.skills.contains(&repo.join(".agents/skills")),
            "pi discovers the repository's own"
        );
        assert_eq!(r.artifacts, repo.join(".piton/.local"));
        assert!(r.watch.contains(&repo.join(".piton/permissions.sandbox.local.json")));
        assert!(r.files.iter().any(|f| f.path == repo.join(".piton/daemon.local.toml")));

        let host = run(&t, &source(repo), Runtime::Host).unwrap();
        assert_eq!(host.permissions.ask, ["write"]);
        assert_eq!(
            host.permissions.default_mode,
            Some(crate::settings::files::PermissionMode::Ask)
        );
    }

    #[test]
    fn global_only_keys_in_a_repository_are_refused() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".piton/daemon.toml"), "[sandbox]\nbase = \"default\"\n");
        let err = run(&t, &source(repo.clone()), Runtime::Host).unwrap_err();
        assert!(matches!(err, SettingsError::GlobalOnly { .. }), "{err}");
        std::fs::remove_file(repo.join(".piton/daemon.toml")).unwrap();
        write(&repo.join(".piton/settings.toml"), "[host]\n");
        assert!(matches!(
            run(&t, &source(repo), Runtime::Host),
            Err(SettingsError::GlobalOnly { .. })
        ));
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
        assert!(matches!(
            run(&t, &source(repo), Runtime::Host),
            Err(SettingsError::Conflict { .. })
        ));
    }

    #[test]
    fn the_old_layout_is_refused() {
        let t = tree();
        let repo = t.home.join("repo");
        write(&repo.join(".nono.json"), "{}");
        write(&repo.join(".project/config.json"), "{}");
        write(&repo.join(".agents/permissions.json"), "{}");
        let err = run(&t, &source(repo.clone()), Runtime::Host).unwrap_err();
        let SettingsError::OldLayout(paths) = &err else {
            panic!("{err}")
        };
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.path.strip_prefix(&repo).unwrap().to_owned())
            .collect();
        assert!(names.contains(&PathBuf::from(".nono.json")));
        assert!(names.contains(&PathBuf::from(".project")));
        assert!(names.contains(&PathBuf::from(".agents/permissions.json")));
        assert!(err.to_string().contains("piton migrate"));
        // Rules and skills in .agents/ are the new layout, not old.
        for old in [".nono.json", ".agents/permissions.json"] {
            std::fs::remove_file(repo.join(old)).unwrap();
        }
        std::fs::remove_dir_all(repo.join(".project")).unwrap();
        write(&repo.join(".agents/rules/r.md"), "x");
        assert!(run(&t, &source(repo), Runtime::Host).is_ok());

        write(&t.global.join("config.toml"), "roots = []\n");
        assert!(matches!(resolve_global(&t.paths), Err(SettingsError::OldLayout(_))));
        std::fs::remove_file(t.global.join("config.toml")).unwrap();
        // Files piton kept in pi's agent folder block the pi package's resolution, not the daemon's start.
        write(&t.paths.pi_agent_dir.join("permissions.json"), "{}");
        assert!(resolve_global(&t.paths).is_ok());
        let repo = t.home.join("clean");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(matches!(
            run(&t, &source(repo), Runtime::Host),
            Err(SettingsError::OldLayout(_))
        ));
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
        let r = run(&t, &ws, Runtime::Sandbox).unwrap();
        assert_eq!(r.permissions.deny, ["bash(*)"]);
        assert!(r.permissions.allow.is_empty());
        assert_eq!(r.artifacts, src.join(".piton/.local"));
        assert_eq!(r.rules.last().unwrap(), &worker.join(".agents/rules"));
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
