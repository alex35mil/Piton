//! `piton worker create|ls|delete`: workers through the daemon. With `--json` it prints the daemon's response for
//! scripts and editor tooling (nvim's `cli.lua`); refusals exit 4 so callers can tell them from failures.

use std::{path::PathBuf, process::ExitCode};

use super::{
    SourceUpdateFlag, WorkerAction,
    control::{control, report},
};
use crate::{
    config::DaemonConfig,
    daemon::{ControlRequest, ControlResponse},
    project::source::{CreateUpdate, DeleteUpdate, Sha},
};

/// Exit code for a refused worker operation, so callers can tell it from a failure.
const EXIT_REFUSED: u8 = 4;

pub async fn run(config: &DaemonConfig, action: WorkerAction, json: bool) -> ExitCode {
    let here = || std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let sha = |value: String| Sha::try_from(value).map_err(|e| format!("--approve-source-update: {e}"));
    let is_delete = match &action {
        WorkerAction::Delete { .. } => true,
        WorkerAction::Create { .. } | WorkerAction::Ls { .. } => false,
    };
    let request = match action {
        WorkerAction::Create {
            name,
            source,
            label,
            item_id,
            approve_source_update,
        } => {
            let update = match approve_source_update.map(sha).transpose() {
                Ok(None) => CreateUpdate::Ask,
                Ok(Some(target)) => CreateUpdate::Approve { target },
                Err(message) => return report(Err(message)),
            };
            ControlRequest::WorkerCreate(crate::project::CreateRequest {
                source: source.unwrap_or_else(here),
                name,
                label,
                item_id,
                update,
            })
        }
        WorkerAction::Ls { source } => ControlRequest::WorkerList {
            source: source.unwrap_or_else(here),
        },
        WorkerAction::Delete {
            path,
            force,
            source_update,
        } => {
            let update = match source_update {
                None => DeleteUpdate::Ask,
                Some(SourceUpdateFlag::SkipSourceUpdate) => DeleteUpdate::Skip,
                Some(SourceUpdateFlag::ApproveSourceUpdate(value)) => match sha(value) {
                    Ok(target) => DeleteUpdate::Approve { target },
                    Err(message) => return report(Err(message)),
                },
            };
            ControlRequest::WorkerDelete { path, force, update }
        }
    };
    let response = match control(&config.socket, request).await {
        Ok(response) => response,
        Err(message) => return report(Err(message)),
    };
    let code = match &response {
        ControlResponse::Worker { .. } | ControlResponse::Workers { .. } | ControlResponse::WorkerDeleted { .. } => {
            ExitCode::SUCCESS
        }
        ControlResponse::WorkerRefused { .. } => ExitCode::from(EXIT_REFUSED),
        ControlResponse::Sessions { .. }
        | ControlResponse::Started { .. }
        | ControlResponse::Stopped { .. }
        | ControlResponse::Logging { .. }
        | ControlResponse::TrustNeeded { .. }
        | ControlResponse::Trust { .. }
        | ControlResponse::TrustChanged { .. }
        | ControlResponse::Error { .. } => ExitCode::from(1),
    };
    if json {
        println!("{}", serde_json::to_string(&response).unwrap_or_default());
        return code;
    }
    match response {
        ControlResponse::Worker { worker } => println!("{}", worker.path.display()),
        ControlResponse::Workers { workers } => {
            for w in workers {
                println!(
                    "{}  {}  sessions={}",
                    w.worker.path.display(),
                    w.worker.label,
                    w.sessions.len()
                );
            }
        }
        ControlResponse::WorkerDeleted { stopped, warning } => {
            if stopped > 0 {
                println!("stopped {stopped} session(s)");
            }
            if let Some(warning) = warning {
                eprintln!("piton: {warning}");
            }
        }
        ControlResponse::WorkerRefused { refusal } => {
            eprintln!("piton: {refusal}");
            if let crate::project::WorkerRefusal::Leftovers { lines, .. }
            | crate::project::WorkerRefusal::SourceDirty { status: lines, .. } = &refusal
            {
                for line in lines {
                    eprintln!("  {line}");
                }
            }
            if let crate::project::WorkerRefusal::ProtectedFilesUpdate(blocked) = &refusal {
                eprintln!(
                    "  Approving lets Piton write exactly these files from {}; the rest of the update, hooks \
                     included, runs in the sandbox. To approve, run the command again with --approve-source-update \
                     {}{}",
                    blocked.target,
                    blocked.target,
                    match is_delete {
                        true => ", or with --skip-source-update to leave the source as it is",
                        false => "",
                    }
                );
            }
        }
        other @ (ControlResponse::Sessions { .. }
        | ControlResponse::Started { .. }
        | ControlResponse::Stopped { .. }
        | ControlResponse::Logging { .. }
        | ControlResponse::TrustNeeded { .. }
        | ControlResponse::Trust { .. }
        | ControlResponse::TrustChanged { .. }
        | ControlResponse::Error { .. }) => return report(Ok(other)),
    }
    code
}
