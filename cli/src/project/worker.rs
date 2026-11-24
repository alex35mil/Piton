//! Workers: disposable APFS copy-on-write clones of a source checkout, created next to it as `<parent>/<repo>--<name>`,
//! where agents work in isolation. Everything is cloned except `.piton/.local`, which is linked to the source's, and
//! the `[workers] skip` paths of the source's configuration. See "Workers" in docs/design.md.

use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

use crate::{
    launch::Env,
    project::{
        self, ProjectError, Relation, SHARED_PATH, WORKER_SEPARATOR,
        source::{self, Blocked, CreateUpdate, DeleteUpdate, GitRunner, Plan, UpdateError},
    },
};

/// How piton runs mise for workers: the resolved program, its leading arguments, and the environment it runs with (the
/// daemon's base environment, so mise sees the user's `MISE_*` settings).
#[derive(Debug, Clone)]
pub struct Mise {
    pub program: String,
    pub args: Vec<String>,
    pub env: Env,
}

impl Mise {
    fn run(&self, args: &[&std::ffi::OsStr]) -> std::result::Result<String, String> {
        let out = Command::new(&self.program)
            .args(&self.args)
            .args(args)
            .env_clear()
            .envs(&self.env)
            .output()
            .map_err(|e| format!("can't run mise: {e}"))?;
        match out.status.success() {
            true => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        }
    }

    /// Whether mise trusts the config files in `dir` itself. `false` when `dir` has none, or they're untrusted or
    /// ignored. Parses `mise trust --show`, which lists `<dir>: <status>` for each config folder from `dir` upward.
    fn trusts(&self, dir: &Path) -> std::result::Result<bool, String> {
        let out = self.run(&["trust".as_ref(), "--show".as_ref(), "-C".as_ref(), dir.as_os_str()])?;
        Ok(out.lines().any(|line| {
            line.rsplit_once(": ").is_some_and(|(path, status)| {
                status == "trusted" && crate::config::expand_home(path, &crate::config::home()) == dir
            })
        }))
    }

    fn trust(&self, dir: &Path) -> std::result::Result<(), String> {
        self.run(&["trust".as_ref(), "--quiet".as_ref(), dir.as_os_str()])
            .map(drop)
    }

    fn untrust(&self, dir: &Path) -> std::result::Result<(), String> {
        self.run(&[
            "trust".as_ref(),
            "--quiet".as_ref(),
            "--untrust".as_ref(),
            dir.as_os_str(),
        ])
        .map(drop)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub path: PathBuf,
    pub source: PathBuf,
    pub label: String,
    pub issue: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRequest {
    /// The source checkout, or any worker of it.
    pub from: PathBuf,
    /// Folder suffix; normalized to lowercase words joined by `-`.
    pub name: String,
    /// Shown name; defaults to `name`.
    #[serde(default)]
    pub label: Option<String>,
    /// Tracker item the worker is for.
    #[serde(default)]
    pub issue: Option<String>,
    /// How the source is fast-forwarded when the update changes protected files.
    #[serde(default)]
    pub update: CreateUpdate,
}

/// Why a worker can't be created or deleted. Variants carry what a caller needs to explain it or decide to force.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerRefusal {
    #[error("{path} is not a git checkout")]
    NotARepository { path: PathBuf },
    #[error("invalid worker name {name:?}")]
    InvalidName { name: String },
    #[error("{path} already exists")]
    Exists { path: PathBuf },
    #[error("{checkout} has uncommitted changes")]
    SourceDirty { checkout: PathBuf, status: Vec<String> },
    #[error("{checkout} is on {}, not its default branch {default}", branch.as_deref().unwrap_or("a detached HEAD"))]
    SourceOffDefault {
        checkout: PathBuf,
        branch: Option<String>,
        default: String,
    },
    #[error("can't tell the default branch of {checkout} (run `git remote set-head origin --auto`)")]
    UnknownDefault { checkout: PathBuf },
    #[error("{path} is not a worker")]
    NotAWorker { path: PathBuf },
    #[error("{path} has uncommitted changes or unpushed commits")]
    Leftovers { path: PathBuf, lines: Vec<String> },
    /// The source's configuration is invalid: fix it, then retry.
    #[error("{message}")]
    InvalidConfiguration { message: String },
    /// The source can't be fetched inside the sandbox: its message names the setting.
    #[error("{message}")]
    SourceFetch { checkout: PathBuf, message: String },
    /// Fast-forwarding the source changes protected files: retry with the user's answer. Nothing has changed.
    #[error(transparent)]
    ProtectedUpdate(Blocked),
    /// Fast-forwarding the source changes protected files, and the user's `piton(git-pull)` deny rule refuses it.
    #[error(
        "updating {checkout} changes protected files ({}), and a `piton(git-pull)` deny rule in [tools.permissions] \
         refuses it; update the source by hand",
        files.join(", ")
    )]
    UpdateDenied { checkout: PathBuf, files: Vec<String> },
    /// A tool workers need (mise, for trust) is enabled but not installed.
    #[error(transparent)]
    ToolUnavailable(crate::tools::Unavailable),
    #[error("{message}")]
    Failed { message: String },
}

impl From<ProjectError> for WorkerRefusal {
    fn from(e: ProjectError) -> Self {
        Self::Failed { message: e.to_string() }
    }
}

impl From<UpdateError> for WorkerRefusal {
    fn from(e: UpdateError) -> Self {
        match e {
            UpdateError::Blocked(blocked) => Self::ProtectedUpdate(*blocked),
            UpdateError::Denied { checkout, files } => Self::UpdateDenied { checkout, files },
            UpdateError::Fetch { ref checkout, .. } | UpdateError::NoBranch { ref checkout, .. } => Self::SourceFetch {
                checkout: checkout.clone(),
                message: e.to_string(),
            },
            UpdateError::Apply { .. } | UpdateError::Merge { .. } => Self::Failed { message: e.to_string() },
        }
    }
}

type Result<T> = std::result::Result<T, WorkerRefusal>;

/// What the source checkout's permission rules say about an update that changes protected files. The update runs on
/// the host, so the host's rules apply: `[tools.permissions]` and `[tools.permissions.host]`.
fn pull_rule(source: &Path, paths: &crate::config::Paths) -> Result<source::PullRule> {
    let workspace = project::Workspace {
        path: source.to_path_buf(),
        checkout: source.to_path_buf(),
        relation: Relation::Source,
    };
    let resolved = crate::config::resolve(&workspace, paths, crate::config::Runtime::Host)
        .map_err(|e| WorkerRefusal::InvalidConfiguration { message: e.to_string() })?;
    let tools = resolved
        .tools
        .map_err(|e| WorkerRefusal::InvalidConfiguration { message: e.to_string() })?;
    Ok(source::PullRule::of(&tools.permissions.merged))
}

fn failed(message: impl Into<String>) -> WorkerRefusal {
    WorkerRefusal::Failed {
        message: message.into(),
    }
}

/// The source checkout for `from`: itself, or the source of the worker it is.
pub fn source_of(from: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(from).map_err(|_| WorkerRefusal::NotARepository { path: from.into() })?;
    let root = git_ok(&path, &["rev-parse", "--show-toplevel"])
        .map_err(|_| WorkerRefusal::NotARepository { path: path.clone() })?;
    let root = PathBuf::from(root);
    match project::resolve(&root)?.relation {
        Relation::Source => Ok(root),
        Relation::Worker { source, .. } => Ok(source),
    }
}

pub fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if (c.is_whitespace() || c == '_' || c == '-') && !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Creates the worker, after fast-forwarding the source through `git` (see [`source`]). With `mise`, a worker of a
/// mise-trusted source is trusted too: its config files are the source's, cloned, so cloning never grants trust the
/// user hasn't given. `paths` locates the configuration chain the skip list and the update setting come from.
pub fn create(
    request: &CreateRequest,
    mise: Option<&Mise>,
    paths: &crate::config::Paths,
    git: &dyn GitRunner,
) -> Result<WorkerInfo> {
    let source = source_of(&request.from)?;
    let normalized = normalize_name(&request.name);
    if normalized.is_empty() {
        return Err(WorkerRefusal::InvalidName {
            name: request.name.clone(),
        });
    }
    let file_name = format!(
        "{}{WORKER_SEPARATOR}{normalized}",
        source.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    );
    let path = source
        .parent()
        .map(|p| p.join(&file_name))
        .ok_or_else(|| failed("source has no parent"))?;
    if path.symlink_metadata().is_ok() {
        return Err(WorkerRefusal::Exists { path });
    }
    let workspace = project::Workspace {
        path: source.clone(),
        checkout: source.clone(),
        relation: Relation::Source,
    };
    let workers = crate::config::resolve_launch(&workspace, paths)
        .map_err(|e| WorkerRefusal::InvalidConfiguration { message: e.to_string() })?
        .workers;
    let skip_list = workers.skip;

    let branch = ready_branch(&source)?;
    let rule = pull_rule(&source, paths)?;
    let plan = source::plan(git, &source, &branch, (&request.update).into(), rule)?;
    source::apply(git, &source, &plan)?;

    // Created outside the cleanup below, so a failure never removes a directory this call didn't create.
    mkdir_like(&source, &path)?;
    let label = request.label.clone().unwrap_or_else(|| request.name.clone());
    let result = (|| {
        let mut skip: Vec<Vec<String>> = skip_list.iter().map(|s| segments(s)).collect();
        skip.push(segments(SHARED_PATH));
        clone_tree(&source, &path, &skip)?;
        link_shared(&source, &path)?;
        for (key, value) in [
            ("agent.source", Some(source.to_string_lossy().into_owned())),
            ("agent.label", Some(label.clone())),
            ("agent.issue", request.issue.clone()),
        ] {
            if let Some(value) = value {
                git_ok(&path, &["config", "--local", key, &value]).map_err(failed)?;
            }
        }
        if let Some(mise) = mise
            && mise.trusts(&source).map_err(failed)?
        {
            mise.trust(&path)
                .map_err(|e| failed(format!("can't trust the mise config of {}: {e}", path.display())))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&path);
        return Err(e);
    }
    crate::log!("worker created: {} (from {})", path.display(), source.display());
    Ok(WorkerInfo {
        path,
        source,
        label,
        issue: request.issue.clone(),
    })
}

/// The source's default branch when it's clean and on it.
fn ready_branch(source: &Path) -> Result<String> {
    let status = git_ok(source, &["status", "--porcelain"]).map_err(failed)?;
    if !status.is_empty() {
        return Err(WorkerRefusal::SourceDirty {
            checkout: source.into(),
            status: status.lines().take(15).map(str::to_string).collect(),
        });
    }
    let default = git_ok(
        source,
        &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
    )
    .map_err(|_| WorkerRefusal::UnknownDefault {
        checkout: source.into(),
    })?;
    let default = default.trim_start_matches("origin/").to_string();
    let branch = git_ok(source, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    if branch.as_deref() != Some(default.as_str()) {
        return Err(WorkerRefusal::SourceOffDefault {
            checkout: source.into(),
            branch,
            default,
        });
    }
    Ok(default)
}

fn segments(path: &str) -> Vec<String> {
    path.split('/').map(str::to_string).collect()
}

/// Clones `src` into the existing empty directory `dst`, entry by entry with clonefile(2) (which clones a whole tree in
/// one call), except the `skip` paths: each directory on the way to a skipped path is created and walked instead.
fn clone_tree(src: &Path, dst: &Path, skip: &[Vec<String>]) -> Result<()> {
    let entries = std::fs::read_dir(src).map_err(|e| failed(format!("can't read {}: {e}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| failed(format!("can't read {}: {e}", src.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        // Paths that start with this entry: either the entry itself (skip it) or something inside it (walk it).
        let below: Vec<Vec<String>> = skip
            .iter()
            .filter(|s| s.first() == Some(&name))
            .map(|s| s[1..].to_vec())
            .collect();
        if below.is_empty() {
            clonefile(&from, &to)?;
        } else if below.iter().any(Vec::is_empty) {
            // Skipped.
        } else if entry
            .file_type()
            .map(|t| t.is_dir() && !t.is_symlink())
            .unwrap_or(false)
        {
            mkdir_like(&from, &to)?;
            clone_tree(&from, &to, &below)?;
        } else {
            // Not a real directory (e.g. a symlink): nothing to skip inside it.
            clonefile(&from, &to)?;
        }
    }
    Ok(())
}

fn mkdir_like(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mode = std::fs::metadata(src)
        .map_err(|e| failed(format!("{}: {e}", src.display())))?
        .permissions()
        .mode();
    std::fs::DirBuilder::new()
        .mode(mode & 0o7777)
        .create(dst)
        .map_err(|e| failed(format!("can't create {}: {e}", dst.display())))
}

#[allow(unsafe_code)]
fn clonefile(src: &Path, dst: &Path) -> Result<()> {
    let c = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(|_| failed(format!("bad path {}", p.display())));
    let (s, d) = (c(src)?, c(dst)?);
    const CLONE_NOFOLLOW: u32 = 0x0001;
    // SAFETY: `s` and `d` are valid NUL-terminated strings that outlive the call; clonefile(2) only reads them.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) };
    if rc != 0 {
        return Err(failed(format!(
            "can't clone {}: {}",
            src.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Links the worker's `.piton/.local` to the source's, when the source has one.
fn link_shared(source: &Path, worker: &Path) -> Result<()> {
    let shared = source.join(SHARED_PATH);
    if !shared.is_dir() {
        return Ok(());
    }
    let target = worker.join(SHARED_PATH);
    // Never touch anything reached through a symlinked ancestor.
    let parent = target.parent().ok_or_else(|| failed("bad shared path"))?;
    let real_parent = std::fs::canonicalize(parent).map_err(|e| failed(format!("{}: {e}", parent.display())))?;
    let real_worker = std::fs::canonicalize(worker).map_err(|e| failed(format!("{}: {e}", worker.display())))?;
    if !real_parent.starts_with(&real_worker) {
        return Err(failed(format!(
            "can't link {SHARED_PATH}: its parent isn't a directory inside the worker"
        )));
    }
    std::os::unix::fs::symlink(&shared, &target).map_err(|e| failed(format!("can't link {SHARED_PATH}: {e}")))
}

pub fn list(from: &Path) -> Result<Vec<WorkerInfo>> {
    let source = source_of(from)?;
    let parent = source.parent().ok_or_else(|| failed("source has no parent"))?;
    let prefix = format!(
        "{}{WORKER_SEPARATOR}",
        source.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    );
    let mut workers = Vec::new();
    let entries = std::fs::read_dir(parent).map_err(|e| failed(format!("can't read {}: {e}", parent.display())))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Ok(info) = info(&entry.path())
            && info.source == source
        {
            workers.push(info);
        }
    }
    workers.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(workers)
}

/// The worker at `path`.
pub fn info(path: &Path) -> Result<WorkerInfo> {
    let path = std::fs::canonicalize(path).map_err(|_| WorkerRefusal::NotAWorker { path: path.into() })?;
    let workspace = project::resolve(&path)?;
    // The worker itself, not a folder inside one: deleting `<worker>/src` mustn't delete the worker.
    let (Relation::Worker { source, .. }, true) = (workspace.relation, workspace.checkout == path) else {
        return Err(WorkerRefusal::NotAWorker { path });
    };
    let get = |key: &str| git_ok(&path, &["config", "--local", "--get", key]).ok();
    Ok(WorkerInfo {
        label: get("agent.label").unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        issue: get("agent.issue"),
        source,
        path,
    })
}

/// Uncommitted changes and unpushed commits, as lines to show, including an incoming-PR review's unpublished work.
/// A pristine review (see [`PrReview`]) isn't uncommitted work: its staged contribution and held configuration are the
/// tool's.
pub fn leftovers(path: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    let review = pr_review(path);
    let pristine = match &review {
        Some(PrReview::Pristine { .. }) => true,
        Some(PrReview::Unpublished { number, phase }) => {
            // The tools keep HEAD detached, so these commits or rebase progress are on no branch.
            lines.push(format!(
                "Incoming PR #{number}: unpublished {phase} work; publish, finish, or restore it through /pr first"
            ));
            false
        }
        Some(PrReview::Changed { .. }) | None => false,
    };
    match git_ok(path, &["status", "--porcelain"]) {
        Err(e) => lines.push(format!("git status failed: {e}")),
        Ok(s) if !s.is_empty() && !pristine => {
            lines.push("Uncommitted changes:".into());
            lines.extend(s.lines().take(15).map(str::to_string));
        }
        Ok(_) => {}
    }
    match git_ok(path, &["log", "--branches", "--not", "--remotes", "--oneline"]) {
        Err(e) => lines.push(format!("git log failed: {e}")),
        Ok(s) if !s.is_empty() => {
            lines.push("Unpushed commits:".into());
            lines.extend(s.lines().take(15).map(str::to_string));
        }
        Ok(_) => {}
    }
    lines
}

/// A worker deletion checked and decided before anything changes: [`remove`] carries it out.
#[derive(Debug, Clone)]
pub struct Removal {
    pub worker: WorkerInfo,
    force: bool,
    source: AfterRemoval,
}

/// What happens to the source once the worker is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AfterRemoval {
    Update(Plan),
    /// It can't be updated (not clean, off its default branch, the fetch failed): it may stay behind.
    Stale {
        warning: String,
    },
}

/// Checks a deletion and plans the source's update, changing nothing but the fetched objects: refuses with
/// leftovers (unless `force`) or with an update that waits for the user (unless `update` answers it). When sandboxed
/// Git isn't available, the worker is still deleted and the source left as it is.
pub fn plan_removal(
    worker: WorkerInfo,
    force: bool,
    update: &DeleteUpdate,
    git: source::SourceGit<'_>,
    paths: &crate::config::Paths,
) -> Result<Removal> {
    if !project::is_worker_layout(&worker.path, &worker.source) || !worker.path.join(".git").is_dir() {
        return Err(WorkerRefusal::NotAWorker {
            path: worker.path.clone(),
        });
    }
    if !force {
        refuse_leftovers(&worker.path)?;
    }
    let stale = |why: String| AfterRemoval::Stale {
        warning: format!("source not updated, it may be behind: {why}"),
    };
    let source = match (ready_branch(&worker.source), git) {
        (Err(e), _) => stale(e.to_string()),
        (Ok(_), source::SourceGit::Unavailable(why)) => stale(why),
        (Ok(branch), source::SourceGit::Ready(git)) => {
            let rule = pull_rule(&worker.source, paths)?;
            match source::plan(git, &worker.source, &branch, update.into(), rule) {
                Ok(plan) => AfterRemoval::Update(plan),
                Err(UpdateError::Blocked(blocked)) => return Err(WorkerRefusal::ProtectedUpdate(*blocked)),
                // The user's rule: the worker goes, the source stays as it is.
                Err(e @ UpdateError::Denied { .. }) => stale(e.to_string()),
                Err(
                    e @ (UpdateError::Fetch { .. }
                    | UpdateError::NoBranch { .. }
                    | UpdateError::Apply { .. }
                    | UpdateError::Merge { .. }),
                ) => stale(e.to_string()),
            }
        }
    };
    Ok(Removal { worker, force, source })
}

fn refuse_leftovers(path: &Path) -> Result<()> {
    let lines = leftovers(path);
    match lines.is_empty() {
        true => Ok(()),
        false => Err(WorkerRefusal::Leftovers {
            path: path.to_path_buf(),
            lines,
        }),
    }
}

/// Deletes the worker's files (its sessions must already be stopped): unlinks the shared folder as a link, removes
/// the folder without following symlinks, then updates the source as planned. Returns a warning when the source
/// wasn't updated. With `mise`, also drops mise's trust in the folder.
pub fn remove(removal: &Removal, mise: Option<&Mise>, git: source::SourceGit<'_>) -> Result<Option<String>> {
    let worker = &removal.worker;
    // The sessions just stopped may have left work behind.
    if !removal.force {
        refuse_leftovers(&worker.path)?;
    }
    let link = worker.path.join(SHARED_PATH);
    if link
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        std::fs::remove_file(&link).map_err(|e| failed(format!("can't remove {}: {e}", link.display())))?;
    }
    // Before deleting: mise resolves the path, so it must still exist. A leftover trust entry is harmless, so a
    // failure is only logged.
    if let Some(mise) = mise
        && let Err(e) = mise.untrust(&worker.path)
    {
        crate::log!("warning: can't drop mise trust for {}: {e}", worker.path.display());
    }
    // remove_dir_all doesn't follow symlinks.
    std::fs::remove_dir_all(&worker.path)
        .map_err(|e| failed(format!("can't delete {}: {e}", worker.path.display())))?;
    if !worker.source.join(SHARED_PATH).exists() && link.parent().is_some() {
        crate::log!(
            "warning: {} is missing after deleting {}",
            SHARED_PATH,
            worker.path.display()
        );
    }
    crate::log!("worker deleted: {}", worker.path.display());
    Ok(match (&removal.source, git) {
        (AfterRemoval::Stale { warning }, _) => Some(warning.clone()),
        // A plan exists only when Git was there to make it.
        (AfterRemoval::Update(_), source::SourceGit::Unavailable(why)) => {
            Some(format!("source not updated, it may be behind: {why}"))
        }
        (AfterRemoval::Update(plan), source::SourceGit::Ready(git)) => {
            match (plan, source::apply(git, &worker.source, plan)) {
                (_, Err(e)) => Some(format!("source not updated, it may be behind: {e}")),
                (Plan::Skipped, Ok(())) => Some(format!("{} wasn't updated, as asked", worker.source.display())),
                (Plan::Current | Plan::Sandboxed { .. }, Ok(())) => None,
            }
        }
    })
}

/// An incoming-PR review the pi package's `/pr` tools recorded for this checkout. The record is
/// `.piton/.local/pull-requests/<number>/checkout.json`; the fields read here are a contract with
/// `pi/workflows/extensions/pr/state.ts` (see "Workers" in docs/design.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrReview {
    /// Review or fixes, with HEAD, index, and files on disk exactly as the tool left them and nothing untracked.
    Pristine { number: u64 },
    /// Review or fixes that the maintainer changed since; Git status reports the changes.
    Changed { number: u64 },
    /// A rebase in progress or unpublished, or a fix commit not yet published: commits or progress on no branch.
    Unpublished { number: u64, phase: &'static str },
}

/// The review recorded for this checkout, if any. Records for other checkouts, finished (`prepared`, `restored`)
/// reviews, and unreadable records don't count.
pub fn pr_review(checkout: &Path) -> Option<PrReview> {
    let records = checkout.join(SHARED_PATH).join("pull-requests");
    let canonical = std::fs::canonicalize(checkout).ok()?;
    std::fs::read_dir(&records).ok()?.flatten().find_map(|entry| {
        let number: u64 = entry.file_name().to_str()?.parse().ok()?;
        let record: ReviewRecord =
            serde_json::from_slice(&std::fs::read(entry.path().join("checkout.json")).ok()?).ok()?;
        if std::fs::canonicalize(&record.location.root).ok()? != canonical {
            return None;
        }
        match record.phase {
            ReviewPhase::Prepared | ReviewPhase::Restored => None,
            ReviewPhase::ContributorReview | ReviewPhase::Fixes => {
                let state = record.workspace?;
                Some(match matches_review(&canonical, &state) {
                    true => PrReview::Pristine { number },
                    false => PrReview::Changed { number },
                })
            }
            ReviewPhase::Rebasing => Some(PrReview::Unpublished {
                number,
                phase: "rebase",
            }),
            ReviewPhase::Rebased => Some(PrReview::Unpublished {
                number,
                phase: "rebase result",
            }),
            ReviewPhase::CommittingFixes | ReviewPhase::CommittedFixes => Some(PrReview::Unpublished {
                number,
                phase: "fix commit",
            }),
        }
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum ReviewPhase {
    Prepared,
    ContributorReview,
    Fixes,
    Rebasing,
    Rebased,
    CommittingFixes,
    CommittedFixes,
    Restored,
}

#[derive(Deserialize)]
struct ReviewRecord {
    phase: ReviewPhase,
    location: ReviewLocation,
    workspace: Option<ReviewWorkspace>,
}

#[derive(Deserialize)]
struct ReviewLocation {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewWorkspace {
    head_sha: String,
    index_tree: String,
    disk_commit: String,
}

fn matches_review(checkout: &Path, state: &ReviewWorkspace) -> bool {
    let sha = |s: &str| s.len() >= 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    if ![&state.head_sha, &state.index_tree, &state.disk_commit]
        .iter()
        .all(|s| sha(s))
    {
        return false;
    }
    let head = git_ok(checkout, &["rev-parse", "--verify", "HEAD"]);
    let detached = git_ok(checkout, &["symbolic-ref", "--quiet", "HEAD"]).is_err();
    let index = git_ok(checkout, &["write-tree"]);
    if head.as_deref() != Ok(state.head_sha.as_str()) || !detached || index.as_deref() != Ok(state.index_tree.as_str())
    {
        return false;
    }
    // The files on disk against the recorded disk commit, through a throwaway index so the real one is untouched.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let scratch = std::env::temp_dir().join(format!("piton-pr-review-{}-{nanos}.index", std::process::id()));
    let with_index = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
            .args(args)
            .env("GIT_INDEX_FILE", &scratch)
            .output()
            .ok()
    };
    let ok = |out: Option<std::process::Output>| out.filter(|o| o.status.success());
    let clean = ok(with_index(&["read-tree", &state.disk_commit])).is_some()
        && ok(with_index(&["update-index", "-q", "--refresh"])).is_some()
        && ok(with_index(&["diff-files", "--quiet", "--ignore-submodules=all"])).is_some()
        && ok(with_index(&["ls-files", "--others", "--exclude-standard", "-z"])).is_some_and(|o| o.stdout.is_empty());
    let _ = std::fs::remove_file(&scratch);
    clean
}

/// A Git command that runs nothing the checkout configures: the daemon reads checkouts agents work in.
fn git_ok(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_PAGER", "cat")
        .output()
        .map_err(|e| format!("can't run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        });
        return Err(err.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::source::tests::Recorder;

    /// `create`, with Git run on the host (sandboxes don't nest in a sandboxed test run).
    fn create_with_host(
        request: &CreateRequest,
        mise: Option<&Mise>,
        paths: &crate::config::Paths,
    ) -> Result<WorkerInfo> {
        create(request, mise, paths, &Recorder::default())
    }

    /// Plans and carries out a deletion with Git on the host, the way the daemon does.
    fn remove_now(worker: &WorkerInfo, force: bool, mise: Option<&Mise>) -> Result<Option<String>> {
        remove_with(worker, force, &DeleteUpdate::Ask, mise)
    }

    fn remove_with(
        worker: &WorkerInfo,
        force: bool,
        update: &DeleteUpdate,
        mise: Option<&Mise>,
    ) -> Result<Option<String>> {
        let git = Recorder::default();
        let p = paths(worker.source.parent().unwrap());
        let removal = plan_removal(worker.clone(), force, update, source::SourceGit::Ready(&git), &p)?;
        remove(&removal, mise, source::SourceGit::Ready(&git))
    }

    #[test]
    fn names() {
        assert_eq!(normalize_name("Fix the Login bug"), "fix-the-login-bug");
        assert_eq!(normalize_name("  KID-5: settings_window!  "), "kid-5-settings-window");
        assert_eq!(normalize_name("---"), "");
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn paths(root: &Path) -> crate::config::Paths {
        crate::config::Paths {
            home: root.to_path_buf(),
            global: root.join(".config/piton"),
        }
    }

    /// A source checkout with an origin, `.piton/.local`, a skipped folder, and a nested one kept.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let origin = root.join("origin.git");
        let source = root.join("repo");
        std::fs::create_dir_all(&source).unwrap();
        git(&root, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.email", "t@t"]);
        git(&source, &["config", "user.name", "t"]);
        std::fs::write(source.join("README"), "hi").unwrap();
        std::fs::write(source.join(".gitignore"), "/apps/.build\n/.piton/.local\n").unwrap();
        std::fs::create_dir_all(source.join(".piton")).unwrap();
        std::fs::write(
            source.join(".piton/config.toml"),
            "[workers]\nskip = [\"apps/.build/index-build\"]\n",
        )
        .unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-q", "-m", "init"]);
        git(&source, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&source, &["push", "-q", "-u", "origin", "main"]);
        git(&source, &["remote", "set-head", "origin", "main"]);
        std::fs::create_dir_all(source.join(".piton/.local/gigs")).unwrap();
        std::fs::create_dir_all(source.join("apps/.build/index-build/x")).unwrap();
        std::fs::create_dir_all(source.join("apps/.build/xcode")).unwrap();
        std::fs::write(source.join("apps/.build/xcode/keep"), "k").unwrap();
        (dir, source)
    }

    #[test]
    fn create_list_delete() {
        let (_dir, source) = fixture();
        let info = create_with_host(
            &CreateRequest {
                from: source.clone(),
                name: "Fix X".into(),
                label: None,
                issue: Some("KID-5".into()),
                update: CreateUpdate::Ask,
            },
            None,
            &paths(source.parent().unwrap()),
        )
        .unwrap();
        let worker = source.parent().unwrap().join("repo--fix-x");
        assert_eq!(info.path, worker);
        assert_eq!(info.label, "Fix X");
        assert!(worker.join("README").is_file());
        assert!(
            worker.join("apps/.build/xcode/keep").is_file(),
            "nested folder next to a skip is kept"
        );
        assert!(!worker.join("apps/.build/index-build").exists(), "skip list honored");
        let link = worker.join(SHARED_PATH);
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), source.join(SHARED_PATH));

        // The worker resolves to its source, and the source's settings apply to it.
        let ws = project::resolve(&worker).unwrap();
        assert_eq!(
            ws.relation,
            Relation::Worker {
                source: source.clone(),
                name: "fix-x".into(),
            }
        );
        // Creating from a worker clones its source.
        let second = create_with_host(
            &CreateRequest {
                from: worker.clone(),
                name: "two".into(),
                label: Some("Second".into()),
                issue: None,
                update: CreateUpdate::Ask,
            },
            None,
            &paths(source.parent().unwrap()),
        )
        .unwrap();
        assert_eq!(second.source, source);

        let listed: Vec<_> = list(&source).unwrap().into_iter().map(|w| w.path).collect();
        assert_eq!(
            listed,
            [
                source.parent().unwrap().join("repo--fix-x"),
                source.parent().unwrap().join("repo--two")
            ]
        );

        // Leftovers refuse a delete unless forced; the source's shared folder survives.
        std::fs::write(worker.join("new-file"), "x").unwrap();
        match remove_now(&info, false, None) {
            Err(WorkerRefusal::Leftovers { lines, .. }) => assert!(lines.iter().any(|l| l.contains("new-file"))),
            other => panic!("expected leftovers, got {other:?}"),
        }
        remove_now(&info, true, None).unwrap();
        assert!(!worker.exists());
        assert!(source.join(SHARED_PATH).join("gigs").is_dir());
    }

    /// An upstream commit to the source's origin, made from a clone of it.
    fn push_upstream(source: &Path, file: &str) -> source::Sha {
        let root = source.parent().unwrap();
        let upstream = root.join("upstream");
        if !upstream.exists() {
            git(
                root,
                &["clone", "-q", root.join("origin.git").to_str().unwrap(), "upstream"],
            );
            git(&upstream, &["config", "user.email", "t@t"]);
            git(&upstream, &["config", "user.name", "t"]);
        }
        git(&upstream, &["pull", "-q", "origin", "main"]);
        let path = upstream.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Valid in any of the files tests change (`config.toml`, `settings.json`).
        std::fs::write(&path, "").unwrap();
        std::fs::write(upstream.join("CHANGES"), file).unwrap();
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", file]);
        git(&upstream, &["push", "-q", "origin", "HEAD:main"]);
        source::Sha::try_from(git_ok(&upstream, &["rev-parse", "HEAD"]).unwrap()).unwrap()
    }

    fn request(source: &Path, name: &str, update: CreateUpdate) -> CreateRequest {
        CreateRequest {
            from: source.to_path_buf(),
            name: name.into(),
            label: None,
            issue: None,
            update,
        }
    }

    /// An update that changes protected files refuses creation before anything changes; with the approval of that
    /// commit, the daemon writes the approved files and the merge still runs in the sandbox.
    #[test]
    fn creation_asks_before_a_protected_update() {
        let (_dir, source) = fixture();
        let p = paths(source.parent().unwrap());
        let before = git_ok(&source, &["rev-parse", "HEAD"]).unwrap();
        let target = push_upstream(&source, ".piton/config.toml");
        let git = Recorder::default();
        let blocked = match create(&request(&source, "one", CreateUpdate::Ask), None, &p, &git) {
            Err(WorkerRefusal::ProtectedUpdate(blocked)) => blocked,
            other => panic!("expected ProtectedUpdate, got {other:?}"),
        };
        assert_eq!(blocked.files, [".piton/config.toml"]);
        assert_eq!(blocked.target, target);
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), before);
        assert!(!source.parent().unwrap().join("repo--one").exists());
        assert_eq!(git.ran("merge"), 0);

        // Typed for clients: the refusal's fields at the top level.
        let json = serde_json::to_value(WorkerRefusal::ProtectedUpdate(blocked)).unwrap();
        assert_eq!(json["kind"], "protected_update");
        assert_eq!(json["files"][0], ".piton/config.toml");
        assert_eq!(json["target"], target.as_str());

        let approved = CreateUpdate::Approve { target: target.clone() };
        create(&request(&source, "one", approved), None, &p, &git).unwrap();
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), target.as_str());
        assert_eq!(git.ran("merge"), 1, "the merge runs through the sandboxed runner");
        assert!(git_ok(&source, &["status", "--porcelain"]).unwrap().is_empty());

        let ordinary = push_upstream(&source, "src/lib.rs");
        let git = Recorder::default();
        create(&request(&source, "two", CreateUpdate::Ask), None, &p, &git).unwrap();
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), ordinary.as_str());
        assert_eq!((git.ran("fetch"), git.ran("merge")), (1, 1));
    }

    /// `piton(git-pull)` in the source's `[tools.permissions]` answers the question before anyone is asked: `allow` applies
    /// the protected files; `deny` refuses creation and leaves the source as it is on deletion; `ask` asks.
    #[test]
    fn the_git_pull_rule_decides_first() {
        let (_dir, source) = fixture();
        let p = paths(source.parent().unwrap());
        // As a project's setup keeps it: local configuration outside Git.
        std::fs::write(source.join(".git/info/exclude"), "/.piton/*.local.*\n").unwrap();
        let rule = |list: &str| {
            let text = format!("[tools.permissions]\n{list} = [\"piton(git-pull)\"]\n");
            std::fs::write(source.join(".piton/config.local.toml"), text).unwrap();
        };
        let one = create_with_host(&request(&source, "one", CreateUpdate::Ask), None, &p).unwrap();

        rule("deny");
        let before = git_ok(&source, &["rev-parse", "HEAD"]).unwrap();
        push_upstream(&source, ".piton/config.toml");
        match create(
            &request(&source, "two", CreateUpdate::Ask),
            None,
            &p,
            &Recorder::default(),
        ) {
            Err(WorkerRefusal::UpdateDenied { files, .. }) => assert_eq!(files, [".piton/config.toml"]),
            other => panic!("expected UpdateDenied, got {other:?}"),
        }
        let json = serde_json::to_value(WorkerRefusal::UpdateDenied {
            checkout: source.clone(),
            files: vec!["x".into()],
        })
        .unwrap();
        assert_eq!(json["kind"], "update_denied");
        let warning = remove_with(&one, false, &DeleteUpdate::Ask, None).unwrap();
        assert!(
            warning.is_some_and(|w| w.contains("piton(git-pull)")),
            "deleted, the source left"
        );
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), before);

        rule("allow");
        let git = Recorder::default();
        create(&request(&source, "three", CreateUpdate::Ask), None, &p, &git).unwrap();
        assert_ne!(
            git_ok(&source, &["rev-parse", "HEAD"]).unwrap(),
            before,
            "applied without asking"
        );

        rule("ask");
        push_upstream(&source, ".pi/settings.json");
        assert!(matches!(
            create(
                &request(&source, "four", CreateUpdate::Ask),
                None,
                &p,
                &Recorder::default()
            ),
            Err(WorkerRefusal::ProtectedUpdate(_))
        ));
    }

    #[test]
    fn git_pull_rules_combine_strictly() {
        let settings = |allow: &[&str], ask: &[&str], deny: &[&str]| crate::config::PermissionSettings {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ask: ask.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let rule = |a: &[&str], b: &[&str], c: &[&str]| source::PullRule::of(&settings(a, b, c));
        const PULL: &str = "piton(git-pull)";
        let pull = PULL;
        assert_eq!(rule(&[], &[], &[]), source::PullRule::Ask);
        assert_eq!(rule(&[pull], &[], &[]), source::PullRule::Allow);
        assert_eq!(rule(&[pull], &[pull], &[]), source::PullRule::Ask);
        assert_eq!(rule(&[pull], &[], &["*"]), source::PullRule::Deny);
        assert_eq!(
            rule(&["bash(*)"], &[], &[]),
            source::PullRule::Ask,
            "other tools' rules"
        );
        assert_eq!(rule(&["*"], &[], &[]), source::PullRule::Allow);
    }

    /// Deletion asks before it deletes anything; the answer updates on the host or leaves the source behind.
    #[test]
    fn deletion_asks_before_deleting() {
        let (_dir, source) = fixture();
        let p = paths(source.parent().unwrap());
        let one = create_with_host(&request(&source, "one", CreateUpdate::Ask), None, &p).unwrap();
        let two = create_with_host(&request(&source, "two", CreateUpdate::Ask), None, &p).unwrap();
        let before = git_ok(&source, &["rev-parse", "HEAD"]).unwrap();
        let target = push_upstream(&source, ".piton/config.toml");

        match remove_now(&one, false, None) {
            Err(WorkerRefusal::ProtectedUpdate(blocked)) => assert_eq!(blocked.target, target),
            other => panic!("expected ProtectedUpdate, got {other:?}"),
        }
        assert!(one.path.exists(), "nothing deleted before the answer");

        let warning = remove_with(&one, false, &DeleteUpdate::Skip, None).unwrap();
        assert!(warning.is_some_and(|w| w.contains("wasn't updated")));
        assert!(!one.path.exists());
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), before);

        let approved = DeleteUpdate::Approve { target: target.clone() };
        assert_eq!(remove_with(&two, false, &approved, None).unwrap(), None);
        assert!(!two.path.exists());
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), target.as_str());
    }

    /// Without the source's sandbox (its configuration or environment tools failed), a worker is still deleted, and
    /// the source left as it is with the reason.
    #[test]
    fn deletion_goes_on_without_sandboxed_git() {
        let (_dir, source) = fixture();
        let p = paths(source.parent().unwrap());
        let one = create_with_host(&request(&source, "one", CreateUpdate::Ask), None, &p).unwrap();
        let before = git_ok(&source, &["rev-parse", "HEAD"]).unwrap();
        push_upstream(&source, "src/lib.rs");
        let unavailable = || source::SourceGit::Unavailable("mise env failed".into());
        let removal = plan_removal(one.clone(), false, &DeleteUpdate::Ask, unavailable(), &p).unwrap();
        let warning = remove(&removal, None, unavailable()).unwrap();
        assert!(warning.is_some_and(|w| w.contains("mise env failed")));
        assert!(!one.path.exists());
        assert_eq!(git_ok(&source, &["rev-parse", "HEAD"]).unwrap(), before);
    }

    #[test]
    fn dirty_source_refuses() {
        let (_dir, source) = fixture();
        std::fs::write(source.join("README"), "changed").unwrap();
        let p = paths(source.parent().unwrap());
        match create_with_host(
            &CreateRequest {
                from: source,
                name: "x".into(),
                label: None,
                issue: None,
                update: CreateUpdate::Ask,
            },
            None,
            &p,
        ) {
            Err(WorkerRefusal::SourceDirty { status, .. }) => assert!(status.iter().any(|l| l.contains("README"))),
            other => panic!("expected SourceDirty, got {other:?}"),
        }
    }

    /// mise with its own trust store, so the test never touches the user's.
    fn isolated_mise(state: &Path) -> Mise {
        let mut env: Env = std::env::vars().filter(|(k, _)| !k.starts_with("MISE_")).collect();
        env.insert("MISE_STATE_DIR".into(), state.to_string_lossy().into_owned());
        Mise {
            program: crate::launch::resolve_program("mise", &env),
            args: Vec::new(),
            env,
        }
    }

    #[test]
    fn a_worker_of_a_trusted_source_is_trusted_until_deleted() {
        let (dir, source) = fixture();
        std::fs::write(source.join("mise.toml"), "[env]\nPITON_TEST = \"1\"\n").unwrap();
        git(&source, &["add", "mise.toml"]);
        git(&source, &["commit", "-q", "-m", "mise"]);
        git(&source, &["push", "-q"]);
        let mise = isolated_mise(&dir.path().join("mise-state"));
        let request = |name: &str| CreateRequest {
            from: source.clone(),
            name: name.into(),
            label: None,
            issue: None,
            update: CreateUpdate::Ask,
        };

        // Untrusted source: the worker isn't trusted either.
        let p = paths(source.parent().unwrap());
        let untrusted = create_with_host(&request("one"), Some(&mise), &p).unwrap();
        assert!(!mise.trusts(&untrusted.path).unwrap());

        mise.trust(&source).unwrap();
        let trusted = create_with_host(&request("two"), Some(&mise), &p).unwrap();
        assert!(mise.trusts(&trusted.path).unwrap());
        assert!(!mise.trusts(&untrusted.path).unwrap(), "only new workers are trusted");

        // Deleting drops the trust: a later folder at the same path starts untrusted.
        remove_now(&trusted, true, Some(&mise)).unwrap();
        std::fs::create_dir_all(&trusted.path).unwrap();
        std::fs::write(trusted.path.join("mise.toml"), "[env]\nX = \"1\"\n").unwrap();
        assert!(!mise.trusts(&trusted.path).unwrap());
        assert!(mise.trusts(&source).unwrap(), "the source keeps its trust");
    }

    #[test]
    fn a_worker_claim_needs_the_layout() {
        let (_dir, source) = fixture();
        let elsewhere = source.parent().unwrap().join("other");
        std::fs::create_dir_all(&elsewhere).unwrap();
        git(&elsewhere, &["init", "-q"]);
        git(
            &elsewhere,
            &["config", "--local", "agent.source", source.to_str().unwrap()],
        );
        assert!(matches!(
            project::resolve(&elsewhere),
            Err(ProjectError::BadWorker { .. })
        ));
    }

    /// A folder inside a worker is part of it: its sandbox settings come from the source, never from the folder, and
    /// it can't be deleted as if it were the worker.
    #[test]
    fn folders_inside_a_worker_belong_to_it() {
        let (_dir, source) = fixture();
        let worker = create_with_host(
            &CreateRequest {
                from: source.clone(),
                name: "sub".into(),
                label: None,
                issue: None,
                update: CreateUpdate::Ask,
            },
            None,
            &paths(source.parent().unwrap()),
        )
        .unwrap();
        let inner = worker.path.join("apps/.build/xcode");
        let ws = project::resolve(&inner).unwrap();
        assert_eq!(ws.checkout, worker.path);
        assert_eq!(ws.source(), source);
        assert!(matches!(ws.relation, Relation::Worker { .. }));
        // A repository nested in the worker (a submodule, a vendored checkout) is still the worker's.
        git(&inner, &["init", "-q"]);
        assert_eq!(project::resolve(&inner).unwrap().source(), source);
        assert!(matches!(info(&inner), Err(WorkerRefusal::NotAWorker { .. })));
        assert_eq!(info(&worker.path).unwrap().path, worker.path);
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        git_ok(dir, args).unwrap()
    }

    /// The review state the pi package's `/pr` tools leave a worker in: HEAD at the merge base, the PR in the index,
    /// and a held configuration file at its base version on disk. Deletion treats it as clean; anything more is a
    /// leftover.
    #[test]
    fn pristine_pr_review_is_not_a_leftover() {
        let (_dir, source) = fixture();
        let info = create_with_host(
            &CreateRequest {
                from: source.clone(),
                name: "pr-12".into(),
                label: None,
                issue: None,
                update: CreateUpdate::Ask,
            },
            None,
            &paths(source.parent().unwrap()),
        )
        .unwrap();
        let w = &info.path;
        std::fs::write(w.join("mise.toml"), "base\n").unwrap();
        git(w, &["add", "mise.toml"]);
        git(w, &["commit", "-q", "-m", "base"]);
        let base = git_out(w, &["rev-parse", "HEAD"]);
        std::fs::write(w.join("README"), "pr").unwrap();
        std::fs::write(w.join("mise.toml"), "pr\n").unwrap();
        git(w, &["commit", "-q", "-am", "pr"]);
        let pr = git_out(w, &["rev-parse", "HEAD"]);
        let pr_tree = git_out(w, &["rev-parse", &format!("{pr}^{{tree}}")]);
        // Files on disk: the PR with mise.toml held at base.
        git(w, &["checkout", "-q", &base, "--", "mise.toml"]);
        let disk_tree = git_out(w, &["write-tree"]);
        let disk = git_out(w, &["commit-tree", &disk_tree, "-m", "disk"]);
        // Review state: HEAD at base, detached; index = PR; disk = PR with held base mise.toml.
        git(w, &["read-tree", &pr]);
        git(w, &["update-ref", "--no-deref", "HEAD", &base]);
        let record = |phase: &str, root: &Path| {
            serde_json::json!({
                "phase": phase,
                "location": { "root": root },
                "workspace": { "headSha": base, "indexTree": pr_tree, "diskCommit": disk },
            })
        };
        let file = source.join(SHARED_PATH).join("pull-requests/12/checkout.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let uncommitted = |lines: &[String]| lines.iter().any(|l| l == "Uncommitted changes:");
        assert!(
            uncommitted(&leftovers(w)),
            "no record: the review state is uncommitted work"
        );

        let unpublished = |lines: &[String]| lines.iter().any(|l| l.starts_with("Incoming PR #12: unpublished"));
        let pristine = Some(PrReview::Pristine { number: 12 });
        let changed = Some(PrReview::Changed { number: 12 });

        std::fs::write(&file, record("contributorReview", w).to_string()).unwrap();
        assert_eq!(pr_review(w), pristine);
        assert!(!uncommitted(&leftovers(w)) && !unpublished(&leftovers(w)));

        // A record for another checkout or a finished review doesn't apply.
        std::fs::write(&file, record("contributorReview", &source).to_string()).unwrap();
        assert_eq!(pr_review(w), None);
        std::fs::write(&file, record("restored", w).to_string()).unwrap();
        assert_eq!(pr_review(w), None);

        // Unpublished commits or rebase progress sit on no branch: they're leftovers even with a clean status.
        git(w, &["reset", "-q", "--hard", &pr]);
        git(w, &["checkout", "-q", "--detach", &pr]);
        assert!(git_out(w, &["status", "--porcelain"]).is_empty());
        for phase in ["rebasing", "rebased", "committingFixes", "committedFixes"] {
            std::fs::write(&file, record(phase, w).to_string()).unwrap();
            assert!(unpublished(&leftovers(w)), "{phase} must be a leftover");
        }

        // Edits, untracked files, and staging changes in review or fixes are leftovers.
        git(w, &["read-tree", &pr]);
        git(w, &["checkout", "-q", &base, "--", "mise.toml"]);
        git(w, &["read-tree", &pr]);
        git(w, &["update-ref", "--no-deref", "HEAD", &base]);
        std::fs::write(&file, record("fixes", w).to_string()).unwrap();
        assert_eq!(pr_review(w), pristine);
        std::fs::write(w.join("README"), "edited").unwrap();
        assert_eq!(pr_review(w), changed);
        assert!(uncommitted(&leftovers(w)));
        std::fs::write(w.join("README"), "pr").unwrap();
        assert_eq!(pr_review(w), pristine);
        std::fs::write(w.join("new.txt"), "x").unwrap();
        assert_eq!(pr_review(w), changed);
        std::fs::remove_file(w.join("new.txt")).unwrap();
        git(w, &["reset", "-q", "--", "README"]);
        assert_eq!(pr_review(w), changed);
        assert!(uncommitted(&leftovers(w)));
    }
}
