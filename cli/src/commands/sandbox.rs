//! `piton sandbox show|why`: the sandbox a launch in a workspace gets, resolved exactly as the daemon resolves it (the
//! login shell's environment, the workspace's `mise env` run inside the sandbox), and the rule that decides one access.

use std::{path::PathBuf, process::ExitCode};

use super::{SandboxAction, WhyAccess};
use crate::{
    config::DaemonConfig,
    launch::{
        self,
        sandbox::{Access, Resolution, Source},
    },
};

pub async fn run(action: SandboxAction, config: &DaemonConfig) -> ExitCode {
    let workspace = match &action {
        SandboxAction::Show { workspace }
        | SandboxAction::Why { workspace, .. }
        | SandboxAction::Gate { workspace } => workspace.clone(),
    };
    let (policy, profile, silence, catalog) = match policy(config, workspace).await {
        Ok((resolution, warnings, catalog)) => {
            for link in &resolution.links {
                eprintln!("piton: warning: {link}");
            }
            for warning in &warnings {
                eprintln!("piton: warning: {warning}");
            }
            (resolution.policy, resolution.profile, resolution.silence, catalog)
        }
        Err(message) => {
            eprintln!("piton: {message}");
            return ExitCode::from(3);
        }
    };
    match action {
        SandboxAction::Gate { .. } => serve_gate(policy, catalog, config).await,
        SandboxAction::Show { .. } => {
            let output = serde_json::json!({
                "nono_deny_groups": launch::sandbox::nono_version(),
                "profile": profile,
                "policy": policy,
                "silence": silence
                    .iter()
                    .map(|s| serde_json::json!({ "entry": s.entry(), "source": s.source.to_string() }))
                    .collect::<Vec<_>>(),
                // Hosts pi reaches without asking under a filtered network, and the models pi discovered (listed,
                // never trusted).
                "pi_hosts": catalog.trusted(),
                "pi_discovered_models": catalog.discovered(),
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap_or_default());
            ExitCode::SUCCESS
        }
        SandboxAction::Why { path, access, .. } => {
            let access = match access {
                None => Access::Read,
                Some(WhyAccess::Write) => Access::Write,
                Some(WhyAccess::Connect) => {
                    // `host:port` is a network destination; a path is a Unix socket.
                    let text = path.to_string_lossy().into_owned();
                    match (text.starts_with('/'), launch::sandbox::Destination::parse(&text)) {
                        (false, Some(to)) => {
                            let decision = policy.decide_network(&to);
                            let (allowed, because) = match (decision.allowed, catalog.allows(&to.host, to.port)) {
                                (true, _) => (true, explain(&decision.source)),
                                (false, Some(pi)) => (true, format!("pi reaches it: {}", pi.source)),
                                (false, None) => (
                                    false,
                                    format!(
                                        "{}; in a session, Piton's gate asks you first",
                                        explain(&decision.source)
                                    ),
                                ),
                            };
                            let outcome = if allowed { "allowed" } else { "denied" };
                            println!("{outcome}: connect to {to}");
                            println!("  because: {because}");
                            return match allowed {
                                true => ExitCode::SUCCESS,
                                false => ExitCode::from(4),
                            };
                        }
                        (true, _) | (false, None) => Access::Connect,
                    }
                }
                Some(WhyAccess::UnixBind) => Access::Bind,
                Some(WhyAccess::Listen) => {
                    // Seatbelt decides every address and port alike: the address is only shown.
                    let address = path.to_string_lossy().into_owned();
                    let decision = policy.decide_listen();
                    let outcome = if decision.allowed { "allowed" } else { "denied" };
                    println!("{outcome}: listen on TCP at {address}");
                    println!("  because: {}", explain(&decision.source));
                    if decision.allowed {
                        println!(
                            "  note: the sandbox can't limit listening to this Mac: a listener on any address is \
                             reachable from the LAN and tailnet"
                        );
                    }
                    return match decision.allowed {
                        true => ExitCode::SUCCESS,
                        false => ExitCode::from(4),
                    };
                }
                Some(WhyAccess::Service) => {
                    let service = path.to_string_lossy().into_owned();
                    let decision = policy.decide_service(&service);
                    let outcome = if decision.allowed { "allowed" } else { "denied" };
                    println!("{outcome}: look up the Mach service {service}");
                    println!("  because: {}", explain(&decision.source));
                    return match decision.allowed {
                        true => ExitCode::SUCCESS,
                        false => ExitCode::from(4),
                    };
                }
            };
            let target = crate::agent::canonical_target(&std::path::absolute(&path).unwrap_or(path));
            let decision = policy.decide(&target, access);
            let verb = match access {
                Access::Read => "read",
                Access::Write => "write",
                Access::Connect => "connect to",
                Access::Bind => "create a socket at",
            };
            let outcome = if decision.allowed { "allowed" } else { "denied" };
            println!("{outcome}: {verb} {}", target.display());
            println!("  because: {}", explain(&decision.source));
            for line in silenced_lines(&silence, &target, decision.allowed) {
                println!("{line}");
            }
            if decision.allowed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(4)
            }
        }
    }
}

/// Starts a gate for `policy` that the policy's entries and pi's hosts decide, prints the policy with it, and serves until
/// stdin closes.
async fn serve_gate(
    mut policy: launch::sandbox::Policy,
    catalog: crate::pi::catalog::Catalog,
    config: &DaemonConfig,
) -> ExitCode {
    use launch::sandbox::{Gate, Network};
    let decider = policy.clone();
    let catalog = std::sync::Arc::new(catalog);
    let decide: crate::launch::sandbox::network::Decide = std::sync::Arc::new(move |_user, to| {
        let literal = to.host.parse::<std::net::IpAddr>().is_ok();
        let verdict = match decider.decide_network(&to).allowed || catalog.allows(&to.host, to.port).is_some() {
            true => crate::launch::sandbox::network::Verdict::Allow { literal },
            false => crate::launch::sandbox::network::Verdict::Deny {
                reason: format!("Piton's sandbox refused {to}: no connect entry lists it"),
            },
        };
        Box::pin(async move { verdict })
    });
    let (refused, _) = tokio::sync::mpsc::unbounded_channel();
    let gate = match crate::launch::sandbox::network::Gate::start(
        decide,
        Some(crate::launch::sandbox::network::Route::Direct),
        refused,
    ) {
        Ok(gate) => gate,
        Err(e) => {
            eprintln!("piton: can't start the gate: {e}");
            return ExitCode::from(3);
        }
    };
    let (user, credential) = gate.issue("probe");
    match &mut policy.network {
        Network::Open { .. } => {
            eprintln!("piton: the network is open: no gate");
            return ExitCode::from(3);
        }
        Network::Filtered { gate: slot, .. } => {
            *slot = Gate::Listening {
                port: gate.port,
                user,
                credential,
                ssh: crate::launch::sandbox::network::ssh::ensure(&config.state_dir()),
            };
        }
    }
    println!("{}", serde_json::to_string(&policy).unwrap_or_default());
    use tokio::io::AsyncReadExt;
    let mut sink = Vec::new();
    let _ = tokio::io::stdin().read_to_end(&mut sink).await;
    drop(gate);
    ExitCode::SUCCESS
}

async fn policy(
    config: &DaemonConfig,
    workspace: Option<PathBuf>,
) -> Result<(Resolution, Vec<launch::LaunchWarning>, crate::pi::catalog::Catalog), String> {
    let dir = workspace
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    launch::check_workspace_links(&dir).map_err(|e| e.to_string())?;
    let workspace = launch::resolve_workspace(&dir).map_err(|e| e.to_string())?;
    let resolved = launch::resolve_config(config, &workspace).map_err(|e| e.to_string())?;
    let base: launch::Env = match config.env.login_shell {
        true => launch::login_shell_env()
            .await
            .unwrap_or_else(|| std::env::vars().collect()),
        false => std::env::vars().collect(),
    };
    let tools = crate::tools::Tools::resolve(config, &base);
    // `show` and `why` describe a session's policy without starting one: no socket folder of its own, so a user's
    // `XDG_RUNTIME_DIR` or `CLAUDE_CODE_TMPDIR` is shown as it is, and Piton's own folder isn't.
    launch::prepare_sandbox(
        config,
        &tools,
        &resolved,
        &base,
        &workspace.path,
        launch::ClaudeSockets::None,
    )
    .await
    .map(|sandboxed| {
        // pi's hosts, as a session's gate trusts them.
        let builtin = config
            .pi
            .command
            .first()
            .and_then(|pi| crate::pi::catalog::builtin_for(pi, &sandboxed.env).ok())
            .unwrap_or_default();
        let files = crate::pi::catalog::Files::new(
            sandboxed.resolution.agent_dirs[1].clone(),
            &config.paths.global,
            &resolved.levels,
        );
        let catalog = crate::pi::catalog::Catalog::new(builtin, files);
        (sandboxed.resolution, sandboxed.warnings, catalog)
    })
    .map_err(|e| e.to_string())
}

/// The lines saying a block here is only logged, one per `silence` entry that covers `target` when it's denied: for
/// every process, or the one the entry names.
fn silenced_lines(silence: &[launch::sandbox::Silenced], target: &std::path::Path, allowed: bool) -> Vec<String> {
    match allowed {
        true => Vec::new(),
        false => silence
            .iter()
            .filter(|s| {
                let any = s.process.as_deref().unwrap_or_default();
                let probe = launch::sandbox::Silenced {
                    process: None,
                    ..(*s).clone()
                };
                probe.matches(any, "file-read-data", &target.to_string_lossy())
            })
            .map(|s| {
                let whose = match &s.process {
                    Some(process) => format!("by {process}"),
                    None => "by any process".to_string(),
                };
                format!(
                    "  silenced {whose}: {} ({}): a block here is only logged",
                    s.entry().inline(),
                    s.source
                )
            })
            .collect(),
    }
}

fn explain(source: &Source) -> String {
    match source {
        Source::Workspace => "the workspace is writable".into(),
        Source::BuiltIn { what } => format!("built in: {what}"),
        Source::NonoGroup { group } => format!(
            "built-in credential deny (nono's {group}); lift it with `read` in the global config.toml or a \
             config.local.toml"
        ),
        Source::Piton { what } => format!("Piton: {what}"),
        Source::FnoxConfig => {
            "a file fnox loads, or a folder on its path: which secrets go where; the file changes only \
                               through a write the user approves (the edit or write tool), its folders never"
                .into()
        }
        Source::AgentDir => "pi's runtime state in its agent directory".into(),
        Source::Preset { preset, from } => format!("the {preset} preset, {from}"),
        Source::Config { key, from } => format!("{key}, {from}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A denied path under a `silence` entry says so, with the file that listed it; an allowed one doesn't.
    #[test]
    fn why_names_a_silenced_path() {
        use launch::sandbox::{SilenceSource, Silenced, Silent};
        let silence = [
            Silenced {
                target: Silent::Path("/h/Library/Caches/tool".into()),
                process: None,
                source: SilenceSource::File("/h/.config/piton/config.toml".into()),
            },
            Silenced {
                target: Silent::Path("/h/Library/Caches/*".into()),
                process: Some("git".into()),
                source: SilenceSource::File("/h/.config/piton/config.toml".into()),
            },
        ];
        let lines = silenced_lines(&silence, Path::new("/h/Library/Caches/tool/x"), false);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("any process") && lines[0].contains("/h/.config/piton/config.toml"),
            "{lines:?}"
        );
        assert!(lines[1].contains("by git"), "{lines:?}");
        assert!(silenced_lines(&silence, Path::new("/h/Library/Caches/tool/x"), true).is_empty());
        assert!(silenced_lines(&silence, Path::new("/h/other"), false).is_empty());
    }
}
