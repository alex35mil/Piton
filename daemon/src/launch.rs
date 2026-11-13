//! Building pi's environment and command line for a launch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::Config;
use crate::project::{self, ProjectError, Workspace};
use crate::settings::{DaemonSettings, SettingsError};
use crate::wire::LaunchMode;

pub type Env = BTreeMap<String, String>;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("workspace {path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("mise env failed in {workspace}: {stderr}")]
    Mise { workspace: PathBuf, stderr: String },
    #[error("can't run {program}: {source}")]
    Spawn { program: String, source: std::io::Error },
    #[error("can't write the sandbox profile in {path}: {source}")]
    Profile { path: PathBuf, source: std::io::Error },
    #[error(transparent)]
    Project(#[from] ProjectError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Sandbox(#[from] crate::sandbox::SandboxError),
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

/// pi's environment for `workspace` in `mode`: the base environment, `mise env` for that folder, then what piton
/// decides, set last so a repository's `mise.toml` can't override it: the runtime (which permissions apply), the
/// global tier, and this binary (the pi package resolves configuration through it).
pub async fn workspace_env(
    config: &Config,
    base: &Env,
    workspace: &Path,
    mode: LaunchMode,
) -> Result<Env, LaunchError> {
    let mut env = mise_env(config, base, workspace).await?;
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
    Ok(env)
}

/// The launch's configuration: `[sandbox]` and `[workers]` of the workspace's chain, refusing them when they're invalid.
/// The pi package checks its own parts when pi starts.
pub fn resolve_settings(config: &Config, workspace: &Workspace) -> Result<DaemonSettings, LaunchError> {
    Ok(crate::settings::resolve_daemon(workspace, &config.paths)?)
}

async fn mise_env(config: &Config, base: &Env, workspace: &Path) -> Result<Env, LaunchError> {
    let mut env = base.clone();
    if config.env.mise {
        let out = Command::new(resolve_program("mise", base))
            .args(["env", "--json", "-C"])
            .arg(workspace)
            .env_clear()
            .envs(base)
            .output()
            .await
            .map_err(|source| LaunchError::Spawn {
                program: "mise".into(),
                source,
            })?;
        if !out.status.success() {
            return Err(LaunchError::Mise {
                workspace: workspace.to_path_buf(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        let vars: BTreeMap<String, String> = serde_json::from_slice(&out.stdout).map_err(|e| LaunchError::Mise {
            workspace: workspace.to_path_buf(),
            stderr: format!("unexpected mise output: {e}"),
        })?;
        env.extend(vars);
    }
    Ok(env)
}

/// The full argument vector for pi in `mode`. `profile` is the generated sandbox profile (see [`write_profile`]),
/// required for sandboxed launches.
pub fn command_line(config: &Config, mode: LaunchMode, session: &SessionArg, profile: Option<&Path>) -> Vec<String> {
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
            let mut argv = config.sandbox.command.clone();
            argv.extend(["run".into(), "-s".into(), "--profile".into()]);
            argv.push(profile.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
            argv.extend(["--allow-cwd".into(), "--".into()]);
            argv.extend(pi);
            argv
        }
    }
}

/// Writes the launch's sandbox profile (see sandbox.rs) to a file of its own (`key`), removed when the session ends
/// ([`ProfileFile`]). The folder is the daemon's private state, which no sandbox reads; nono reads the profile before
/// it applies the sandbox.
/// `base` is the daemon's base environment (the login shell's): `$VAR` grants are expanded from it.
pub fn write_profile(
    config: &Config,
    resolved: &DaemonSettings,
    base: &Env,
    key: u64,
) -> Result<ProfileFile, LaunchError> {
    let profile = crate::sandbox::profile(resolved, &config.paths.home, base, &config.agent_socket())?;
    let dir = profiles_dir(config);
    let write = || -> std::io::Result<PathBuf> {
        crate::diagnostics::private_dir(&dir)?;
        let path = dir.join(format!("{key}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(&profile).unwrap_or_default())?;
        Ok(path)
    };
    write().map(ProfileFile).map_err(|source| LaunchError::Profile {
        path: dir.clone(),
        source,
    })
}

/// Where generated profiles are written: next to the daemon's socket (its state folder), outside every sandbox.
fn profiles_dir(config: &Config) -> PathBuf {
    config.state_dir().join("profiles")
}

/// A launch's generated sandbox profile; removed when dropped.
#[derive(Debug)]
pub struct ProfileFile(PathBuf);

impl ProfileFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProfileFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_wraps_pi_in_nono() {
        let config = Config::default();
        let argv = command_line(
            &config,
            LaunchMode::Sandbox,
            &SessionArg::Resume("/s.jsonl".into()),
            Some(Path::new("/state/profiles/1.json")),
        );
        assert_eq!(
            argv,
            [
                "nono",
                "run",
                "-s",
                "--profile",
                "/state/profiles/1.json",
                "--allow-cwd",
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
    #[tokio::test]
    async fn piton_variables_win_over_the_workspace_environment() {
        let mut config = Config::default();
        config.env.mise = false;
        config.paths.global = PathBuf::from("/global/piton");
        let base: Env = [
            ("PITON_RUNTIME".to_string(), "sandbox".to_string()),
            ("PITON_CONFIG_HOME".to_string(), "/repo/fake".to_string()),
            ("PITON_BIN".to_string(), "/repo/fake-piton".to_string()),
        ]
        .into();
        let host = workspace_env(&config, &base, Path::new("/w"), LaunchMode::Host)
            .await
            .unwrap();
        assert_eq!(host.get("PITON_RUNTIME"), None, "host runtime is unset");
        assert_eq!(host["PITON_CONFIG_HOME"], "/global/piton");
        assert_ne!(host["PITON_BIN"], "/repo/fake-piton");
        let sandbox = workspace_env(&config, &Env::new(), Path::new("/w"), LaunchMode::Sandbox)
            .await
            .unwrap();
        assert_eq!(sandbox["PITON_RUNTIME"], "sandbox");
    }

    #[test]
    fn host_runs_pi_directly() {
        let config = Config::default();
        let argv = command_line(&config, LaunchMode::Host, &SessionArg::New, None);
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
