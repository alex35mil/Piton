//! Control requests: one request and one response per connection (`piton ls`, `start`, `stop`, `worker`, `logging`).

use std::{path::PathBuf, sync::Arc};

use super::{
    Daemon,
    protocol::{ControlRequest, ControlResponse, WorkerEntry},
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
            ControlRequest::WorkerCreate(request) => {
                let source = match tokio::task::spawn_blocking({
                    let from = request.from.clone();
                    move || crate::project::source_of(&from)
                })
                .await
                {
                    Ok(Ok(source)) => source,
                    Ok(Err(refusal)) => return ControlResponse::WorkerRefused { refusal },
                    Err(e) => return ControlResponse::Error { message: e.to_string() },
                };
                let git = match self.source_git(&source).await {
                    Ok(git) => git,
                    Err(refusal) => return ControlResponse::WorkerRefused { refusal },
                };
                let mise = match self.worker_mise() {
                    Ok(mise) => mise,
                    Err(refusal) => return ControlResponse::WorkerRefused { refusal },
                };
                let paths = self.config().paths.clone();
                let filter = self.source_filter(&source);
                let created = tokio::task::spawn_blocking(move || {
                    crate::project::create_worker(&request, mise.as_ref(), &paths, &filter, &git)
                })
                .await;
                match created {
                    Ok(Ok(worker)) => ControlResponse::Worker { worker },
                    Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
                    Err(e) => ControlResponse::Error { message: e.to_string() },
                }
            }
            ControlRequest::WorkerList { from } => {
                match tokio::task::spawn_blocking(move || crate::project::list_workers(&from)).await {
                    Ok(Ok(workers)) => {
                        let sessions: Vec<_> = lock(&self.sessions).values().map(|s| s.snapshot().info()).collect();
                        let workers = workers
                            .into_iter()
                            .map(|worker| WorkerEntry {
                                sessions: sessions
                                    .iter()
                                    .filter(|s| s.workspace.starts_with(&worker.path))
                                    .cloned()
                                    .collect(),
                                worker,
                            })
                            .collect();
                        ControlResponse::Workers { workers }
                    }
                    Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
                    Err(e) => ControlResponse::Error { message: e.to_string() },
                }
            }
            ControlRequest::WorkerDelete { path, force, update } => self.delete_worker(path, force, update).await,
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

    /// Which of a source checkout's committed grants apply to work the daemon does there (worker Git, the update rule),
    /// from the user's decision; none when it can't be read.
    fn source_filter(&self, source: &std::path::Path) -> crate::config::GrantFilter {
        crate::launch::resolve_workspace(source)
            .ok()
            .and_then(|ws| self.trust_state(&ws).ok())
            .map_or(crate::config::GrantFilter::None, |state| state.filter())
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

    /// Git for a source checkout, run in the sandbox a launch there gets. Creation needs it; deletion goes on without
    /// it.
    async fn source_git(
        &self,
        source: &std::path::Path,
    ) -> Result<crate::launch::git::SourceGit, crate::project::WorkerRefusal> {
        let filter = self.source_filter(source);
        crate::launch::git::SourceGit::for_source(&self.config(), &self.tools, &self.base_env, source, &filter)
            .await
            .map_err(|e| crate::project::WorkerRefusal::InvalidConfiguration {
                message: format!("can't sandbox Git for {}: {e}", source.display()),
            })
    }

    /// Checks a deletion and plans the source's update, so every question comes before anything changes; then stops
    /// the worker's sessions and deletes its files.
    async fn delete_worker(
        self: &Arc<Self>,
        path: PathBuf,
        force: bool,
        update: crate::project::source::DeleteUpdate,
    ) -> ControlResponse {
        let worker = match tokio::task::spawn_blocking(move || crate::project::worker_info(&path)).await {
            Ok(Ok(worker)) => worker,
            Ok(Err(refusal)) => return ControlResponse::WorkerRefused { refusal },
            Err(e) => return ControlResponse::Error { message: e.to_string() },
        };
        // Deletion doesn't depend on the source's sandbox: without it, the source is only left as it is.
        let git: Arc<Result<crate::launch::git::SourceGit, String>> = Arc::new(
            self.source_git(&worker.source)
                .await
                .map_err(|refusal| refusal.to_string()),
        );
        let paths = self.config().paths.clone();
        let filter = self.source_filter(&worker.source);
        let planned = tokio::task::spawn_blocking({
            let git = git.clone();
            move || crate::project::plan_removal(worker, force, &update, source_git(&git), &paths, &filter)
        })
        .await;
        let removal = match planned {
            Ok(Ok(removal)) => removal,
            Ok(Err(refusal)) => return ControlResponse::WorkerRefused { refusal },
            Err(e) => return ControlResponse::Error { message: e.to_string() },
        };
        // Before any session stops: mise, for the worker's trust, enabled but not installed refuses the deletion.
        let mise = match self.worker_mise() {
            Ok(mise) => mise,
            Err(refusal) => return ControlResponse::WorkerRefused { refusal },
        };
        let worker = removal.worker.clone();
        let handles: Vec<_> = lock(&self.sessions)
            .values()
            // Sessions in the worker or in a folder inside it.
            .filter(|s| s.snapshot().workspace.starts_with(&worker.path))
            .cloned()
            .collect();
        let stopped = handles.len();
        let mut stops = tokio::task::JoinSet::new();
        for handle in handles {
            crate::log!("session {}: stopping for worker delete", handle.key);
            stops.spawn(async move { handle.stop(StopReason::Requested).await });
        }
        while stops.join_next().await.is_some() {}
        // Leftovers were checked above (or forced); the sessions just stopped can't have added commits worth keeping
        // without the user seeing them, but check again unless forced.
        match tokio::task::spawn_blocking(move || {
            crate::project::remove_worker(&removal, mise.as_ref(), source_git(&git))
        })
        .await
        {
            Ok(Ok(warning)) => ControlResponse::WorkerDeleted { stopped, warning },
            Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
            Err(e) => ControlResponse::Error { message: e.to_string() },
        }
    }
}

/// The sandboxed Git a deletion uses, or why there's none.
fn source_git(git: &Result<crate::launch::git::SourceGit, String>) -> crate::project::source::SourceGit<'_> {
    match git {
        Ok(git) => crate::project::source::SourceGit::Ready(git),
        Err(why) => crate::project::source::SourceGit::Unavailable(why.clone()),
    }
}
