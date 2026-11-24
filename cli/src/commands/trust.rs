//! `piton trust status|allow|deny|reset`, and the trust question `piton start` asks in a terminal.

use std::{io::IsTerminal, path::PathBuf, process::ExitCode};

use super::{
    TrustAction,
    control::{control, report},
};
use crate::{
    apps::TrustDecisionWire,
    config::DaemonConfig,
    daemon::{ControlRequest, ControlResponse},
    trust::{TrustNeeded, TrustState},
};

pub async fn run(action: TrustAction, config: &DaemonConfig) -> ExitCode {
    let here = |workspace: Option<PathBuf>| workspace.or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    match action {
        TrustAction::Status { workspace, json } => {
            let response = control(
                &config.socket,
                ControlRequest::TrustStatus {
                    workspace: here(workspace),
                },
            )
            .await;
            match response {
                Ok(ControlResponse::Trust { source, trust }) => {
                    match json {
                        true => println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({"source": source, "trust": trust}))
                                .unwrap_or_default()
                        ),
                        false => print_state(&source, &trust),
                    }
                    ExitCode::SUCCESS
                }
                other => report(other),
            }
        }
        TrustAction::Allow { workspace } => decide(config, here(workspace), TrustDecisionWire::Trusted).await,
        TrustAction::Deny { workspace } => decide(config, here(workspace), TrustDecisionWire::Declined).await,
        TrustAction::Reset { workspace } => {
            match control(
                &config.socket,
                ControlRequest::TrustReset {
                    workspace: here(workspace),
                },
            )
            .await
            {
                Ok(ControlResponse::Trust { source, .. }) => {
                    println!("{}: the next launch asks", source.display());
                    ExitCode::SUCCESS
                }
                other => report(other),
            }
        }
    }
}

/// Records a decision for the grants the repository has now: what `status` shows is what's decided.
async fn decide(config: &DaemonConfig, workspace: PathBuf, decision: TrustDecisionWire) -> ExitCode {
    let grants = match control(
        &config.socket,
        ControlRequest::TrustStatus {
            workspace: workspace.clone(),
        },
    )
    .await
    {
        Ok(ControlResponse::Trust { trust, .. }) => grants_of(&trust),
        other => return report(other),
    };
    match control(
        &config.socket,
        ControlRequest::TrustDecide {
            workspace,
            decision,
            grants,
        },
    )
    .await
    {
        Ok(ControlResponse::Trust { source, trust }) => {
            print_state(&source, &trust);
            ExitCode::SUCCESS
        }
        other => report(other),
    }
}

fn grants_of(trust: &TrustState) -> crate::config::Grants {
    match trust {
        TrustState::NothingToTrust => Default::default(),
        TrustState::Trusted { grants, .. }
        | TrustState::Declined { grants, .. }
        | TrustState::Needed { grants, .. } => grants.clone(),
    }
}

fn print_state(source: &std::path::Path, trust: &TrustState) {
    let (word, grants, resources, added) = match trust {
        TrustState::NothingToTrust => {
            println!(
                "{}: nothing to trust (no committed grants, no project files pi loads)",
                source.display()
            );
            return;
        }
        TrustState::Trusted { grants, resources, .. } => ("trusted", grants, resources, None),
        TrustState::Declined { grants, resources } => ("declined", grants, resources, None),
        TrustState::Needed {
            grants,
            resources,
            added,
            ..
        } => ("not decided", grants, resources, Some(added)),
    };
    println!("{}: {word}", source.display());
    for line in contents(grants, resources, added) {
        println!("{line}");
    }
}

/// What trusting the repository lets in, as lines: its grants (new ones marked), pi's project files, MCP servers.
fn contents(
    grants: &crate::config::Grants,
    resources: &crate::trust::Resources,
    added: Option<&crate::config::Grants>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if !grants.is_empty() {
        lines.push("committed grants:".to_string());
        lines.extend(
            crate::trust::lines(grants, added.unwrap_or(grants))
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    if !resources.pi.is_empty() {
        lines.push("pi's project files (extensions run code in pi's process):".to_string());
        lines.extend(resources.pi.iter().map(|path| format!("  {path}")));
    }
    if !resources.mcp_servers.is_empty() {
        lines.push("MCP servers in .piton/mcp.json:".to_string());
        lines.extend(resources.mcp_servers.iter().map(|name| format!("  {name}")));
    }
    lines
}

/// What the user answered at the terminal.
pub enum Answer {
    Trust,
    DontTrust,
    Cancel,
}

/// Asks in the terminal when stdin and stderr are one; `None` when there's nobody to ask.
pub fn ask(needed: &TrustNeeded) -> Option<Answer> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return None;
    }
    eprintln!("{} isn't trusted yet. Trusting it applies:", needed.source.display());
    let added = match needed.added.len() == needed.grants.len() {
        true => None,
        false => Some(&needed.added),
    };
    for line in contents(&needed.grants, &needed.resources, added) {
        eprintln!("{line}");
    }
    eprint!("Trust it? [y]es, [n]o (its restrictions only), [c]ancel: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    Some(match line.trim().to_lowercase().as_str() {
        "y" | "yes" => Answer::Trust,
        "n" | "no" => Answer::DontTrust,
        _ => Answer::Cancel,
    })
}

/// Records `answer` for the grants the user was shown.
pub async fn answer(config: &DaemonConfig, needed: &TrustNeeded, answer: &Answer) -> Result<(), ExitCode> {
    let decision = match answer {
        Answer::Trust => TrustDecisionWire::Trusted,
        Answer::DontTrust => TrustDecisionWire::Declined,
        Answer::Cancel => return Err(ExitCode::from(1)),
    };
    match control(
        &config.socket,
        ControlRequest::TrustDecide {
            workspace: needed.source.clone(),
            decision,
            grants: needed.grants.clone(),
        },
    )
    .await
    {
        Ok(ControlResponse::Trust { .. }) => Ok(()),
        other => Err(report(other)),
    }
}
