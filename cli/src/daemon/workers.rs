//! Worker operations, one function each, for every caller: the control socket (`piton worker`, so nvim) and a
//! session's channel (`agent/workers.rs`, after the user approves). Operations on one source run one at a time,
//! whoever asks: each create or delete holds its source's lock from its first check to its last change.

use std::{path::PathBuf, sync::Arc};

use super::{Daemon, protocol::WorkerEntry};
use crate::{
    project::{
        CreateRequest, WorkerInfo, WorkerRefusal,
        source::{DeleteUpdate, SourceGit},
    },
    session::{StopReason, lock},
};

/// A deleted worker: the sessions that were running in it and got stopped, and a warning when the source couldn't be
/// fast-forwarded afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    pub stopped: usize,
    pub warning: Option<String>,
}

type Result<T> = std::result::Result<T, WorkerRefusal>;

/// Runs blocking work off the runtime; a panic is a failed operation.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(WorkerRefusal::Failed { message: e.to_string() }))
}

/// One lock per source checkout (canonical), made on first use.
#[derive(Debug, Default)]
pub struct SourceLocks(std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>);

impl SourceLocks {
    /// Waits for `source`'s lock: held for a whole create or delete there.
    pub async fn hold(&self, source: &std::path::Path) -> tokio::sync::OwnedMutexGuard<()> {
        let slot = lock(&self.0)
            .entry(source.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        slot.lock_owned().await
    }
}

impl Daemon {
    async fn source_lock(&self, source: &std::path::Path) -> tokio::sync::OwnedMutexGuard<()> {
        self.worker_locks.hold(source).await
    }

    /// Creates a worker of `request.source`'s source.
    pub async fn create_worker(&self, request: CreateRequest) -> Result<WorkerInfo> {
        let source = blocking({
            let from = request.source.clone();
            move || crate::project::source_of(&from)
        })
        .await?;
        let _held = self.source_lock(&source).await;
        let git = self.source_git(&source).await?;
        let mise = self.worker_mise()?;
        let paths = self.config().paths.clone();
        let filter = self.source_filter(&source);
        blocking(move || crate::project::create_worker(&request, mise.as_ref(), &paths, &filter, &git)).await
    }

    /// The live sessions in `worker` or a folder inside it.
    pub fn sessions_in(&self, worker: &std::path::Path) -> Vec<crate::session::SessionKey> {
        lock(&self.sessions)
            .values()
            .filter(|s| s.snapshot().workspace.starts_with(worker))
            .map(|s| s.key)
            .collect()
    }

    /// The workers of `from`'s source, each with its live sessions.
    pub async fn list_workers(&self, from: PathBuf) -> Result<Vec<WorkerEntry>> {
        let workers = blocking(move || crate::project::list_workers(&from)).await?;
        let sessions: Vec<_> = lock(&self.sessions).values().map(|s| s.snapshot().info()).collect();
        Ok(workers
            .into_iter()
            .map(|worker| WorkerEntry {
                sessions: sessions
                    .iter()
                    .filter(|s| s.workspace.starts_with(&worker.path))
                    .cloned()
                    .collect(),
                worker,
            })
            .collect())
    }

    /// Checks a deletion and plans the source's update, so every question comes before anything changes; then stops
    /// the worker's sessions and deletes its files.
    pub async fn delete_worker(&self, path: PathBuf, force: bool, update: DeleteUpdate) -> Result<Deleted> {
        let worker = blocking(move || crate::project::worker_info(&path)).await?;
        let _held = self.source_lock(&worker.source).await;
        // Deletion doesn't depend on the source's sandbox: without it, the source is only left as it is.
        let git: Arc<std::result::Result<crate::launch::git::SourceGit, String>> = Arc::new(
            self.source_git(&worker.source)
                .await
                .map_err(|refusal| refusal.to_string()),
        );
        let paths = self.config().paths.clone();
        let filter = self.source_filter(&worker.source);
        let removal = blocking({
            let git = git.clone();
            move || crate::project::plan_removal(worker, force, &update, source_git(&git), &paths, &filter)
        })
        .await?;
        // Before any session stops: mise, for the worker's trust, enabled but not installed refuses the deletion.
        let mise = self.worker_mise()?;
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
        let warning =
            blocking(move || crate::project::remove_worker(&removal, mise.as_ref(), source_git(&git))).await?;
        Ok(Deleted { stopped, warning })
    }

    /// Which of a source checkout's committed grants apply to work the daemon does there (worker Git, the update rule,
    /// `[workers.permissions]`), from the user's decision; none when it can't be read.
    pub(crate) fn source_filter(&self, source: &std::path::Path) -> crate::config::GrantFilter {
        crate::launch::resolve_workspace(source)
            .ok()
            .and_then(|ws| self.trust_state(&ws).ok())
            .map_or(crate::config::GrantFilter::None, |state| state.filter())
    }

    /// Git for a source checkout, run in the sandbox a launch there gets. Creation needs it; deletion goes on without
    /// it.
    async fn source_git(&self, source: &std::path::Path) -> Result<crate::launch::git::SourceGit> {
        let filter = self.source_filter(source);
        crate::launch::git::SourceGit::for_source(&self.config(), &self.integrations, &self.base_env, source, &filter)
            .await
            .map_err(|e| WorkerRefusal::InvalidConfiguration {
                message: format!("can't sandbox Git for {}: {e}", source.display()),
            })
    }
}

/// The sandboxed Git a deletion uses, or why there's none.
fn source_git(git: &std::result::Result<crate::launch::git::SourceGit, String>) -> SourceGit<'_> {
    match git {
        Ok(git) => SourceGit::Ready(git),
        Err(why) => SourceGit::Unavailable(why.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Two operations on one source run one after another; on two sources, at once.
    #[tokio::test]
    async fn one_operation_at_a_time_per_source() {
        let locks = SourceLocks::default();
        let (a, b) = (PathBuf::from("/p/a"), PathBuf::from("/p/b"));
        let held = locks.hold(&a).await;
        let same = tokio::time::timeout(Duration::from_millis(50), locks.hold(&a)).await;
        assert!(same.is_err(), "the same source waits");
        let other = tokio::time::timeout(Duration::from_millis(50), locks.hold(&b)).await;
        assert!(other.is_ok(), "another source doesn't");
        drop(held);
        let after = tokio::time::timeout(Duration::from_millis(50), locks.hold(&a)).await;
        assert!(after.is_ok(), "free once released");
    }
}
