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
            } => match self.launch_in(&workspace, mode, SessionArg::New).await {
                Ok(handle) => {
                    if let Some(text) = prompt {
                        let _ = handle.tx.send(SessionCmd::Internal(Internal::Prompt { text }));
                    }
                    ControlResponse::Started {
                        session: handle.snapshot().info(),
                    }
                }
                Err(e) => ControlResponse::Error { message: e.to_string() },
            },
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
                let mise = self.mise.clone();
                let paths = self.config().paths.clone();
                let created =
                    tokio::task::spawn_blocking(move || crate::project::create_worker(&request, mise.as_ref(), &paths))
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
            ControlRequest::WorkerDelete { path, force } => self.delete_worker(path, force).await,
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

    /// Stops the worker's sessions (after checking it can be deleted), then deletes its files.
    async fn delete_worker(self: &Arc<Self>, path: PathBuf, force: bool) -> ControlResponse {
        let checked = tokio::task::spawn_blocking(move || {
            let worker = crate::project::worker_info(&path)?;
            if !force {
                let lines = crate::project::worker_leftovers(&worker.path);
                if !lines.is_empty() {
                    return Err(crate::project::WorkerRefusal::Leftovers {
                        path: worker.path,
                        lines,
                    });
                }
            }
            Ok(worker)
        })
        .await;
        let worker = match checked {
            Ok(Ok(worker)) => worker,
            Ok(Err(refusal)) => return ControlResponse::WorkerRefused { refusal },
            Err(e) => return ControlResponse::Error { message: e.to_string() },
        };
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
        let mise = self.mise.clone();
        match tokio::task::spawn_blocking(move || crate::project::remove_worker(&worker, force, mise.as_ref())).await {
            Ok(Ok(warning)) => ControlResponse::WorkerDeleted { stopped, warning },
            Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
            Err(e) => ControlResponse::Error { message: e.to_string() },
        }
    }
}
