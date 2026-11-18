//! Building pi's environment and command line for a launch.

pub mod env;
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
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Sandbox(#[from] crate::launch::sandbox::SandboxError),
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

/// A launch whose environment and, for a sandboxed one, policy are resolved.
pub enum Prepared {
    Host { env: Env },
    Sandbox { env: Env, resolution: sandbox::Resolution },
}

/// Builds a launch in `workspace` in `mode`.
pub async fn prepare(
    config: &DaemonConfig,
    resolved: &LaunchConfig,
    base: &Env,
    workspace: &Path,
    mode: LaunchMode,
) -> Result<Prepared, LaunchError> {
    Ok(match mode {
        LaunchMode::Host => Prepared::Host {
            env: host_env(config, base, workspace).await?,
        },
        LaunchMode::Sandbox => {
            let (env, resolution) = prepare_sandbox(config, resolved, base, workspace).await?;
            Prepared::Sandbox { env, resolution }
        }
    })
}

/// pi's environment for a host launch in `workspace`: the base environment and the environment tools' for that folder,
/// run on the host (a host session trusts the project by choice), then Piton's own variables.
async fn host_env(config: &DaemonConfig, base: &Env, workspace: &Path) -> Result<Env, LaunchError> {
    let env = env::host(&env::enabled(&config.env), base, workspace).await?;
    Ok(with_piton_vars(config, env, LaunchMode::Host))
}

/// A sandboxed launch's environment and policy. The environment tools run inside the sandbox, under the launch's
/// policy with the host's agent directory (none of their values yet); the launch's policy is then resolved from their
/// environment, which accepts a widening value only from a file the agent can't write.
pub async fn prepare_sandbox(
    config: &DaemonConfig,
    resolved: &LaunchConfig,
    base: &Env,
    workspace: &Path,
) -> Result<(Env, sandbox::Resolution), LaunchError> {
    let bootstrap = sandbox_policy(config, resolved, base, &env::SandboxEnv::daemon(base), workspace)?;
    let tools = env::enabled(&config.env);
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
    let resolution = sandbox_policy(config, resolved, base, &launch_env, workspace)?;
    Ok((
        with_piton_vars(config, launch_env.into_env(), LaunchMode::Sandbox),
        resolution,
    ))
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
            // The helper runs pi with `execvp`, which searches the PATH it was started with: pi's.
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
        config.env.mise = false;
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
