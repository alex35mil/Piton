//! Updating a source checkout around worker creation and deletion: fetch its default branch and fast-forward it.
//!
//! The source is a checkout a sandboxed agent may have worked in, so whatever Git runs there (hooks, `core.fsmonitor`,
//! and the files they read) may be the agent's. The fetch and the fast-forward run inside the sandbox a launch in the
//! source gets, with every hook on, so the user's hooks keep running, contained. Read-only Git commands run on the host
//! with hooks and fsmonitor off. Nothing here ever runs Git on the host with hooks on.
//!
//! A fast-forward that changes protected files (`.piton/`, `.pi/`) can't be done by that sandbox, which can't write
//! them. The operation is refused before anything changes, with what the client needs to ask the user. When the user
//! approves that exact commit, the daemon writes those files itself, from the commit, by hash and without following a
//! symlink, then the fast-forward runs in the sandbox with every rule in place, so hooks can't write any protected file
//! (`.piton/config.local.toml` included). The commit is the one the remote reports, never a remote-tracking ref an agent
//! could have rewritten. `[workers.permissions] protected_files_update` answers the question before anyone is asked.
//! See "Workers" in docs/design.md.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

use crate::agent::{FileMode, ProtectedRoots, remove_file, write_file};

/// A full commit SHA, validated where it enters (a request, Git's output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha(String);

impl TryFrom<String> for Sha {
    type Error = String;

    fn try_from(value: String) -> Result<Self, String> {
        let valid = matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit());
        match valid {
            true => Ok(Self(value.to_ascii_lowercase())),
            false => Err(format!("{value:?} isn't a full commit SHA")),
        }
    }
}

impl From<Sha> for String {
    fn from(sha: Sha) -> Self {
        sha.0
    }
}

impl std::fmt::Display for Sha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Sha {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a create request says about a source update that changes protected files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CreateUpdate {
    /// Refuse with [`Blocked`] so the client can ask.
    #[default]
    Ask,
    /// The user approved this commit's changes to protected files.
    Approve { target: Sha },
}

/// What a delete request says about the source update after the worker is gone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteUpdate {
    /// Refuse with [`Blocked`] so the client can ask.
    #[default]
    Ask,
    /// The user approved this commit's changes to protected files.
    Approve { target: Sha },
    /// Leave the source as it is: it may stay behind its upstream.
    Skip,
}

/// A source update that changes protected files and waits for the user: nothing has changed yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error(
    "updating {checkout} to {upstream} ({target}) changes protected files, which the sandbox can't write: {}",
    files.join(", ")
)]
pub struct Blocked {
    pub checkout: PathBuf,
    /// `origin/<default branch>`.
    pub upstream: String,
    /// The commit the remote reported, which an approval names.
    pub target: Sha,
    /// The protected files the update changes, relative to the checkout.
    pub files: Vec<String>,
}

/// How Git runs on the source checkout for an update: contained by the sandbox a launch there gets, hooks on.
pub trait GitRunner {
    fn sandboxed(&self, checkout: &Path, args: &[&str]) -> Result<String, String>;
}

/// Git for an update, or why the sandbox a launch in the source gets couldn't be built (its configuration or
/// environment tools failed): then the source can't be updated, only left as it is.
pub enum SourceGit<'a> {
    Ready(&'a dyn GitRunner),
    Unavailable(String),
}

/// Why the source couldn't be fetched or fast-forwarded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error(
        "can't fetch {checkout} inside the sandbox: {stderr}. Sandboxed Git authenticates the way an agent's does: over \
         SSH, through the SSH agent (`\"$SSH_AUTH_SOCK\" = \"allow\"` in [sandbox.network.unix_socket_connections]), \
         with `~/.ssh/config`, `~/.ssh/known_hosts`, and the public keys readable, which the standard profile grants; \
         over HTTPS through the keychain, `\"~/Library/Keychains\" = \"read\"` in [sandbox.filesystem]"
    )]
    Fetch { checkout: PathBuf, stderr: String },
    #[error("origin has no branch {branch} for {checkout}")]
    NoBranch { checkout: PathBuf, branch: String },
    /// Waits for the user, also when an approval names a commit the remote has since moved past.
    #[error(transparent)]
    Blocked(Box<Blocked>),
    /// The user's `protected_files_update = "deny"` refuses an update that changes protected files.
    #[error(
        "updating {checkout} changes protected files ({}), and `protected_files_update = \"deny\"` in \
         [workers.permissions] refuses it; update the source by hand",
        files.join(", ")
    )]
    Denied { checkout: PathBuf, files: Vec<String> },
    /// The approved files can't be written as Git has them (a symlink or submodule, a folder swapped for a link).
    #[error("can't apply the approved change to {checkout}: {reason}")]
    Apply { checkout: PathBuf, reason: String },
    #[error("can't fast-forward {checkout}: {stderr}")]
    Merge { checkout: PathBuf, stderr: String },
}

/// A fetched update, checked and decided: what [`apply`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do: the source already has the commit.
    Current,
    /// The fast-forward runs in the sandbox; `approved` are the protected files the daemon writes first, from the
    /// commit, because the user approved them (empty when the update touches none).
    Sandboxed { target: Sha, approved: Vec<String> },
    /// The delete request leaves the source as it is.
    Skipped,
}

/// The update a request asks for, as both requests' choices.
#[derive(Debug, Clone, Copy)]
pub enum Requested<'a> {
    Ask,
    Approve(&'a Sha),
    Skip,
}

use crate::config::PermissionMode;

impl<'a> From<&'a CreateUpdate> for Requested<'a> {
    fn from(update: &'a CreateUpdate) -> Self {
        match update {
            CreateUpdate::Ask => Requested::Ask,
            CreateUpdate::Approve { target } => Requested::Approve(target),
        }
    }
}

impl<'a> From<&'a DeleteUpdate> for Requested<'a> {
    fn from(update: &'a DeleteUpdate) -> Self {
        match update {
            DeleteUpdate::Ask => Requested::Ask,
            DeleteUpdate::Approve { target } => Requested::Approve(target),
            DeleteUpdate::Skip => Requested::Skip,
        }
    }
}

/// Fetches `branch` inside the sandbox and decides how the source gets to the remote's commit. Changes nothing but
/// the fetched objects and refs.
pub fn plan(
    runner: &dyn GitRunner,
    checkout: &Path,
    branch: &str,
    requested: Requested<'_>,
    // `[workers.permissions] protected_files_update`: `Allow` applies the changed protected files without asking; `Deny`
    // refuses creating a worker and leaves the source as it is when deleting one; `Ask` asks the client.
    rule: PermissionMode,
) -> Result<Plan, UpdateError> {
    let fetch_failed = |stderr: String| UpdateError::Fetch {
        checkout: checkout.to_path_buf(),
        stderr,
    };
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let remote_ref = format!("refs/heads/{branch}");
    let fetch = || {
        runner
            .sandboxed(checkout, &["fetch", "origin", &refspec])
            .map_err(fetch_failed)
    };
    fetch()?;
    // What the remote has, not what an agent may have written in `refs/remotes/`.
    let target = remote_tip(runner, checkout, &remote_ref, branch)?;
    if !has_commit(checkout, &target) {
        // The remote moved between the fetch and the read: fetch once more.
        fetch()?;
        if !has_commit(checkout, &target) {
            return Err(fetch_failed(format!("the fetch didn't bring {target}")));
        }
    }
    let head = host_read(checkout, &["rev-parse", "--verify", "HEAD^{commit}"])
        .ok()
        .and_then(|head| Sha::try_from(head).ok());
    if head.as_ref() == Some(&target) || is_ancestor(checkout, &target, "HEAD") {
        return Ok(Plan::Current);
    }
    let files = protected_changes(checkout, &target).map_err(|stderr| UpdateError::Merge {
        checkout: checkout.to_path_buf(),
        stderr,
    })?;
    if files.is_empty() {
        return Ok(match requested {
            Requested::Skip => Plan::Skipped,
            Requested::Ask | Requested::Approve(_) => Plan::Sandboxed {
                target,
                approved: Vec::new(),
            },
        });
    }
    let blocked = |target: Sha, files: Vec<String>| {
        UpdateError::Blocked(Box::new(Blocked {
            checkout: checkout.to_path_buf(),
            upstream: format!("origin/{branch}"),
            target,
            files,
        }))
    };
    match (requested, rule) {
        (Requested::Skip, _) => Ok(Plan::Skipped),
        // The user's rule decides before anyone is asked.
        (Requested::Ask | Requested::Approve(_), PermissionMode::Allow) => Ok(Plan::Sandboxed {
            target,
            approved: files,
        }),
        (Requested::Ask | Requested::Approve(_), PermissionMode::Deny) => Err(UpdateError::Denied {
            checkout: checkout.to_path_buf(),
            files,
        }),
        // An approval covers only the commit the user saw.
        (Requested::Approve(approved), PermissionMode::Ask) if *approved == target => Ok(Plan::Sandboxed {
            target,
            approved: files,
        }),
        (Requested::Approve(_) | Requested::Ask, PermissionMode::Ask) => Err(blocked(target, files)),
    }
}

/// Fast-forwards the source as `plan` decided: the approved protected files first, written by the daemon from the
/// commit, then the merge in the sandbox. When the merge fails, the approved files go back to `HEAD`.
pub fn apply(runner: &dyn GitRunner, checkout: &Path, plan: &Plan) -> Result<(), UpdateError> {
    let (target, approved) = match plan {
        Plan::Current | Plan::Skipped => return Ok(()),
        Plan::Sandboxed { target, approved } => (target, approved),
    };
    let apply_failed = |reason: String| UpdateError::Apply {
        checkout: checkout.to_path_buf(),
        reason,
    };
    let root = std::fs::canonicalize(checkout).map_err(|e| apply_failed(e.to_string()))?;
    let head = host_read(checkout, &["rev-parse", "--verify", "HEAD^{commit}"]).map_err(apply_failed)?;
    // Both sides read up front, so a refusal (a symlink, a submodule) comes before anything is written.
    let to = entries(checkout, target.as_str(), approved).map_err(apply_failed)?;
    let back = entries(checkout, &head, approved).map_err(apply_failed)?;
    if let Err(reason) = set_entries(checkout, &root, &to) {
        let _ = set_entries(checkout, &root, &back);
        return Err(apply_failed(reason));
    }
    match runner.sandboxed(checkout, &["merge", "--ff-only", target.as_str()]) {
        Ok(_) => Ok(()),
        Err(stderr) => {
            let _ = set_entries(checkout, &root, &back);
            Err(UpdateError::Merge {
                checkout: checkout.to_path_buf(),
                stderr,
            })
        }
    }
}

/// A protected path as a commit has it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File { path: String, blob: String, mode: u32 },
    Missing { path: String },
}

/// Each path as `commit` has it. A symlink or submodule among them refuses: the daemon writes only regular files.
fn entries(checkout: &Path, commit: &str, paths: &[String]) -> Result<Vec<Entry>, String> {
    paths
        .iter()
        .map(|path| {
            let listed = host_read(checkout, &["ls-tree", "-z", commit, "--", path])?;
            let line = listed.split('\0').find(|line| !line.is_empty());
            let Some((meta, name)) = line.and_then(|line| line.split_once('\t')) else {
                return Ok(Entry::Missing { path: path.clone() });
            };
            match (meta.split(' ').collect::<Vec<_>>().as_slice(), name == path) {
                ([mode @ ("100644" | "100755"), "blob", blob], true) => Ok(Entry::File {
                    path: path.clone(),
                    blob: (*blob).to_string(),
                    mode: u32::from_str_radix(&mode[3..], 8).map_err(|e| e.to_string())?,
                }),
                (_, true) => Err(format!(
                    "{path} is a symlink or submodule in {commit}; change it by hand"
                )),
                (_, false) => Err(format!("unexpected `git ls-tree` output for {path}: {line:?}")),
            }
        })
        .collect()
}

/// Writes or removes each entry in the working tree without following a symlink, and stages it, with no hook run.
fn set_entries(checkout: &Path, root: &Path, entries: &[Entry]) -> Result<(), String> {
    for entry in entries {
        match entry {
            Entry::File { path, blob, mode } => {
                let content = host_bytes(checkout, &["cat-file", "blob", blob])?;
                write_file(root, &root.join(path), &content, FileMode::Set(*mode))
                    .map_err(|e| format!("{path}: {e}"))?;
                let cacheinfo = format!("{mode:o},{blob},{path}");
                host_read(checkout, &["update-index", "--add", "--cacheinfo", &cacheinfo])?;
            }
            Entry::Missing { path } => {
                remove_file(root, &root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                host_read(checkout, &["update-index", "--force-remove", "--", path])?;
            }
        }
    }
    Ok(())
}

fn remote_tip(runner: &dyn GitRunner, checkout: &Path, remote_ref: &str, branch: &str) -> Result<Sha, UpdateError> {
    let listed = runner
        .sandboxed(checkout, &["ls-remote", "origin", remote_ref])
        .map_err(|stderr| UpdateError::Fetch {
            checkout: checkout.to_path_buf(),
            stderr,
        })?;
    let sha = listed
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(_, name)| *name == remote_ref)
        .map(|(sha, _)| sha.to_string());
    match sha {
        Some(sha) => Sha::try_from(sha).map_err(|stderr| UpdateError::Fetch {
            checkout: checkout.to_path_buf(),
            stderr,
        }),
        None => Err(UpdateError::NoBranch {
            checkout: checkout.to_path_buf(),
            branch: branch.to_string(),
        }),
    }
}

/// The protected files between HEAD and `target`, relative to the checkout: the same definition the sandbox enforces,
/// so this check and the sandbox never disagree.
fn protected_changes(checkout: &Path, target: &Sha) -> Result<Vec<String>, String> {
    let changed = host_read(
        checkout,
        &[
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            "HEAD",
            target.as_str(),
            "--",
        ],
    )?;
    let canonical = std::fs::canonicalize(checkout).unwrap_or_else(|_| checkout.to_path_buf());
    let roots = ProtectedRoots {
        source: canonical.clone(),
        checkout: canonical.clone(),
    };
    Ok(changed
        .split('\0')
        .filter(|name| !name.is_empty())
        .filter(|name| roots.protected_root(&canonical.join(name)).is_some())
        .map(str::to_string)
        .collect())
}

fn has_commit(checkout: &Path, sha: &Sha) -> bool {
    host_read(checkout, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

fn is_ancestor(checkout: &Path, ancestor: &Sha, of: &str) -> bool {
    host_read(checkout, &["merge-base", "--is-ancestor", ancestor.as_str(), of]).is_ok()
}

/// A Git command on the host that runs nothing the checkout configures (hooks, fsmonitor), nor the user's pager.
fn host_git(checkout: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_PAGER", "cat")
        .output()
        .map_err(|e| format!("can't run git: {e}"))?;
    match out.status.success() {
        true => Ok(out.stdout),
        false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

fn host_read(checkout: &Path, args: &[&str]) -> Result<String, String> {
    host_git(checkout, args).map(|out| String::from_utf8_lossy(&out).trim_end_matches('\n').to_string())
}

fn host_bytes(checkout: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    host_git(checkout, args)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Runs Git without a sandbox (sandboxes don't nest in a sandboxed test run), recording each command; a hook
    /// installed in the source runs, so tests see what it wrote.
    #[derive(Default)]
    pub struct Recorder {
        pub calls: std::sync::Mutex<Vec<String>>,
    }

    impl Recorder {
        pub fn ran(&self, what: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|args| args.starts_with(what))
                .count()
        }
    }

    impl GitRunner for Recorder {
        fn sandboxed(&self, checkout: &Path, args: &[&str]) -> Result<String, String> {
            self.calls.lock().unwrap().push(args.join(" "));
            let out = Command::new("git").arg("-C").arg(checkout).args(args).output().unwrap();
            match out.status.success() {
                true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
                false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
            }
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A source and a second clone that pushes upstream commits.
    fn repos() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let origin = root.join("origin.git");
        git(&root, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        let clone = |name: &str| {
            let path = root.join(name);
            git(
                &root,
                &["clone", "-q", origin.to_str().unwrap(), path.to_str().unwrap()],
            );
            git(&path, &["config", "user.email", "t@t"]);
            git(&path, &["config", "user.name", "t"]);
            path
        };
        let source = clone("source");
        std::fs::write(source.join("README"), "1").unwrap();
        std::fs::create_dir_all(source.join(".pi/old")).unwrap();
        std::fs::write(source.join(".pi/old/gone.ts"), "old").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "one"]);
        git(&source, &["push", "-q", "origin", "HEAD:main"]);
        let upstream = clone("upstream");
        (dir, source, upstream)
    }

    /// Commits `changes` upstream (`Some(content)` writes, `None` deletes) and pushes.
    fn push(upstream: &Path, changes: &[(&str, Option<&str>)]) -> Sha {
        git(upstream, &["pull", "-q", "origin", "main"]);
        for (file, content) in changes {
            let path = upstream.join(file);
            match content {
                Some(content) => {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, content).unwrap();
                }
                None => std::fs::remove_file(&path).unwrap(),
            }
        }
        git(upstream, &["add", "-A"]);
        git(upstream, &["commit", "-q", "-m", "change"]);
        git(upstream, &["push", "-q", "origin", "HEAD:main"]);
        Sha::try_from(git(upstream, &["rev-parse", "HEAD"])).unwrap()
    }

    fn head(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn an_ordinary_update_runs_in_the_sandbox() {
        let (_dir, source, upstream) = repos();
        let target = push(&upstream, &[("src/lib.rs", Some("x"))]);
        let runner = Recorder::default();
        let plan = plan(&runner, &source, "main", Requested::Ask, PermissionMode::Ask).unwrap();
        assert_eq!(
            plan,
            Plan::Sandboxed {
                target: target.clone(),
                approved: vec![]
            }
        );
        apply(&runner, &source, &plan).unwrap();
        assert_eq!(head(&source), target.as_str());
        assert_eq!(
            (runner.ran("fetch"), runner.ran("ls-remote"), runner.ran("merge")),
            (1, 1, 1)
        );
        assert_eq!(
            super::plan(&runner, &source, "main", Requested::Ask, PermissionMode::Ask).unwrap(),
            Plan::Current
        );
    }

    /// The approved protected files are written by the daemon from the commit (added, changed, deleted, executable),
    /// and the merge, in the sandbox, finds them done.
    #[test]
    fn an_approved_update_applies_the_approved_files_then_merges() {
        let (_dir, source, upstream) = repos();
        let before = head(&source);
        let target = push(
            &upstream,
            &[
                (".piton/config.toml", Some("[workers]\n")),
                (".pi/old/gone.ts", None),
                ("src/lib.rs", Some("x")),
            ],
        );
        let runner = Recorder::default();
        let blocked = match plan(&runner, &source, "main", Requested::Ask, PermissionMode::Ask) {
            Err(UpdateError::Blocked(blocked)) => blocked,
            other => panic!("expected Blocked, got {other:?}"),
        };
        assert_eq!(blocked.target, target);
        assert_eq!(blocked.files, [".pi/old/gone.ts", ".piton/config.toml"]);
        assert_eq!(blocked.upstream, "origin/main");
        assert_eq!(head(&source), before, "nothing changed");

        let plan = plan(
            &runner,
            &source,
            "main",
            Requested::Approve(&target),
            PermissionMode::Ask,
        )
        .unwrap();
        apply(&runner, &source, &plan).unwrap();
        assert_eq!(head(&source), target.as_str());
        assert_eq!(
            std::fs::read_to_string(source.join(".piton/config.toml")).unwrap(),
            "[workers]\n"
        );
        assert!(
            !source.join(".pi/old").exists(),
            "the deleted file and its emptied folder"
        );
        assert_eq!(std::fs::read_to_string(source.join("src/lib.rs")).unwrap(), "x");
        assert!(git(&source, &["status", "--porcelain"]).is_empty());
        assert_eq!(runner.ran("merge"), 1);
    }

    /// The executable bit travels with an approved file.
    #[test]
    fn approved_files_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, source, upstream) = repos();
        git(&upstream, &["pull", "-q", "origin", "main"]);
        std::fs::create_dir_all(upstream.join(".pi/bin")).unwrap();
        std::fs::write(upstream.join(".pi/bin/run"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(upstream.join(".pi/bin/run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", "exe"]);
        git(&upstream, &["push", "-q", "origin", "HEAD:main"]);
        let target = Sha::try_from(git(&upstream, &["rev-parse", "HEAD"])).unwrap();
        let runner = Recorder::default();
        let plan = plan(
            &runner,
            &source,
            "main",
            Requested::Approve(&target),
            PermissionMode::Ask,
        )
        .unwrap();
        apply(&runner, &source, &plan).unwrap();
        let mode = std::fs::metadata(source.join(".pi/bin/run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(git(&source, &["status", "--porcelain"]).is_empty());
    }

    /// A stale approval asks again; skipping leaves the source as it is.
    #[test]
    fn an_approval_covers_only_that_commit() {
        let (_dir, source, upstream) = repos();
        let target = push(&upstream, &[(".piton/config.toml", Some(""))]);
        let newer = push(&upstream, &[(".pi/settings.json", Some("{}"))]);
        let runner = Recorder::default();
        assert!(matches!(
            plan(&runner, &source, "main", Requested::Approve(&target), PermissionMode::Ask),
            Err(UpdateError::Blocked(b)) if b.target == newer && b.files == [".pi/settings.json", ".piton/config.toml"]
        ));
        assert_eq!(
            plan(&runner, &source, "main", Requested::Skip, PermissionMode::Ask).unwrap(),
            Plan::Skipped
        );
    }

    /// A symlink among the approved paths refuses before anything is written; a merge that fails puts the approved
    /// files back.
    #[test]
    fn what_cant_be_applied_leaves_the_source_as_it_was() {
        let (_dir, source, upstream) = repos();
        git(&upstream, &["pull", "-q", "origin", "main"]);
        std::fs::create_dir_all(upstream.join(".pi")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", upstream.join(".pi/link")).unwrap();
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", "link"]);
        git(&upstream, &["push", "-q", "origin", "HEAD:main"]);
        let target = Sha::try_from(git(&upstream, &["rev-parse", "HEAD"])).unwrap();
        let runner = Recorder::default();
        let planned = plan(
            &runner,
            &source,
            "main",
            Requested::Approve(&target),
            PermissionMode::Ask,
        )
        .unwrap();
        assert!(matches!(
            apply(&runner, &source, &planned),
            Err(UpdateError::Apply { .. })
        ));
        assert!(git(&source, &["status", "--porcelain"]).is_empty());
        assert_eq!(runner.ran("merge"), 0);

        // A merge that fails (an untracked file in the way): the approved file goes back to HEAD.
        let (_dir, source, upstream) = repos();
        let target = push(
            &upstream,
            &[(".piton/config.toml", Some("new")), ("blocker", Some("theirs"))],
        );
        std::fs::write(source.join("blocker"), "mine").unwrap();
        let planned = plan(
            &runner,
            &source,
            "main",
            Requested::Approve(&target),
            PermissionMode::Ask,
        )
        .unwrap();
        assert!(matches!(
            apply(&runner, &source, &planned),
            Err(UpdateError::Merge { .. })
        ));
        assert!(!source.join(".piton/config.toml").exists());
        assert_eq!(git(&source, &["status", "--porcelain"]), "?? blocker");
    }

    /// A remote-tracking ref an agent rewrote doesn't decide what the source moves to.
    #[test]
    fn the_target_is_what_the_remote_has() {
        let (_dir, source, upstream) = repos();
        let real = push(&upstream, &[("src/lib.rs", Some("x"))]);
        std::fs::write(source.join(".piton-forged"), "x").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "forged"]);
        let forged = head(&source);
        git(&source, &["reset", "-q", "--hard", "HEAD~1"]);
        git(&source, &["update-ref", "refs/remotes/origin/main", &forged]);
        let runner = Recorder::default();
        assert_eq!(
            plan(&runner, &source, "main", Requested::Ask, PermissionMode::Ask).unwrap(),
            Plan::Sandboxed {
                target: real,
                approved: vec![]
            }
        );
    }

    #[test]
    fn a_missing_branch_or_remote_is_typed() {
        let (_dir, source, _upstream) = repos();
        let runner = Recorder::default();
        assert!(matches!(
            plan(&runner, &source, "nope", Requested::Ask, PermissionMode::Ask),
            Err(UpdateError::Fetch { .. } | UpdateError::NoBranch { .. })
        ));
        git(&source, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
        let failed = plan(&runner, &source, "main", Requested::Ask, PermissionMode::Ask).unwrap_err();
        assert!(matches!(failed, UpdateError::Fetch { .. }));
        assert!(failed.to_string().contains("~/.ssh/known_hosts"), "{failed}");
        assert!(
            failed
                .to_string()
                .contains("`\"$SSH_AUTH_SOCK\" = \"allow\"` in [sandbox.network.unix_socket_connections]"),
            "names the socket entry: {failed}"
        );
    }

    #[test]
    fn shas_are_validated() {
        assert!(Sha::try_from("abc".to_string()).is_err());
        assert!(Sha::try_from("g".repeat(40)).is_err());
        assert_eq!(Sha::try_from("A".repeat(40)).unwrap().as_str(), "a".repeat(40));
    }
}
