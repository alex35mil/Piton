//! A workspace's relation to its project: whether it's a source checkout or a worker cloned from one.
//!
//! Everything that decides the sandbox and what agents may do is read from the **source** checkout (config/), never
//! from a worker, so an agent can't widen its own sandbox by editing files in its clone. The relation itself comes from
//! the worker's local git config (`agent.source`), which lives in `.git/` and is checked against the folder layout
//! (`<parent>/<repo>--<name>` next to `<parent>/<repo>`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The only path a worker shares with its source, as a symlink (the pi package's workflow artifacts).
pub const SHARED_PATH: &str = crate::config::LOCAL_DIR;
/// Separator between a source's folder name and a worker's name.
pub const WORKER_SEPARATOR: &str = "--";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    /// A checkout that isn't a worker (or isn't a git repository at all).
    Source,
    /// A worker cloned from `source`, named `name` (`<repo>--<name>`).
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
        "{path} says it's a worker of {claimed}, but it isn't laid out as one (expected <parent>/<repo>{WORKER_SEPARATOR}<name> next to <parent>/<repo>)"
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

/// A worker's name: `<name>` in `<repo>--<name>`, for a worker laid out next to its source.
pub fn worker_name(worker: &Path, source: &Path) -> Option<String> {
    if !is_worker_layout(worker, source) {
        return None;
    }
    let (wn, sn) = (
        worker.file_name()?.to_string_lossy(),
        source.file_name()?.to_string_lossy(),
    );
    Some(wn[sn.len() + WORKER_SEPARATOR.len()..].to_string())
}

/// `worker` is `<parent>/<repo>--<name>` and `source` is `<parent>/<repo>`.
pub fn is_worker_layout(worker: &Path, source: &Path) -> bool {
    let (Some(wp), Some(sp)) = (worker.parent(), source.parent()) else {
        return false;
    };
    let (Some(wn), Some(sn)) = (worker.file_name(), source.file_name()) else {
        return false;
    };
    let (wn, sn) = (wn.to_string_lossy(), sn.to_string_lossy());
    wp == sp && wn.len() > sn.len() + WORKER_SEPARATOR.len() && wn.starts_with(&format!("{sn}{WORKER_SEPARATOR}"))
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
        assert!(is_worker_layout(Path::new("/p/repo--fix-x"), source));
        assert_eq!(
            worker_name(Path::new("/p/repo--fix-x"), source).as_deref(),
            Some("fix-x")
        );
        // A source whose own name contains the separator: the worker name is what follows the source's name.
        assert_eq!(
            worker_name(Path::new("/p/a--b--fix"), Path::new("/p/a--b")).as_deref(),
            Some("fix")
        );
        assert_eq!(worker_name(Path::new("/q/repo--fix-x"), source), None);
        assert!(!is_worker_layout(Path::new("/p/repo--"), source));
        assert!(!is_worker_layout(Path::new("/p/repox--fix"), source));
        assert!(!is_worker_layout(Path::new("/q/repo--fix"), source));
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
