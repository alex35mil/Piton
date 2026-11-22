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
        SandboxAction::Show { workspace } | SandboxAction::Why { workspace, .. } => workspace.clone(),
    };
    let (policy, profile, silence) = match policy(config, workspace).await {
        Ok((resolution, warnings)) => {
            for link in &resolution.links {
                eprintln!("piton: warning: {link}");
            }
            for warning in &warnings {
                eprintln!("piton: warning: {warning}");
            }
            (resolution.policy, resolution.profile, resolution.silence)
        }
        Err(message) => {
            eprintln!("piton: {message}");
            return ExitCode::from(3);
        }
    };
    match action {
        SandboxAction::Show { .. } => {
            let output = serde_json::json!({
                "nono_deny_groups": launch::sandbox::nono_version(),
                "profile": profile,
                "policy": policy,
                "silence": silence
                    .iter()
                    .map(|s| serde_json::json!({ "entry": s.entry(), "source": s.source.to_string() }))
                    .collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap_or_default());
            ExitCode::SUCCESS
        }
        SandboxAction::Why { path, access, .. } => {
            let access = match access {
                None => Access::Read,
                Some(WhyAccess::Write) => Access::Write,
                Some(WhyAccess::Connect) => Access::Connect,
                Some(WhyAccess::Bind) => Access::Bind,
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

async fn policy(
    config: &DaemonConfig,
    workspace: Option<PathBuf>,
) -> Result<(Resolution, Vec<launch::LaunchWarning>), String> {
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
    .map(|sandboxed| (sandboxed.resolution, sandboxed.warnings))
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
