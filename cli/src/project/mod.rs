//! A workspace's relation to its project: whether it's a source checkout or a worker cloned from one.
//!
//! Everything that decides the sandbox and what agents may do is read from the **source** checkout (config/), never
//! from a worker, so an agent can't widen its own sandbox by editing files in its clone. The relation itself comes from
//! the worker's local git config (`agent.source`), which lives in `.git/` and is checked against the folder layout
//! (`<parent>/<repo>.workers/<name>` next to `<parent>/<repo>`).

pub mod source;
pub mod tools;
mod worker;

use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub use self::worker::{
    CreateRequest, Mise, WorkerInfo, WorkerRefusal, containing as worker_containing, create as create_worker,
    info as worker_info, list as list_workers, path as worker_path, permissions as worker_permissions, plan_removal,
    remove as remove_worker, source_of,
};

/// The only path a worker shares with its source, as a symlink (the pi package's workflow artifacts).
pub const SHARED_PATH: &str = crate::config::LOCAL_DIR;
/// What a source's folder name gets to name the folder its workers live in, next to it: `<repo>.workers`.
pub const WORKERS_SUFFIX: &str = ".workers";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    /// A checkout that isn't a worker (or isn't a git repository at all).
    Source,
    /// A worker cloned from `source`, named `name` (`<repo>.workers/<name>`).
    Worker { source: PathBuf, name: String },
}

#[derive(Debug, Clone)]
pub struct Workspace {
    /// The workspace pi runs in (canonical): a checkout or a folder inside one.
    pub path: PathBuf,
    /// The checkout `path` is in (its top folder), or `path` itself when it isn't in one.
    pub checkout: PathBuf,
    /// What `checkout` is.
    pub relation: Relation,
}

impl Workspace {
    /// The checkout whose files decide this workspace's sandbox and settings: never a worker, and never a folder inside
    /// a checkout, both of which an agent working there could edit.
    pub fn source(&self) -> &Path {
        match &self.relation {
            Relation::Source => &self.checkout,
            Relation::Worker { source, .. } => source,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error(
        "{path} says it's a worker of {claimed}, but it isn't laid out as one (expected <parent>/<repo>{WORKERS_SUFFIX}/<name> next to <parent>/<repo>)"
    )]
    BadWorker { path: PathBuf, claimed: PathBuf },
}

/// Resolves `path` (canonical) to a workspace: the checkout it's in and what that checkout is. A checkout with
/// `agent.source` in its local git config is a worker; the claim is accepted only when the layout matches, so a stray
/// config value can't point a sandbox elsewhere. Every enclosing checkout is considered, nearest first, so a nested
/// repository inside a worker (a submodule) is still part of that worker.
pub fn resolve(path: &Path) -> Result<Workspace, ProjectError> {
    let mut nearest: Option<&Path> = None;
    for checkout in path.ancestors().filter(|dir| dir.join(".git").exists()) {
        nearest = nearest.or(Some(checkout));
        let Some(source) = git_config_local(checkout, "agent.source") else {
            continue;
        };
        let source = std::fs::canonicalize(&source).unwrap_or_else(|_| PathBuf::from(&source));
        let Some(name) = worker_name(checkout, &source) else {
            return Err(ProjectError::BadWorker {
                path: checkout.to_path_buf(),
                claimed: source,
            });
        };
        return Ok(Workspace {
            path: path.to_path_buf(),
            checkout: checkout.to_path_buf(),
            relation: Relation::Worker { source, name },
        });
    }
    Ok(Workspace {
        path: path.to_path_buf(),
        checkout: nearest.unwrap_or(path).to_path_buf(),
        relation: Relation::Source,
    })
}

/// The folder `source`'s workers live in: `<parent>/<repo>.workers`, next to it. `None` for a source with no parent or
/// folder name (the file system root), which can't have workers.
pub fn workers_dir(source: &Path) -> Option<PathBuf> {
    let name = source.file_name()?;
    let mut folder = name.to_os_string();
    folder.push(WORKERS_SUFFIX);
    Some(source.parent()?.join(folder))
}

/// A worker's name: `<name>` when `worker` is `<parent>/<repo>.workers/<name>` and `source` is `<parent>/<repo>`.
pub fn worker_name(worker: &Path, source: &Path) -> Option<String> {
    if worker.parent()? != workers_dir(source)? {
        return None;
    }
    let name = worker.file_name()?.to_string_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

/// `worker` is laid out as a worker of `source`: `<parent>/<repo>.workers/<name>` next to `<parent>/<repo>`.
pub fn is_worker_layout(worker: &Path, source: &Path) -> bool {
    worker_name(worker, source).is_some()
}

/// `key` in the local config of the repository rooted at `path` (callers check that `path` has a `.git`).
fn git_config_local(path: &Path, key: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["config", "--local", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// A plain relative path inside the checkout: no absolute paths, `..`, or empty segments.
pub fn validate_relative(path: &str) -> Result<(), &'static str> {
    if path.is_empty() || path.starts_with('/') {
        return Err("must be a relative path");
    }
    if path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Err("must not contain empty, `.`, or `..` segments");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_layout() {
        let source = Path::new("/p/repo");
        assert_eq!(workers_dir(source), Some(PathBuf::from("/p/repo.workers")));
        assert_eq!(workers_dir(Path::new("/")), None);
        assert!(is_worker_layout(Path::new("/p/repo.workers/fix-x"), source));
        assert_eq!(
            worker_name(Path::new("/p/repo.workers/fix-x"), source).as_deref(),
            Some("fix-x")
        );
        // A source whose own name looks like a container: its workers are in `<name>.workers`, and it stays a source.
        assert_eq!(
            worker_name(Path::new("/p/a.workers.workers/fix"), Path::new("/p/a.workers")).as_deref(),
            Some("fix")
        );
        assert!(!is_worker_layout(Path::new("/p/a.workers"), Path::new("/p/a")));
        // Not inside this source's container: another parent, another repository's container, the old layout, deeper,
        // or the container itself.
        assert!(!is_worker_layout(Path::new("/q/repo.workers/fix"), source));
        assert!(!is_worker_layout(Path::new("/p/repox.workers/fix"), source));
        assert!(!is_worker_layout(Path::new("/p/repo--fix"), source));
        assert!(!is_worker_layout(Path::new("/p/repo.workers/fix/sub"), source));
        assert!(!is_worker_layout(Path::new("/p/repo.workers"), source));
        assert!(!is_worker_layout(Path::new("/p/repo"), source));
    }

    #[test]
    fn relative_paths() {
        assert!(validate_relative("apps/.build/index-build").is_ok());
        assert!(validate_relative("/abs").is_err());
        assert!(validate_relative("a/../b").is_err());
        assert!(validate_relative("a//b").is_err());
        assert!(validate_relative("").is_err());
    }

    /// pi started in a folder inside a checkout belongs to that checkout.
    #[test]
    fn subfolders_resolve_to_their_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap().join("repo");
        let sub = repo.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let ws = resolve(&sub).unwrap();
        assert_eq!(
            (ws.path.as_path(), ws.checkout.as_path()),
            (sub.as_path(), repo.as_path())
        );
        assert_eq!(ws.relation, Relation::Source);
        assert_eq!(ws.source(), repo.as_path());
        // Outside any checkout, the folder stands for itself.
        let loose = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(resolve(&loose).unwrap().checkout, loose);
    }
}
