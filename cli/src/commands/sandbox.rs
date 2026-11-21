//! `piton sandbox show|why`: the sandbox a launch in a workspace gets, resolved exactly as the daemon resolves it (the
//! login shell's environment, the workspace's `mise env` run inside the sandbox), and the rule that decides one access.

use std::{path::PathBuf, process::ExitCode};

use super::SandboxAction;
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
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap_or_default());
            ExitCode::SUCCESS
        }
        SandboxAction::Why {
            path, write, connect, ..
        } => {
            let access = match (write, connect) {
                (false, false) => Access::Read,
                (true, false) => Access::Write,
                (false, true) => Access::Connect,
                (true, true) => {
                    eprintln!("piton: --write or --connect, not both");
                    return ExitCode::from(1);
                }
            };
            let target = crate::agent::canonical_target(&std::path::absolute(&path).unwrap_or(path));
            let decision = policy.decide(&target, access);
            let verb = match access {
                Access::Read => "read",
                Access::Write => "write",
                Access::Connect => "connect to",
            };
            let outcome = if decision.allowed { "allowed" } else { "denied" };
            println!("{outcome}: {verb} {}", target.display());
            println!("  because: {}", explain(&decision.source));
            if let Some(line) = silenced_line(&silence, &target, decision.allowed) {
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
    launch::prepare_sandbox(config, &tools, &resolved, &base, &workspace.path)
        .await
        .map(|sandboxed| (sandboxed.resolution, sandboxed.warnings))
        .map_err(|e| e.to_string())
}

/// The line saying a block here is only logged, when a `silence` entry covers `target` and it's denied.
fn silenced_line(silence: &[launch::sandbox::Silenced], target: &std::path::Path, allowed: bool) -> Option<String> {
    match allowed {
        true => None,
        false => launch::sandbox::silenced(silence, target).map(|s| {
            format!(
                "  silenced: {} (sandbox.silence, listed in {}): a block here is only logged",
                s.path.display(),
                s.file.display()
            )
        }),
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
            "a file fnox loads, or a folder on its path: which secrets go where (never writable)".into()
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
        let silence = [launch::sandbox::Silenced {
            path: "/h/Library/Caches/tool".into(),
            file: "/h/.config/piton/config.toml".into(),
        }];
        let line = silenced_line(&silence, Path::new("/h/Library/Caches/tool/x"), false).unwrap();
        assert!(
            line.contains("silenced") && line.contains("/h/.config/piton/config.toml"),
            "{line}"
        );
        assert_eq!(
            silenced_line(&silence, Path::new("/h/Library/Caches/tool/x"), true),
            None
        );
        assert_eq!(silenced_line(&silence, Path::new("/h/other"), false), None);
    }
}
