//! `piton ls`, `piton start`, `piton stop`: live sessions.

use std::{path::PathBuf, process::ExitCode};

use super::control::{control, report};
use crate::{
    config::DaemonConfig,
    daemon::{ControlRequest, ControlResponse},
    launch::LaunchMode,
    session::SessionInfo,
};

pub async fn ls(config: &DaemonConfig, json: bool) -> ExitCode {
    match control(&config.socket, ControlRequest::List).await {
        Ok(ControlResponse::Sessions { sessions }) => {
            print_sessions(&sessions, json);
            ExitCode::SUCCESS
        }
        other => report(other),
    }
}

pub async fn start(config: &DaemonConfig, workspace: Option<PathBuf>, host: bool, prompt: Option<String>) -> ExitCode {
    let workspace = match workspace.map_or_else(std::env::current_dir, Ok) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("piton: {e}");
            return ExitCode::from(2);
        }
    };
    let mode = if host { LaunchMode::Host } else { LaunchMode::Sandbox };
    let request = || ControlRequest::Start {
        workspace: workspace.clone(),
        mode,
        prompt: prompt.clone(),
    };
    let mut response = control(&config.socket, request()).await;
    // Not decided yet: ask here when there's a terminal, then start again; otherwise say how to answer.
    if let Ok(ControlResponse::TrustNeeded { needed }) = &response {
        let Some(answer) = super::trust::ask(needed) else {
            eprintln!("piton: {needed}");
            return ExitCode::from(1);
        };
        if let Err(code) = super::trust::answer(config, needed, &answer).await {
            return code;
        }
        response = control(&config.socket, request()).await;
    }
    match response {
        Ok(ControlResponse::Started { session }) => {
            println!(
                "started {} in {} ({:?})",
                session.key,
                session.workspace.display(),
                session.mode
            );
            ExitCode::SUCCESS
        }
        Ok(ControlResponse::TrustNeeded { needed }) => {
            eprintln!("piton: {needed}");
            ExitCode::from(1)
        }
        other => report(other),
    }
}

pub async fn stop(config: &DaemonConfig, session: String) -> ExitCode {
    match control(&config.socket, ControlRequest::Stop { session }).await {
        Ok(ControlResponse::Stopped { forced }) => {
            if forced {
                eprintln!("piton: pi didn't exit in time and was force-stopped");
            }
            ExitCode::SUCCESS
        }
        other => report(other),
    }
}

fn print_sessions(sessions: &[SessionInfo], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(sessions).unwrap_or_default());
        return;
    }
    for s in sessions {
        println!(
            "{:>4}  {:<11} {:<8} clients={}  {}  {}",
            s.key,
            format!("{:?}", s.status).to_lowercase(),
            format!("{:?}", s.mode).to_lowercase(),
            s.clients,
            s.workspace.display(),
            s.id.as_deref().unwrap_or("-"),
        );
    }
}
