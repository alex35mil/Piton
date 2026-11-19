//! The daemon's Git on a source checkout: inside the sandbox a launch there gets, hooks on. See `project/source.rs`.

use std::{path::Path, process::Command};

use super::{
    Env, LaunchError, helper, prepare_sandbox, resolve_config, resolve_program,
    sandbox::{Containment, Policy, contained, containment},
};
use crate::{
    config::DaemonConfig,
    project::{Relation, Workspace, source::GitRunner},
};

/// Git for one source checkout: the policy and environment a sandboxed launch there gets, resolved once.
pub struct SourceGit {
    /// pi's environment for a sandboxed launch in the source: what Git runs with.
    env: Env,
    policy: Policy,
    containment: Containment,
    helper: std::path::PathBuf,
}

impl SourceGit {
    /// Resolves the sandbox of a launch in `source`, as [`prepare_sandbox`] does for pi (environment tools included).
    pub async fn for_source(config: &DaemonConfig, base: &Env, source: &Path) -> Result<Self, LaunchError> {
        let workspace = Workspace {
            path: source.to_path_buf(),
            checkout: source.to_path_buf(),
            relation: Relation::Source,
        };
        let resolved = resolve_config(config, &workspace)?;
        let (env, resolution) = prepare_sandbox(config, &resolved, base, source).await?;
        Ok(Self {
            env,
            policy: resolution.policy,
            containment: containment(),
            helper: helper()?,
        })
    }
}

impl GitRunner for SourceGit {
    fn sandboxed(&self, checkout: &Path, args: &[&str]) -> Result<String, String> {
        let program = resolve_program("git", &self.env);
        let mut git_args: Vec<std::ffi::OsString> = vec!["-C".into(), checkout.as_os_str().to_os_string()];
        git_args.extend(args.iter().map(std::ffi::OsString::from));
        let call = contained(
            &self.helper,
            &self.policy,
            self.containment,
            program,
            git_args,
            &self.env,
        )?;
        let out = Command::new(&call.program)
            .args(&call.args)
            .env_clear()
            .envs(&call.env)
            // Never wait on a prompt nobody sees: a credential request fails instead.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("can't run git: {e}"))?;
        match out.status.success() {
            true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
            false => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(match stderr.is_empty() {
                    true => format!("git exited with {}", out.status),
                    false => stderr,
                })
            }
        }
    }
}
