//! Building pi's environment and command line for a launch.

pub mod claude_sockets;
pub mod env;
pub mod fnox;
pub mod git;
pub mod handoff;
mod mode;
pub mod sandbox;

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

use tokio::process::Command;

pub use self::mode::LaunchMode;
use crate::{
    config::{ConfigError, DaemonConfig, LaunchConfig},
    project::{self, ProjectError, Workspace},
};

pub type Env = BTreeMap<String, String>;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("workspace {path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("{tool} env failed in {workspace}: {stderr}")]
    EnvTool {
        tool: env::EnvTool,
        workspace: PathBuf,
        stderr: String,
    },
    #[error("can't run {program}: {source}")]
    Spawn { program: String, source: std::io::Error },
    #[error("can't find this piton binary to start the sandbox: {0}")]
    Helper(std::io::Error),
    #[error(
        "workspace {workspace}: {link} is a symlink inside the checkout {checkout} that leads out of it, which a sandboxed \
         session could have made; open the real folder instead"
    )]
    WorkspaceLink {
        workspace: PathBuf,
        link: PathBuf,
        checkout: PathBuf,
    },
    /// A tool the launch needs is enabled but not installed.
    #[error(transparent)]
    Tool(#[from] crate::tools::Unavailable),
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Sandbox(#[from] crate::launch::sandbox::SandboxError),
    #[error(transparent)]
    Fnox(#[from] fnox::FnoxError),
}

/// Which session pi starts with.
#[derive(Debug, Clone)]
pub enum SessionArg {
    New,
    Resume(PathBuf),
}

/// Captures the user's shell environment once (launchd's is minimal): a non-interactive login shell, which runs
/// `.zshenv` and `.zprofile` (where the user's setup puts PATH, `PI_CODING_AGENT_DIR`, and `mise env`) without
/// interactive-only setup (prompt, direnv hooks). Anything printed before the marker is ignored.
pub async fn login_shell_env() -> Option<Env> {
    const MARKER: &str = "__PITON_ENV__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '{MARKER}'; env -0");
    let child = Command::new(&shell)
        .args(["-l", "-c", &script])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), child)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let start = out.stdout.windows(MARKER.len()).position(|w| w == MARKER.as_bytes())? + MARKER.len();
    Some(parse_env0(&out.stdout[start..]))
}

/// Resolves a command name against `PATH` in `env` (pi's environment), not the daemon's own. Names with a `/` and
/// names not found are returned as given.
pub fn resolve_program(program: &str, env: &Env) -> String {
    if program.contains('/') {
        return program.to_string();
    }
    let path = env.get("PATH").map(String::as_str).unwrap_or_default();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(program);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    program.to_string()
}

fn parse_env0(bytes: &[u8]) -> Env {
    bytes
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// A launch whose environment, credential proxy, and, for a sandboxed one, policy are resolved.
pub enum Prepared {
    Host { env: Env, proxy: Option<fnox::Proxy> },
    Sandbox(Box<Sandboxed>),
}

/// A sandboxed launch's environment and policy, and the credential proxy it starts behind, checked against the policy.
pub struct Sandboxed {
    pub env: Env,
    pub resolution: sandbox::Resolution,
    pub proxy: Option<fnox::Proxy>,
    /// What commands the user approves run with: `all` and each `[[sandbox.commands]]` entry's policy.
    pub commands: sandbox::grant::CommandPolicies,
    /// What the session starts with a warning about.
    pub warnings: Vec<LaunchWarning>,
    /// The `pi-claude-code` preset's socket folder of the session's own, set as `CLAUDE_CODE_TMPDIR`: created when the
    /// session launches, removed when it ends.
    pub sockets: Option<PathBuf>,
}

/// Something a sandboxed session starts despite, and tells the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchWarning {
    /// The `pi-claude-code` preset couldn't open the config folder `PI_CLAUDE_CODE_CONFIG_DIR` names.
    ClaudeCodeFolder(sandbox::ClaudeCodeFolder),
    /// The `pi-claude-code` preset signs in with a token, and pi's environment has none.
    NoClaudeToken,
    /// The `pi-claude-code` preset doesn't open the socket folder the user's `XDG_RUNTIME_DIR` or `CLAUDE_CODE_TMPDIR`
    /// names.
    ClaudeCodeSockets {
        var: &'static str,
        path: PathBuf,
        reason: String,
    },
}

/// The variable `claude setup-token`'s token goes in.
pub const CLAUDE_CODE_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

impl std::fmt::Display for LaunchWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use sandbox::ClaudeCodeFolder;
        let blocked = "so Claude Code's own files stay closed: pi's claude-code models may fail, and what `claude` \
                       writes there is asked about like any blocked process";
        let fix = "set PI_CLAUDE_CODE_CONFIG_DIR to a folder of its own (e.g. ~/.config/pi/claude) in your shell or \
                   global mise configuration";
        match self {
            LaunchWarning::ClaudeCodeFolder(folder) => match folder {
                ClaudeCodeFolder::Unset => write!(
                    f,
                    "The pi-claude-code preset is on, but PI_CLAUDE_CODE_CONFIG_DIR isn't set, {blocked}; {fix}."
                ),
                ClaudeCodeFolder::Interactive { path } => write!(
                    f,
                    "The pi-claude-code preset doesn't open {}: it's the interactive Claude Code's folder, whose hooks, \
                     plugins, and MCP servers run on the host, {blocked}; {fix}.",
                    path.display()
                ),
                ClaudeCodeFolder::Closed { path, reason } => write!(
                    f,
                    "The pi-claude-code preset doesn't open {}: {reason}, {blocked}; {fix}.",
                    path.display()
                ),
                ClaudeCodeFolder::AgentWritable { path, set_by, checkout } => write!(
                    f,
                    "The pi-claude-code preset doesn't open {}: it was set by {set_by}, which a sandboxed session can \
                     write, and it leads out of the checkout {}, {blocked}; {fix}.",
                    path.display(),
                    checkout.display()
                ),
                ClaudeCodeFolder::Granted(path) => write!(f, "The pi-claude-code preset opens {}.", path.display()),
            },
            LaunchWarning::NoClaudeToken => write!(
                f,
                "The pi-claude-code preset signs claude in with a token, but {CLAUDE_CODE_TOKEN} isn't in pi's \
                 environment or among fnox's proxy rules: pi's claude-code models won't sign in. Put the token from \
                 `claude setup-token` behind fnox (or in your environment), or set `login = \"keychain\"`."
            ),
            LaunchWarning::ClaudeCodeSockets { var, path, reason } => write!(
                f,
                "The pi-claude-code preset doesn't open {} ({var}) for claude's sockets: {reason}. claude's startup \
                 probe there meets the sandbox; unset {var} for pi (Piton then gives claude a folder of its own), or \
                 point it at a private folder of its own.",
                path.display()
            ),
        }
    }
}

/// The warnings for a sandboxed launch: the `pi-claude-code` preset's folder when it isn't opened, and its token
/// login with no token in pi's `env` or among the `proxy`'s variables.
fn launch_warnings(resolution: &sandbox::Resolution, env: &Env, proxy: Option<&fnox::Proxy>) -> Vec<LaunchWarning> {
    use sandbox::ClaudeCodeFolder;

    use crate::config::ClaudeLogin;
    let Some(claude_code) = &resolution.claude_code else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    match &claude_code.folder {
        ClaudeCodeFolder::Granted(_) => {}
        folder @ (ClaudeCodeFolder::Unset
        | ClaudeCodeFolder::Interactive { .. }
        | ClaudeCodeFolder::Closed { .. }
        | ClaudeCodeFolder::AgentWritable { .. }) => warnings.push(LaunchWarning::ClaudeCodeFolder(folder.clone())),
    }
    match claude_code.login {
        ClaudeLogin::Token => {
            let in_env = env.get(CLAUDE_CODE_TOKEN).is_some_and(|v| !v.trim().is_empty());
            let from_proxy = proxy.is_some_and(|p| p.variables.iter().any(|v| v == CLAUDE_CODE_TOKEN));
            if !in_env && !from_proxy {
                warnings.push(LaunchWarning::NoClaudeToken);
            }
        }
        ClaudeLogin::Keychain => {}
    }
    match &claude_code.sockets {
        Some(sandbox::ClaudeCodeSockets::Refused { var, path, reason }) => {
            warnings.push(LaunchWarning::ClaudeCodeSockets {
                var,
                path: path.clone(),
                reason: reason.clone(),
            })
        }
        Some(sandbox::ClaudeCodeSockets::Own(_) | sandbox::ClaudeCodeSockets::Users { .. }) | None => {}
    }
    warnings
}

/// Builds a launch in `workspace` in `mode`.
pub async fn prepare(
    config: &DaemonConfig,
    tools: &crate::tools::Tools,
    resolved: &LaunchConfig,
    base: &Env,
    workspace: &Path,
    mode: LaunchMode,
) -> Result<Prepared, LaunchError> {
    Ok(match mode {
        LaunchMode::Host => {
            let mut env = host_env(config, tools, base, workspace).await?;
            let proxy = proxy_for(tools, base, workspace, &mut env).await?;
            Prepared::Host { env, proxy }
        }
        LaunchMode::Sandbox => Prepared::Sandbox(Box::new(
            prepare_sandbox(config, tools, resolved, base, workspace, ClaudeSockets::Session).await?,
        )),
    })
}

/// The credential proxy for a launch in `workspace`, when fnox is used and a `[proxy]` applies there; pi's `FNOX_*`
/// variables then come from the daemon's base environment only.
async fn proxy_for(
    tools: &crate::tools::Tools,
    base: &Env,
    workspace: &Path,
    env: &mut Env,
) -> Result<Option<fnox::Proxy>, LaunchError> {
    let Some(program) = tools.fnox.optional()? else {
        return Ok(None);
    };
    fnox::strip_workspace_variables(env, base);
    Ok(fnox::prepare(program, base, workspace).await?)
}

/// pi's environment for a host launch in `workspace`: the base environment and the environment tools' for that folder,
/// run on the host (a host session trusts the project by choice), then Piton's own variables.
async fn host_env(
    config: &DaemonConfig,
    tools: &crate::tools::Tools,
    base: &Env,
    workspace: &Path,
) -> Result<Env, LaunchError> {
    let env = env::host(&env::enabled(tools)?, base, workspace).await?;
    Ok(with_piton_vars(config, env, LaunchMode::Host))
}

/// A sandboxed launch's environment and policy. The environment tools run inside the sandbox, under the launch's
/// policy with the host's agent directory (none of their values yet); the launch's policy is then resolved from their
/// environment, which accepts a widening value only from a file the agent can't write. When a credential proxy applies,
/// every file fnox loads is closed in the policy (and in the environment tools' sandbox), and the launch is refused when
/// the policy can read the keychain.
pub async fn prepare_sandbox(
    config: &DaemonConfig,
    all_tools: &crate::tools::Tools,
    resolved: &LaunchConfig,
    base: &Env,
    workspace: &Path,
    sockets: ClaudeSockets,
) -> Result<Sandboxed, LaunchError> {
    let tools = env::enabled(all_tools)?;
    // Picked once, so every resolution of this launch (the command grants' too) names the same folder.
    let own_sockets = match sockets {
        ClaudeSockets::Session => claude_sockets::pick(base.get("TMPDIR").map(Path::new), &config.state_dir()),
        ClaudeSockets::None => None,
    };
    // fnox's checks run with the base environment only, so the proxy is known before any policy.
    let proxy = match all_tools.fnox.optional()? {
        Some(program) => fnox::prepare(program, base, workspace).await?,
        None => None,
    };
    let fnox_files: &[PathBuf] = proxy.as_ref().map_or(&[], |p| p.files.as_slice());
    let own = own_sockets.as_deref();
    let sandbox_policy = |resolved: &LaunchConfig, env: &env::SandboxEnv| {
        sandbox_policy(config, resolved, base, env, workspace, fnox_files, own)
    };
    let bootstrap = sandbox_policy(resolved, &env::SandboxEnv::daemon(base))?;
    let launch_env = match tools.is_empty() {
        true => env::SandboxEnv::daemon(base),
        false => {
            env::sandboxed(
                &tools,
                base,
                workspace,
                &bootstrap.policy,
                sandbox::containment(),
                &helper()?,
            )
            .await?
        }
    };
    let resolution = sandbox_policy(resolved, &launch_env)?;
    let resolver = {
        let (config, base, launch_env) = (config.clone(), base.clone(), launch_env.clone());
        let (workspace, fnox_files, own) = (workspace.to_path_buf(), fnox_files.to_vec(), own_sockets.clone());
        sandbox::grant::Resolver::new(resolved.clone(), move |c: &LaunchConfig| {
            self::sandbox_policy(&config, c, &base, &launch_env, &workspace, &fnox_files, own.as_deref())
                .map(|r| r.policy)
                .map_err(|e| e.to_string())
        })
    };
    let mut env = with_piton_vars(config, launch_env.clone().into_env(), LaunchMode::Sandbox);
    if let Some(proxy) = &proxy {
        fnox::strip_workspace_variables(&mut env, base);
        proxy.check_sandbox(&resolution.policy, &config.paths.home)?;
    }
    let credentials = match &proxy {
        Some(_) => sandbox::grant::Credentials::Floor,
        None => sandbox::grant::Credentials::Liftable,
    };
    let claude_login = resolution.claude_code.as_ref().map(|c| c.login);
    // What a host run gets that the agent may have chosen: values set by a file the launch's sandbox can write.
    let writable_vars = launch_env.agent_writable(|file| {
        resolution
            .policy
            .decide(&crate::agent::canonical_target(file), sandbox::Access::Write)
            .allowed
    });
    let commands = sandbox::grant::command_policies(
        &resolution.policy,
        credentials,
        resolver,
        resolution.silence.clone(),
        claude_login,
        writable_vars,
    );
    let warnings = launch_warnings(&resolution, &env, proxy.as_ref());
    let sockets = with_claude_sockets(&mut env, &resolution);
    Ok(Sandboxed {
        env,
        resolution,
        proxy,
        commands,
        warnings,
        sockets,
    })
}

/// Sets the session's own socket folder as `CLAUDE_CODE_TMPDIR` in pi's `env` when the policy opened one, and returns
/// it. Set after the workspace's environment, like Piton's own variables: the policy opened this folder, so pi's
/// `claude` must use it, whatever the workspace set. A folder the user named is left as they set it.
fn with_claude_sockets(env: &mut Env, resolution: &sandbox::Resolution) -> Option<PathBuf> {
    match resolution.claude_code.as_ref().and_then(|c| c.sockets.as_ref()) {
        Some(sandbox::ClaudeCodeSockets::Own(folder)) => {
            env.insert(claude_sockets::VAR.into(), folder.to_string_lossy().into_owned());
            Some(folder.clone())
        }
        Some(sandbox::ClaudeCodeSockets::Users { .. } | sandbox::ClaudeCodeSockets::Refused { .. }) | None => None,
    }
}

/// Whether a sandboxed launch gets a socket folder of its own for the `pi-claude-code` preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeSockets {
    /// A session: the daemon creates the folder at launch and removes it when the session ends.
    Session,
    /// Not a session (`piton sandbox show`, the git sandbox): nothing to create or remove.
    None,
}

/// What Piton decides, set last so a repository's environment can't override it: the runtime (which permissions
/// apply), the global tier, and this binary (the pi package resolves configuration through it).
fn with_piton_vars(config: &DaemonConfig, mut env: Env, mode: LaunchMode) -> Env {
    env.remove("PITON_RUNTIME");
    match mode {
        LaunchMode::Sandbox => {
            env.insert("PITON_RUNTIME".into(), "sandbox".into());
        }
        LaunchMode::Host => {}
    }
    env.insert(
        "PITON_CONFIG_HOME".into(),
        config.paths.global.to_string_lossy().into_owned(),
    );
    // Never a value from the workspace: unset when this binary can't be found.
    env.remove("PITON_BIN");
    if let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) {
        env.insert("PITON_BIN".into(), exe.to_string_lossy().into_owned());
    }
    env
}

/// The launch's configuration: `[sandbox]` and `[workers]` of the workspace's chain, refusing them when they're invalid.
/// The pi package checks its own parts when pi starts.
pub fn resolve_config(config: &DaemonConfig, workspace: &Workspace) -> Result<LaunchConfig, LaunchError> {
    Ok(crate::config::resolve_launch(workspace, &config.paths)?)
}

/// The full argument vector for pi in `mode`. A sandboxed pi starts through `helper sandbox-exec` (this binary), which
/// applies the policy in [`sandbox::POLICY_ENV`] and then runs pi.
pub fn command_line(config: &DaemonConfig, mode: LaunchMode, session: &SessionArg, helper: &Path) -> Vec<String> {
    let mut pi: Vec<String> = config.pi.command.clone();
    pi.extend(["--mode".into(), "rpc".into()]);
    pi.extend(config.pi.args.iter().cloned());
    match session {
        SessionArg::New => {}
        SessionArg::Resume(file) => {
            pi.push("--session".into());
            pi.push(file.to_string_lossy().into_owned());
        }
    }
    match mode {
        LaunchMode::Host => pi,
        LaunchMode::Sandbox => {
            let mut argv = vec![
                helper.to_string_lossy().into_owned(),
                "sandbox-exec".into(),
                "--".into(),
            ];
            // The helper starts pi with pi's environment from the handoff, finding it through pi's PATH.
            argv.extend(pi);
            argv
        }
    }
}

/// This binary: the sandbox helper. In Piton.app it's the bundled `piton`.
pub fn helper() -> Result<PathBuf, LaunchError> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(LaunchError::Helper)
}

/// The sandbox policy of a launch in `workspace`, with pi's environment `env`, and the grants it doesn't follow
/// through a symlink.
fn sandbox_policy(
    config: &DaemonConfig,
    resolved: &LaunchConfig,
    base: &Env,
    env: &env::SandboxEnv,
    workspace: &Path,
    fnox_files: &[PathBuf],
    claude_sockets: Option<&Path>,
) -> Result<sandbox::Resolution, LaunchError> {
    let state = config.state_dir();
    let agent_socket = config.agent_socket();
    let logs = crate::logging::root();
    Ok(sandbox::resolve(
        resolved,
        &sandbox::Facts {
            home: &config.paths.home,
            base_env: base,
            launch_env: env,
            workspace,
            state_dir: &state,
            control_socket: &config.socket,
            agent_socket: &agent_socket,
            logs: &logs,
            write_roots: sandbox::WRITE_ROOTS,
            fnox_files,
            claude_sockets,
        },
    )?)
}

/// Resolves a workspace path the way pi sees its cwd (symlinks resolved), and its relation to its project.
pub fn resolve_workspace(path: &Path) -> Result<Workspace, LaunchError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| LaunchError::NotADirectory {
        path: path.to_path_buf(),
    })?;
    if !canonical.is_dir() {
        return Err(LaunchError::NotADirectory { path: canonical });
    }
    Ok(project::resolve(&canonical)?)
}

/// Refuses a sandboxed launch whose workspace path, as given, goes through a symlink inside a git checkout that leads out
/// of it: a sandboxed session in that checkout could have made it, to send the next session's workspace grant
/// anywhere. A link that stays in its checkout, and one outside any checkout (the user's own), are fine.
pub fn check_workspace_links(path: &Path) -> Result<(), LaunchError> {
    let not_a_directory = || LaunchError::NotADirectory {
        path: path.to_path_buf(),
    };
    let absolute = std::path::absolute(path).map_err(|_| not_a_directory())?;
    // Kept real: every link met is replaced by its target, so `..` steps up from where the OS would be.
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                current.pop();
                continue;
            }
            Component::CurDir => continue,
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => current.push(component),
        }
        if !current.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let real = std::fs::canonicalize(&current).map_err(|_| not_a_directory())?;
        let checkout = current
            .parent()
            .and_then(|parent| parent.ancestors().find(|dir| dir.join(".git").exists()))
            .map(Path::to_path_buf);
        if let Some(checkout) = checkout
            && !real.starts_with(&checkout)
        {
            return Err(LaunchError::WorkspaceLink {
                workspace: path.to_path_buf(),
                link: current,
                checkout,
            });
        }
        current = real;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_starts_pi_through_this_binary() {
        let config = DaemonConfig::for_tests();
        let argv = command_line(
            &config,
            LaunchMode::Sandbox,
            &SessionArg::Resume("/s.jsonl".into()),
            Path::new("/app/piton"),
        );
        assert_eq!(
            argv,
            [
                "/app/piton",
                "sandbox-exec",
                "--",
                "pi",
                "--mode",
                "rpc",
                "--session",
                "/s.jsonl"
            ]
        );
    }

    /// piton's own variables are set after mise's, so a repository's `mise.toml` can't change them.
    #[test]
    fn piton_variables_win_over_the_workspace_environment() {
        let mut config = DaemonConfig::for_tests();
        config.paths.global = PathBuf::from("/global/piton");
        let base: Env = [
            ("PITON_RUNTIME".to_string(), "sandbox".to_string()),
            ("PITON_CONFIG_HOME".to_string(), "/repo/fake".to_string()),
            ("PITON_BIN".to_string(), "/repo/fake-piton".to_string()),
        ]
        .into();
        let host = with_piton_vars(&config, base, LaunchMode::Host);
        assert_eq!(host.get("PITON_RUNTIME"), None, "host runtime is unset");
        assert_eq!(host["PITON_CONFIG_HOME"], "/global/piton");
        assert_ne!(host["PITON_BIN"], "/repo/fake-piton");
        let sandbox = with_piton_vars(&config, Env::new(), LaunchMode::Sandbox);
        assert_eq!(sandbox["PITON_RUNTIME"], "sandbox");
    }

    /// A sandboxed launch through a link inside a checkout that leads out of it is refused; the user's own links work.
    #[test]
    fn workspace_links_out_of_their_checkout_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let repo = root.join("repo");
        for d in ["repo/.git", "repo/src/app", "elsewhere/app", "Projects/real"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let link = |target: &str, at: &str| std::os::unix::fs::symlink(root.join(target), root.join(at)).unwrap();
        link("elsewhere", "repo/out");
        link("repo/src", "repo/in");
        link("Projects", "code");
        link("repo", "Projects/repo-link");

        assert!(matches!(
            check_workspace_links(&repo.join("out/app")),
            Err(LaunchError::WorkspaceLink { link, checkout, .. }) if link == repo.join("out") && checkout == repo
        ));
        assert!(check_workspace_links(&repo.join("in/app")).is_ok());
        assert!(check_workspace_links(&repo.join("src/app")).is_ok());
        // Links outside any checkout are the user's own.
        assert!(check_workspace_links(&root.join("code/real")).is_ok());
        assert!(check_workspace_links(&root.join("Projects/repo-link/src")).is_ok());
        // `..` steps up from where the link leads, as the OS does.
        assert!(matches!(
            check_workspace_links(&repo.join("in/../out")),
            Err(LaunchError::WorkspaceLink { .. })
        ));
    }

    /// A launch's resolution with only the `pi-claude-code` preset's outcome set.
    fn resolution(claude_code: Option<sandbox::ClaudeCode>) -> sandbox::Resolution {
        sandbox::Resolution {
            policy: sandbox::Policy {
                rules: Vec::new(),
                keychain: false,
                services: crate::launch::sandbox::Services::Every {
                    source: crate::launch::sandbox::Source::Workspace,
                },
                apps: crate::launch::sandbox::Apps::Closed,
                network: Vec::new(),
                tag: String::new(),
            },
            links: Vec::new(),
            agent_dirs: ["/h/.pi/agent".into(), "/h/.pi/agent".into()],
            profile: sandbox::ProfileChoice {
                name: crate::config::Profile::Standard,
                set_in: None,
            },
            claude_code,
            silence: Vec::new(),
        }
    }

    /// The session's own socket folder is what pi's `claude` uses: set as `CLAUDE_CODE_TMPDIR`, over the workspace's
    /// value. A folder the user named, or none, leaves pi's environment as it is.
    #[test]
    fn the_session_s_socket_folder_is_set_for_claude() {
        use sandbox::{ClaudeCode, ClaudeCodeSockets};
        let with = |sockets| {
            resolution(Some(ClaudeCode {
                folder: granted_folder(),
                login: crate::config::ClaudeLogin::Token,
                sockets,
            }))
        };
        let from_workspace: Env = [(claude_sockets::VAR.to_string(), "/w/tmp".to_string())].into();

        let mut env = from_workspace.clone();
        let own = PathBuf::from("/t/piton-cc-x");
        let set = with_claude_sockets(&mut env, &with(Some(ClaudeCodeSockets::Own(own.clone()))));
        assert_eq!(set, Some(own));
        assert_eq!(env[claude_sockets::VAR], "/t/piton-cc-x");

        let users = ClaudeCodeSockets::Users {
            var: "CLAUDE_CODE_TMPDIR",
            path: "/w/tmp".into(),
        };
        for r in [with(Some(users)), with(None), resolution(None)] {
            let mut env = from_workspace.clone();
            assert_eq!(with_claude_sockets(&mut env, &r), None);
            assert_eq!(env, from_workspace);
        }
    }

    /// The `pi-claude-code` preset's warnings: its folder when it isn't opened, and its token login with no token in pi's
    /// environment or among fnox's rules. Nothing without the preset.
    #[test]
    fn the_claude_code_preset_warns_instead_of_refusing() {
        use sandbox::{ClaudeCode, ClaudeCodeFolder};

        use crate::config::ClaudeLogin;
        let preset = |folder, login| {
            resolution(Some(ClaudeCode {
                folder,
                login,
                sockets: None,
            }))
        };
        let granted = ClaudeCodeFolder::Granted("/h/.config/pi/claude".into());
        let token: Env = [(CLAUDE_CODE_TOKEN.to_string(), "sk-ant-oat01-x".to_string())].into();
        let proxy = |variables: Vec<String>| fnox::Proxy {
            program: crate::tools::Program {
                program: "fnox".into(),
                args: vec![],
            },
            files: vec![],
            env: Env::new(),
            variables,
        };

        assert!(launch_warnings(&resolution(None), &Env::new(), None).is_empty());
        assert!(launch_warnings(&preset(granted.clone(), ClaudeLogin::Token), &token, None).is_empty());
        let behind_fnox = proxy(vec![CLAUDE_CODE_TOKEN.into()]);
        assert!(
            launch_warnings(
                &preset(granted.clone(), ClaudeLogin::Token),
                &Env::new(),
                Some(&behind_fnox)
            )
            .is_empty(),
            "fnox supplies the token"
        );
        assert_eq!(
            launch_warnings(
                &preset(granted.clone(), ClaudeLogin::Token),
                &Env::new(),
                Some(&proxy(vec![]))
            ),
            [LaunchWarning::NoClaudeToken]
        );
        assert!(launch_warnings(&preset(granted, ClaudeLogin::Keychain), &Env::new(), None).is_empty());
        let unset = launch_warnings(
            &preset(ClaudeCodeFolder::Unset, ClaudeLogin::Keychain),
            &Env::new(),
            None,
        );
        assert_eq!(unset, [LaunchWarning::ClaudeCodeFolder(ClaudeCodeFolder::Unset)]);
        assert!(unset[0].to_string().contains("PI_CLAUDE_CODE_CONFIG_DIR isn't set"));
        let interactive = ClaudeCodeFolder::Interactive {
            path: "/h/.claude".into(),
        };
        let both = launch_warnings(&preset(interactive.clone(), ClaudeLogin::Token), &Env::new(), None);
        assert_eq!(
            both,
            [
                LaunchWarning::ClaudeCodeFolder(interactive),
                LaunchWarning::NoClaudeToken
            ]
        );
        assert!(both[0].to_string().contains("interactive Claude Code"));

        // A socket folder the user names that the preset doesn't open: a warning naming the variable.
        let refused = resolution(Some(ClaudeCode {
            folder: granted_folder(),
            login: ClaudeLogin::Keychain,
            sockets: Some(sandbox::ClaudeCodeSockets::Refused {
                var: "CLAUDE_CODE_TMPDIR",
                path: "/private/tmp".into(),
                reason: "it's /private/tmp".into(),
            }),
        }));
        let warned = launch_warnings(&refused, &Env::new(), None);
        assert!(
            matches!(
                warned.as_slice(),
                [LaunchWarning::ClaudeCodeSockets {
                    var: "CLAUDE_CODE_TMPDIR",
                    ..
                }]
            ),
            "{warned:?}"
        );
        assert!(
            warned[0].to_string().contains("unset CLAUDE_CODE_TMPDIR"),
            "{}",
            warned[0]
        );
    }

    fn granted_folder() -> sandbox::ClaudeCodeFolder {
        sandbox::ClaudeCodeFolder::Granted("/h/.config/pi/claude".into())
    }

    #[test]
    fn host_runs_pi_directly() {
        let config = DaemonConfig::for_tests();
        let argv = command_line(&config, LaunchMode::Host, &SessionArg::New, Path::new("/app/piton"));
        assert_eq!(argv, ["pi", "--mode", "rpc"]);
    }

    #[test]
    fn resolves_programs_against_pis_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tool");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: Env = [("PATH".to_string(), format!("/nonexistent:{}", dir.path().display()))].into();
        assert_eq!(resolve_program("tool", &env), bin.to_string_lossy());
        assert_eq!(resolve_program("missing", &env), "missing");
        assert_eq!(resolve_program("/abs/tool", &env), "/abs/tool");
    }

    #[test]
    fn parses_env0() {
        let env = parse_env0(b"A=1\0B=x=y\0\0");
        assert_eq!(env.get("A").map(String::as_str), Some("1"));
        assert_eq!(env.get("B").map(String::as_str), Some("x=y"));
    }
}
