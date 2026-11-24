//! The daemon's Git on a source checkout: inside the sandbox a launch there gets, hooks on. See `project/source.rs`.

use std::{path::Path, process::Command};

use super::{
    ClaudeSockets, Env, LaunchError, helper, prepare_sandbox, resolve_config, resolve_program,
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
    /// A filtered network's gate for Git's fetches, while this lives: the policy's entries decide, with nobody to ask.
    _gate: Option<crate::launch::sandbox::network::Gate>,
}

impl SourceGit {
    /// Resolves the sandbox of a launch in `source`, as [`prepare_sandbox`] does for pi (environment tools included).
    pub async fn for_source(
        config: &DaemonConfig,
        tools: &crate::tools::Tools,
        base: &Env,
        source: &Path,
    ) -> Result<Self, LaunchError> {
        let workspace = Workspace {
            path: source.to_path_buf(),
            checkout: source.to_path_buf(),
            relation: Relation::Source,
        };
        let resolved = resolve_config(config, &workspace)?;
        // A credential proxy that applies here is checked against this sandbox too (no keychain, no writable fnox
        // files), so the user's hooks don't run where launches are refused. Git itself doesn't run behind the proxy.
        let sandboxed = prepare_sandbox(config, tools, &resolved, base, source, ClaudeSockets::None).await?;
        let mut policy = sandboxed.resolution.policy;
        let gate = gate_for(&mut policy, config, source)?;
        Ok(Self {
            env: sandboxed.env,
            policy,
            containment: containment(),
            helper: helper()?,
            _gate: gate,
        })
    }
}

/// A gate for Git under a filtered network: the policy's `connect` entries decide; a host they don't list is refused
/// and logged (no session to ask). Git doesn't run behind fnox, so the gate dials directly.
fn gate_for(
    policy: &mut Policy,
    config: &DaemonConfig,
    source: &Path,
) -> Result<Option<crate::launch::sandbox::network::Gate>, LaunchError> {
    use crate::launch::sandbox::{Gate, Network};
    match &policy.network {
        Network::Open { .. } | Network::Direct { .. } => return Ok(None),
        Network::Filtered { .. } => {}
    }
    let decider = policy.clone();
    let source = source.to_path_buf();
    let decide: crate::launch::sandbox::network::Decide = std::sync::Arc::new(move |_user, to| {
        let verdict = match decider.decide_network(&to).allowed {
            true => crate::launch::sandbox::network::Verdict::Allow {
                literal: to.host.parse::<std::net::IpAddr>().is_ok(),
            },
            false => {
                crate::log!(
                    "git in {}: the network gate refused {to} (no connect entry lists it)",
                    source.display()
                );
                crate::launch::sandbox::network::Verdict::Deny {
                    reason: format!("Piton's sandbox refused {to}: no `connect` entry lists it"),
                }
            }
        };
        Box::pin(async move { verdict })
    });
    let (refused, _) = tokio::sync::mpsc::unbounded_channel();
    let gate = crate::launch::sandbox::network::Gate::start(
        decide,
        Some(crate::launch::sandbox::network::Route::Direct),
        refused,
    )
    .map_err(|source| LaunchError::Spawn {
        program: "the network gate".into(),
        source,
    })?;
    let (user, credential) = gate.issue("git");
    if let Network::Filtered { gate: slot, .. } = &mut policy.network {
        *slot = Gate::Listening {
            port: gate.port,
            user,
            credential,
            ssh: crate::launch::sandbox::network::ssh::ensure(&config.state_dir()),
        };
    }
    Ok(Some(gate))
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
