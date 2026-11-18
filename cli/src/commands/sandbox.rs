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
    let policy = match policy(config, workspace).await {
        Ok(resolution) => {
            for link in &resolution.links {
                eprintln!("piton: warning: {link}");
            }
            resolution.policy
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
            if decision.allowed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(4)
            }
        }
    }
}

async fn policy(config: &DaemonConfig, workspace: Option<PathBuf>) -> Result<Resolution, String> {
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
        .map(|sandboxed| sandboxed.resolution)
        .map_err(|e| e.to_string())
}

fn explain(source: &Source) -> String {
    match source {
        Source::Workspace => "the workspace is writable".into(),
        Source::BuiltIn { what } => format!("built in: {what}"),
        Source::NonoGroup { group } => format!("built-in credential deny (nono's {group}); lift it with `expose`"),
        Source::Piton { what } => format!("Piton: {what}"),
        Source::AgentDir => "pi's runtime state in its agent directory".into(),
        Source::Preset { preset } => format!("the {preset} preset"),
        Source::Config { key } => format!("{key} in the configuration (`piton config check` lists the files)"),
    }
}
