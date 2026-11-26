//! Control requests: one request and one response per connection (`piton ls`, `start`, `stop`, `worker`, `logging`).

use std::sync::Arc;

use super::{
    Daemon,
    protocol::{ControlRequest, ControlResponse},
};
use crate::{
    launch::SessionArg,
    session::{Internal, SessionCmd, StopReason, lock},
};

impl Daemon {
    pub(super) async fn control(self: &Arc<Self>, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::List => {
                let sessions = lock(&self.sessions).values().map(|s| s.snapshot().info()).collect();
                ControlResponse::Sessions { sessions }
            }
            ControlRequest::Start {
                workspace,
                mode,
                prompt,
            } => match self
                .launch_in(&workspace, mode, SessionArg::New, super::TrustCheck::Ask)
                .await
            {
                Ok(handle) => {
                    if let Some(text) = prompt {
                        let _ = handle.tx.send(SessionCmd::Internal(Internal::Prompt { text }));
                    }
                    ControlResponse::Started {
                        session: handle.snapshot().info(),
                    }
                }
                Err(crate::launch::LaunchError::TrustNeeded(needed)) => {
                    ControlResponse::TrustNeeded { needed: *needed }
                }
                Err(e) => ControlResponse::Error { message: e.to_string() },
            },
            ControlRequest::TrustStatus { workspace } => {
                let resolved = crate::launch::resolve_workspace(&workspace).map_err(|e| e.to_string());
                match resolved.and_then(|ws| {
                    self.trust_state(&ws)
                        .map(|trust| (ws.source().to_path_buf(), trust))
                        .map_err(|e| e.to_string())
                }) {
                    Ok((source, trust)) => ControlResponse::Trust { source, trust },
                    Err(message) => ControlResponse::Error { message },
                }
            }
            ControlRequest::TrustDecide {
                workspace,
                decision,
                grants,
            } => self.trust_response(&workspace, self.decide_trust(&workspace, decision.into(), &grants)),
            ControlRequest::TrustReset { workspace } => self.trust_response(&workspace, self.reset_trust(&workspace)),
            ControlRequest::Stop { session } => {
                let Some(handle) = self.find(&session) else {
                    return ControlResponse::Error {
                        message: format!("no live session matches {session}"),
                    };
                };
                crate::log!("session {}: stop requested ({session})", handle.key);
                let forced = handle
                    .stop(StopReason::Requested)
                    .await
                    .is_some_and(|outcome| outcome.forced);
                ControlResponse::Stopped { forced }
            }
            ControlRequest::WorkerCreate(request) => match self.create_worker(request).await {
                Ok(worker) => ControlResponse::Worker { worker },
                Err(refusal) => ControlResponse::WorkerRefused { refusal },
            },
            ControlRequest::WorkerList { source } => match self.list_workers(source).await {
                Ok(workers) => ControlResponse::Workers { workers },
                Err(refusal) => ControlResponse::WorkerRefused { refusal },
            },
            ControlRequest::WorkerDelete { path, force, update } => {
                match self.delete_worker(path, force, update).await {
                    Ok(super::Deleted { stopped, warning }) => ControlResponse::WorkerDeleted { stopped, warning },
                    Err(refusal) => ControlResponse::WorkerRefused { refusal },
                }
            }
            ControlRequest::Logging => ControlResponse::Logging {
                logging: crate::logging::current_policy(),
            },
            ControlRequest::SetLogging { policy } => {
                self.set_logging(policy);
                ControlResponse::Logging {
                    logging: crate::logging::current_policy(),
                }
            }
        }
    }

    fn trust_response(
        &self,
        workspace: &std::path::Path,
        result: Result<crate::trust::TrustState, super::TrustError>,
    ) -> ControlResponse {
        let source = crate::launch::resolve_workspace(workspace).map(|ws| ws.source().to_path_buf());
        match (result, source) {
            (Ok(trust), Ok(source)) => ControlResponse::Trust { source, trust },
            (Err(super::TrustError::Decide(crate::trust::DecideError::Changed { checkout, state })), _) => {
                ControlResponse::TrustChanged {
                    source: checkout,
                    trust: *state,
                }
            }
            (
                Err(
                    e @ (super::TrustError::Workspace(_)
                    | super::TrustError::Decide(crate::trust::DecideError::State(_))),
                ),
                _,
            ) => ControlResponse::Error { message: e.to_string() },
            (Ok(_), Err(e)) => ControlResponse::Error { message: e.to_string() },
        }
    }
}
