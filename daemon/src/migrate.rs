//! `piton migrate`: moves a source checkout from the layout before `.piton/` to the current one.
//!
//! The whole migration is planned first: every move, conversion, ignore-rule change, and worker relink, with each
//! conflict and each key the current files don't model refused by name. Only a complete plan is applied, so a refusal
//! leaves the repository untouched. Committed files move with `git mv` (or `git rm` and `git add` when converted), so the
//! change is staged for the user to commit; local files are renamed.
//!
//! The global tier is the user's dotfiles: [`global`] describes its conversion for `piton config check` and writes
//! nothing.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::settings::convert::{self, ConvertError, Scope};
use crate::settings::{DaemonFile, LOCAL_DIR, PITON_DIR, Paths};

/// The ignore rule for workflow artifacts (no trailing slash, so it also covers a worker's symlink).
const LOCAL_IGNORE: &str = "/.piton/.local";
/// The one exclusion every local settings file falls under.
const LOCAL_SETTINGS_EXCLUDE: &str = "/.piton/*.local.*";
/// Old ignore rules for the artifacts folder.
const OLD_LOCAL_IGNORES: &[&str] = &["/.project/.local", "/.project/.local/"];
/// Old ignore rules for local settings files, all covered by [`LOCAL_SETTINGS_EXCLUDE`] now.
const OLD_LOCAL_SETTINGS: &[&str] = &[
    "/.project/config.local.json",
    "/.agents/mcp.local.json",
    "/.agents/permissions.local.json",
    "/.agents/web-search.local.json",
    "/.agents/*.local.json",
    "/.nono.local.json",
];

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("{path} is not a git checkout")]
    NotARepository { path: PathBuf },
    #[error("{path} is a worker; run `piton migrate` in its source, {checkout}, which relinks every worker")]
    Worker { path: PathBuf, checkout: PathBuf },
    #[error("{path} already exists; move or merge it by hand, then run `piton migrate` again")]
    Conflict { path: PathBuf },
    #[error("{path} isn't part of piton's old layout; move it out of .project/ by hand")]
    Unknown { path: PathBuf },
    #[error(transparent)]
    Convert(#[from] ConvertError),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("git {args}: {message}")]
    Git { args: String, message: String },
    #[error(transparent)]
    Project(#[from] crate::project::ProjectError),
}

/// Whether Git tracks a path: committed files move with Git, so the move is staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracking {
    Tracked,
    Untracked,
}

/// One change of the migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Move {
        from: PathBuf,
        to: PathBuf,
        tracking: Tracking,
    },
    /// Writes `to` with `text` and removes every file it was converted from.
    Convert {
        from: Vec<(PathBuf, Tracking)>,
        to: PathBuf,
        text: String,
    },
    /// Rewrites the ignore file's lines.
    Ignore {
        file: PathBuf,
        text: String,
        tracking: Tracking,
    },
    /// A worker's link to the source's artifacts: removes the old link, adds the new one.
    Relink {
        worker: PathBuf,
        old: Option<PathBuf>,
        link: PathBuf,
        target: PathBuf,
    },
    /// Removes the emptied `.project/`.
    RemoveDir { path: PathBuf },
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Move { from, to, tracking } => {
                let how = match tracking {
                    Tracking::Tracked => "git mv",
                    Tracking::Untracked => "move",
                };
                write!(f, "{how} {} -> {}", from.display(), to.display())
            }
            Step::Convert { from, to, .. } => {
                let from: Vec<_> = from.iter().map(|(path, _)| path.display().to_string()).collect();
                write!(f, "convert {} -> {}", from.join(" + "), to.display())
            }
            Step::Ignore { file, .. } => write!(f, "update {}", file.display()),
            Step::Relink {
                worker, link, target, ..
            } => write!(
                f,
                "relink worker {}: {} -> {}",
                worker.display(),
                link.display(),
                target.display()
            ),
            Step::RemoveDir { path } => write!(f, "remove the empty {}", path.display()),
        }
    }
}

/// A source checkout's complete migration.
#[derive(Debug)]
pub struct Plan {
    pub root: PathBuf,
    pub steps: Vec<Step>,
}

/// Plans the migration of the checkout at `path`, which must be a source. `workers` are its workers' folders.
pub fn plan(path: &Path, workers: &[PathBuf]) -> Result<Plan, MigrateError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let root = git(&canonical, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|_| MigrateError::NotARepository {
            path: canonical.clone(),
        })?;
    match crate::project::resolve(&root)?.relation {
        crate::project::Relation::Source => {}
        crate::project::Relation::Worker { source, .. } => {
            return Err(MigrateError::Worker {
                path: root,
                checkout: source,
            });
        }
    }
    let piton = root.join(PITON_DIR);
    let mut steps = Vec::new();
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut claim = |to: &Path| -> Result<(), MigrateError> {
        if to.symlink_metadata().is_ok() || targets.iter().any(|t| t == to) {
            return Err(MigrateError::Conflict { path: to.to_path_buf() });
        }
        targets.push(to.to_path_buf());
        Ok(())
    };

    // .project/ -> .piton/
    let project = root.join(".project");
    if project.is_dir() {
        let entries = read_dir(&project)?;
        for entry in entries {
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let to = match name.as_str() {
                "config.json" => piton.join("workflows.json"),
                "config.local.json" => piton.join("workflows.local.json"),
                "integrations.json" | "policies.md" | ".local" => piton.join(&name),
                _ => return Err(MigrateError::Unknown { path: entry }),
            };
            claim(&to)?;
            let tracking = tracking(&root, &entry)?;
            steps.push(Step::Move {
                from: entry,
                to,
                tracking,
            });
        }
    }

    // .agents/{permissions,mcp,web-search}*.json -> .piton/
    let agents = root.join(".agents");
    for name in [
        "permissions.json",
        "permissions.local.json",
        "permissions.host.json",
        "permissions.sandbox.json",
        "permissions.host.local.json",
        "permissions.sandbox.local.json",
        "mcp.json",
        "mcp.local.json",
        "web-search.json",
        "web-search.local.json",
    ] {
        let from = agents.join(name);
        if from.is_file() {
            let to = piton.join(name);
            claim(&to)?;
            let tracking = tracking(&root, &from)?;
            steps.push(Step::Move { from, to, tracking });
        }
    }

    // .nono.json, .piton/nono.json, .piton.toml -> .piton/daemon.toml; the .local overlays -> daemon.local.toml
    for (sources, workers_file, to) in [
        (
            [root.join(".nono.json"), piton.join("nono.json")],
            Some(root.join(".piton.toml")),
            piton.join("daemon.toml"),
        ),
        (
            [root.join(".nono.local.json"), piton.join("nono.local.json")],
            None,
            piton.join("daemon.local.toml"),
        ),
    ] {
        let mut daemon = DaemonFile::default();
        let mut from = Vec::new();
        for path in sources.iter().filter(|path| path.is_file()) {
            let table = convert::nono_profile(&read(path)?, path, Scope::Project)?;
            crate::settings::merge_sandbox(&mut daemon.sandbox, table);
            from.push((path.clone(), tracking(&root, path)?));
        }
        if let Some(path) = workers_file.filter(|path| path.is_file()) {
            daemon.workers = convert::project_toml(&read(&path)?, &path)?;
            from.push((path.clone(), tracking(&root, &path)?));
        }
        if !from.is_empty() {
            claim(&to)?;
            let text = daemon_toml(&daemon).map_err(|e| MigrateError::Io {
                path: to.clone(),
                source: std::io::Error::other(e),
            })?;
            steps.push(Step::Convert { from, to, text });
        }
    }

    // Ignore rules.
    let local_settings = steps.iter().any(|step| match step {
        Step::Move { to, .. } | Step::Convert { to, .. } => is_local_settings(to),
        Step::Ignore { .. } | Step::Relink { .. } | Step::RemoveDir { .. } => false,
    }) || existing_local_settings(&piton)?;
    let exclude = PathBuf::from(git(&root, &["rev-parse", "--git-path", "info/exclude"]).map_err(git_error)?);
    let exclude = if exclude.is_absolute() {
        exclude
    } else {
        root.join(exclude)
    };
    let gitignore = root.join(".gitignore");
    let gitignore_text = read_optional(&gitignore)?;
    let exclude_text = read_optional(&exclude)?;
    let (new_gitignore, gitignore_has_exclusion) = rewrite_ignore(&gitignore_text, true);
    let (mut new_exclude, exclude_has_exclusion) = rewrite_ignore(&exclude_text, false);
    if local_settings && !gitignore_has_exclusion && !exclude_has_exclusion {
        if !new_exclude.is_empty() && !new_exclude.ends_with('\n') {
            new_exclude.push('\n');
        }
        new_exclude.push_str(LOCAL_SETTINGS_EXCLUDE);
        new_exclude.push('\n');
    }
    if new_gitignore != gitignore_text {
        let tracking = tracking(&root, &gitignore)?;
        steps.push(Step::Ignore {
            file: gitignore,
            text: new_gitignore,
            tracking,
        });
    }
    if new_exclude != exclude_text {
        steps.push(Step::Ignore {
            file: exclude,
            text: new_exclude,
            tracking: Tracking::Untracked,
        });
    }

    // Workers: link .piton/.local to the source's.
    let artifacts_moved = steps.iter().any(|step| match step {
        Step::Move { to, .. } => *to == root.join(LOCAL_DIR),
        Step::Convert { .. } | Step::Ignore { .. } | Step::Relink { .. } | Step::RemoveDir { .. } => false,
    });
    if artifacts_moved || root.join(LOCAL_DIR).is_dir() {
        for worker in workers {
            let old = worker.join(".project/.local");
            let old = old
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
                .then_some(old);
            let link = worker.join(LOCAL_DIR);
            if link.symlink_metadata().is_ok() {
                if old.is_none() {
                    continue;
                }
                return Err(MigrateError::Conflict { path: link });
            }
            steps.push(Step::Relink {
                worker: worker.clone(),
                old,
                link,
                target: root.join(LOCAL_DIR),
            });
        }
    }

    if project.is_dir() {
        steps.push(Step::RemoveDir { path: project });
    }
    Ok(Plan { root, steps })
}

/// Applies a plan, in order.
pub fn apply(plan: &Plan) -> Result<(), MigrateError> {
    let root = &plan.root;
    for step in &plan.steps {
        match step {
            Step::Move { from, to, tracking } => {
                create_parent(to)?;
                match tracking {
                    Tracking::Tracked => {
                        git(root, &["mv", "--", &s(from), &s(to)]).map_err(git_error)?;
                    }
                    Tracking::Untracked => std::fs::rename(from, to).map_err(|source| io(from, source))?,
                }
            }
            Step::Convert { from, to, text } => {
                create_parent(to)?;
                std::fs::write(to, text).map_err(|source| io(to, source))?;
                let mut tracked_any = false;
                for (path, tracking) in from {
                    match tracking {
                        Tracking::Tracked => {
                            git(root, &["rm", "-q", "--", &s(path)]).map_err(git_error)?;
                            tracked_any = true;
                        }
                        Tracking::Untracked => std::fs::remove_file(path).map_err(|source| io(path, source))?,
                    }
                }
                if tracked_any {
                    git(root, &["add", "--", &s(to)]).map_err(git_error)?;
                }
            }
            Step::Ignore { file, text, tracking } => {
                std::fs::write(file, text).map_err(|source| io(file, source))?;
                match tracking {
                    Tracking::Tracked => {
                        git(root, &["add", "--", &s(file)]).map_err(git_error)?;
                    }
                    Tracking::Untracked => {}
                }
            }
            Step::Relink { old, link, target, .. } => {
                if let Some(old) = old {
                    std::fs::remove_file(old).map_err(|source| io(old, source))?;
                }
                create_parent(link)?;
                std::os::unix::fs::symlink(target, link).map_err(|source| io(link, source))?;
            }
            Step::RemoveDir { path } => match std::fs::remove_dir(path) {
                Ok(()) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(io(path, source)),
            },
        }
    }
    Ok(())
}

/// The global tier's conversion, for the user to apply in their dotfiles: what each old global file becomes. Writes
/// nothing.
pub fn global(paths: &Paths) -> Result<Vec<String>, MigrateError> {
    let mut out = Vec::new();
    let config = paths.global.join("config.toml");
    let mut base = String::from(crate::sandbox::DEFAULT_BASE);
    let mut daemon = None;
    if config.is_file() {
        let convert::OldConfig {
            daemon: file,
            settings,
            keep_awake,
        } = convert::old_config(&read(&config)?, &config)?;
        if let Some(name) = &file.sandbox.base {
            base = name.clone();
        }
        daemon = Some(file);
        out.push(format!(
            "{} -> {} and {}",
            config.display(),
            paths.global.join("daemon.toml").display(),
            paths.global.join("settings.toml").display()
        ));
        out.push(format!(
            "settings.toml:\n{}",
            toml::to_string(&settings).unwrap_or_default()
        ));
        if let Some(on) = keep_awake {
            out.push(format!(
                "keep_awake = {on}: now state, not configuration; set it once in the apps (it then survives restarts)"
            ));
        }
    }
    // The base nono profile, when it's a file of the user's (a built-in name has none): its grants become [sandbox].
    let profile = nono_profiles_dir(paths).join(format!("{base}.json"));
    if profile.is_file() {
        let table = convert::nono_profile(&read(&profile)?, &profile, Scope::Global)?;
        let file = daemon.get_or_insert_with(DaemonFile::default);
        let command = file.sandbox.command.take();
        file.sandbox = table;
        file.sandbox.command = command;
        out.push(format!(
            "{} -> [sandbox] in {}",
            profile.display(),
            paths.global.join("daemon.toml").display()
        ));
    }
    if let Some(file) = daemon {
        out.push(format!("daemon.toml:\n{}", daemon_toml(&file).unwrap_or_default()));
    }
    let pi_dir = &paths.pi_agent_dir;
    for name in [
        "permissions.json",
        "permissions.host.json",
        "permissions.sandbox.json",
        "mcp.json",
        "web-search.json",
        "workflows.json",
    ] {
        let path = pi_dir.join(name);
        if path.exists() {
            out.push(format!("{} -> {}", path.display(), paths.global.join(name).display()));
        }
    }
    let rules = pi_dir.join("rules");
    if rules.is_dir() {
        out.push(format!(
            "{} -> {}",
            rules.display(),
            paths.global.join("agents/rules").display()
        ));
    }
    Ok(out)
}

/// A `daemon.toml` as a person would write it: `[sandbox]` grant lists one path per line, other values inline.
fn daemon_toml(file: &DaemonFile) -> Result<String, toml::ser::Error> {
    let text = toml::to_string(file)?;
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Ok(text);
    };
    if let Some(sandbox) = doc.get_mut("sandbox").and_then(toml_edit::Item::as_table_mut) {
        for (_, item) in sandbox.iter_mut() {
            if let Some(array) = item.as_value_mut().and_then(toml_edit::Value::as_array_mut) {
                for value in array.iter_mut() {
                    value.decor_mut().set_prefix("\n    ");
                    value.decor_mut().set_suffix("");
                }
                array.set_trailing("\n");
                array.set_trailing_comma(false);
            }
        }
    }
    Ok(doc.to_string())
}

/// Where nono keeps the user's profiles.
fn nono_profiles_dir(paths: &Paths) -> PathBuf {
    paths.home.join(".config/nono/profiles")
}

/// The ignore file's text with the old rules replaced, and whether it holds the local settings exclusion afterwards.
/// `artifacts`: the file may hold the artifacts rule (`.gitignore`; the exclude file doesn't need it).
fn rewrite_ignore(text: &str, artifacts: bool) -> (String, bool) {
    let has = |line: &str| text.lines().any(|l| l == line);
    let mut has_exclusion = has(LOCAL_SETTINGS_EXCLUDE);
    let mut has_local = has(LOCAL_IGNORE);
    let mut lines = Vec::new();
    for line in text.lines() {
        if OLD_LOCAL_IGNORES.contains(&line) {
            if artifacts && !has_local {
                lines.push(LOCAL_IGNORE.to_string());
                has_local = true;
            }
        } else if OLD_LOCAL_SETTINGS.contains(&line) {
            if !has_exclusion {
                lines.push(LOCAL_SETTINGS_EXCLUDE.to_string());
                has_exclusion = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if lines.iter().map(String::as_str).eq(text.lines()) {
        return (text.to_string(), has_exclusion);
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, has_exclusion)
}

fn is_local_settings(path: &Path) -> bool {
    let in_piton = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == PITON_DIR);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    in_piton && name.contains(".local.")
}

fn existing_local_settings(piton: &Path) -> Result<bool, MigrateError> {
    if !piton.is_dir() {
        return Ok(false);
    }
    Ok(read_dir(piton)?.iter().any(|path| is_local_settings(path)))
}

fn tracking(root: &Path, path: &Path) -> Result<Tracking, MigrateError> {
    let listed = git(root, &["ls-files", "--", &s(path)]).map_err(git_error)?;
    Ok(if listed.is_empty() {
        Tracking::Untracked
    } else {
        Tracking::Tracked
    })
}

fn read_dir(dir: &Path) -> Result<Vec<PathBuf>, MigrateError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| io(dir, source))?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|source| io(dir, source)))
        .collect::<Result<_, _>>()?;
    entries.sort();
    Ok(entries)
}

fn read(path: &Path) -> Result<String, MigrateError> {
    std::fs::read_to_string(path).map_err(|source| io(path, source))
}

fn read_optional(path: &Path) -> Result<String, MigrateError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(io(path, source)),
    }
}

fn create_parent(path: &Path) -> Result<(), MigrateError> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent).map_err(|source| io(parent, source)),
        None => Ok(()),
    }
}

fn io(path: &Path, source: std::io::Error) -> MigrateError {
    MigrateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

struct GitFailure {
    args: String,
    message: String,
}

fn git_error(failure: GitFailure) -> MigrateError {
    MigrateError::Git {
        args: failure.args,
        message: failure.message,
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, GitFailure> {
    let failure = |message: String| GitFailure {
        args: args.join(" "),
        message,
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| failure(format!("can't run git: {e}")))?;
    if !out.status.success() {
        return Err(failure(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        git(dir, args).unwrap_or_else(|e| panic!("git {}: {}", e.args, e.message));
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A repository in the old layout: committed workflow files, overlays, and settings; local files and artifacts.
    fn old_repository() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "T"]);
        write(
            &root.join(".project/config.json"),
            "{\"branches\":{\"format\":\"identifier-title\"}}\n",
        );
        write(&root.join(".project/integrations.json"), "{}\n");
        write(&root.join(".project/policies.md"), "policy\n");
        write(
            &root.join(".project/config.local.json"),
            "{\"branches\":{\"username\":\"a\"}}\n",
        );
        write(&root.join(".project/.local/gigs/1-x/notes.md"), "notes\n");
        write(&root.join(".agents/permissions.json"), "{\"allow\":[\"read\"]}\n");
        write(&root.join(".agents/mcp.local.json"), "{\"mcpServers\":{}}\n");
        write(&root.join(".agents/rules/r.md"), "rule\n");
        write(
            &root.join(".nono.json"),
            r#"{"filesystem":{"allow":["$HOME/cache"],"read_file":["$HOME/f"]}}"#,
        );
        write(&root.join(".nono.local.json"), r#"{"filesystem":{"read":["$HOME/n"]}}"#);
        write(&root.join(".piton.toml"), "[workers]\nskip = [\"apps/.build\"]\n");
        write(
            &root.join(".gitignore"),
            "node_modules/\n/.nono.local.json\n/.project/.local\n",
        );
        write(
            &root.join(".git/info/exclude"),
            "# comment\n/.project/config.local.json\n/.agents/mcp.local.json\n",
        );
        run(
            &root,
            &[
                "add",
                ".project/config.json",
                ".project/integrations.json",
                ".project/policies.md",
                ".agents",
                ".nono.json",
                ".piton.toml",
                ".gitignore",
            ],
        );
        run(&root, &["reset", "-q", "--", ".agents/mcp.local.json"]);
        run(&root, &["commit", "-q", "-m", "old layout"]);
        (dir, root)
    }

    #[test]
    fn migrates_a_repository_and_its_workers() {
        let (_dir, root) = old_repository();
        let worker = root.with_file_name("repo--w");
        std::fs::create_dir_all(worker.join(".project")).unwrap();
        std::os::unix::fs::symlink(root.join(".project/.local"), worker.join(".project/.local")).unwrap();

        let plan = plan(&root, std::slice::from_ref(&worker)).unwrap();
        apply(&plan).unwrap();

        let piton = root.join(".piton");
        assert!(!root.join(".project").exists());
        assert_eq!(
            std::fs::read_to_string(piton.join("workflows.json")).unwrap(),
            "{\"branches\":{\"format\":\"identifier-title\"}}\n"
        );
        assert!(piton.join("workflows.local.json").is_file());
        assert!(piton.join(".local/gigs/1-x/notes.md").is_file());
        assert!(piton.join("permissions.json").is_file() && piton.join("mcp.local.json").is_file());
        assert!(root.join(".agents/rules/r.md").is_file(), "rules stay in .agents/");
        for old in [".nono.json", ".nono.local.json", ".piton.toml"] {
            assert!(!root.join(old).exists(), "{old}");
        }

        // The converted files resolve to the old grants and skip list, written as a person would.
        let text = std::fs::read_to_string(piton.join("daemon.toml")).unwrap();
        assert_eq!(
            text,
            "[sandbox]\nallow = [\n    \"$HOME/cache\"\n]\nread_file = [\n    \"$HOME/f\"\n]\n\n[workers]\nskip = [\"apps/.build\"]\n"
        );
        let daemon: DaemonFile = toml::from_str(&text).unwrap();
        assert_eq!(daemon.sandbox.allow, ["$HOME/cache"]);
        assert_eq!(daemon.sandbox.read_file, ["$HOME/f"]);
        assert_eq!(daemon.workers.skip, ["apps/.build"]);
        let local: DaemonFile =
            toml::from_str(&std::fs::read_to_string(piton.join("daemon.local.toml")).unwrap()).unwrap();
        assert_eq!(local.sandbox.read, ["$HOME/n"]);

        // Committed files moved in Git; local ones stay out of it.
        let staged = git(&root, &["diff", "--cached", "--name-status"]).unwrap_or_default();
        assert!(staged.contains(".piton/workflows.json"), "{staged}");
        assert!(staged.contains(".piton/daemon.toml"), "{staged}");
        let tracked = git(&root, &["ls-files"]).unwrap_or_default();
        assert!(!tracked.contains("local"), "{tracked}");

        assert_eq!(
            std::fs::read_to_string(root.join(".gitignore")).unwrap(),
            "node_modules/\n/.piton/*.local.*\n/.piton/.local\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".git/info/exclude")).unwrap(),
            "# comment\n/.piton/*.local.*\n"
        );
        let status = git(&root, &["status", "--porcelain", "--untracked-files=all"]).unwrap_or_default();
        assert!(!status.lines().any(|line| line.starts_with("??")), "{status}");

        // The worker links the moved artifacts.
        assert!(!worker.join(".project/.local").exists());
        assert_eq!(
            std::fs::read_link(worker.join(".piton/.local")).unwrap(),
            root.join(".piton/.local")
        );

        // Nothing left to do.
        assert!(super::plan(&root, &[worker]).unwrap().steps.is_empty());
    }

    #[test]
    fn refuses_before_changing_anything() {
        let (_dir, root) = old_repository();
        write(&root.join(".piton/workflows.json"), "{}\n");
        assert!(matches!(
            plan(&root, &[]),
            Err(MigrateError::Conflict { path }) if path == root.join(".piton/workflows.json")
        ));
        std::fs::remove_dir_all(root.join(".piton")).unwrap();

        write(&root.join(".nono.json"), r#"{"filesystem":{"write":["/x"]}}"#);
        assert!(matches!(
            plan(&root, &[]),
            Err(MigrateError::Convert(ConvertError::Unmodeled { key, .. })) if key == "filesystem.write"
        ));
        write(&root.join(".nono.json"), r#"{"extends":"default"}"#);
        assert!(matches!(
            plan(&root, &[]),
            Err(MigrateError::Convert(ConvertError::GlobalOnly { .. }))
        ));
        assert!(root.join(".project/config.json").is_file());
    }

    #[test]
    fn describes_the_global_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let paths = Paths {
            home: home.clone(),
            global: home.join(".config/piton"),
            pi_agent_dir: home.join(".pi/agent"),
        };
        write(
            &paths.global.join("config.toml"),
            "roots = [\"~/p\"]\n[sandbox]\nprofile = \"piton\"\n",
        );
        write(
            &home.join(".config/nono/profiles/piton.json"),
            r#"{"extends":"mise-dev","filesystem":{"allow":["$HOME/a"]},"network":{"network_profile":null}}"#,
        );
        write(&paths.pi_agent_dir.join("permissions.json"), "{}");
        std::fs::create_dir_all(paths.pi_agent_dir.join("rules")).unwrap();
        let lines = global(&paths).unwrap().join("\n");
        assert!(lines.contains("roots = [\"~/p\"]"), "{lines}");
        assert!(lines.contains("base = \"mise-dev\""), "{lines}");
        assert!(lines.contains("allow = [\n    \"$HOME/a\"\n]"), "{lines}");
        assert!(lines.contains("agents/rules"), "{lines}");
        assert!(lines.contains(".config/piton/permissions.json"), "{lines}");
    }
}
