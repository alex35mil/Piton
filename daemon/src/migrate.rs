//! `piton migrate`: moves a source checkout from earlier layouts to the current one: every setting in
//! `.piton/config.toml` (personal ones in `config.local.toml`), MCP servers in `.piton/mcp[.local].json`, policies and
//! workflow artifacts in `.piton/`.
//!
//! The whole migration is planned first: every move, conversion, ignore-rule change, and worker relink, with each
//! conflict and each key the current file doesn't model refused by name. Only a complete plan is applied, so a refusal
//! leaves the repository untouched. Committed files move with `git mv` (or `git rm` and `git add` when converted), so the
//! change is staged for the user to commit; local files are renamed.
//!
//! The global tier is the user's dotfiles: [`global`] plans its conversion for `piton migrate --global`, which prints it
//! and writes nothing.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::settings::convert::{self, ConvertError, SeparateFile};
use crate::settings::edit::Placement;
use crate::settings::{DaemonFile, LOCAL_DIR, LevelKind, PITON_DIR, Paths, Sharing};

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
    #[error("the converted configuration isn't valid: {reason}")]
    Invalid { reason: String },
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
    /// Writes `to` with `text` (staged in Git when `stage`) and removes every file it was converted from.
    Convert {
        from: Vec<(PathBuf, Tracking)>,
        to: PathBuf,
        text: String,
        stage: bool,
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
            Step::Convert { from, to, .. } if from.is_empty() => write!(f, "write {}", to.display()),
            Step::Convert { from, to, .. } => {
                let from: Vec<_> = from.iter().map(|(path, _)| path.display().to_string()).collect();
                write!(f, "write {} and remove {}", to.display(), from.join(", "))
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
    /// Keys that no longer exist, for the user.
    pub notes: Vec<String>,
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

    let mut gathered = Gathered::default();

    // .project/ -> .piton/: its settings into config.toml, the rest moved.
    let project = root.join(".project");
    if project.is_dir() {
        let entries = read_dir(&project)?;
        for entry in entries {
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let tracked = tracking(&root, &entry)?;
            match name.as_str() {
                "config.json" => gathered.file(SeparateFile::Workflows, false, &entry, tracked)?,
                "config.local.json" => gathered.file(SeparateFile::Workflows, true, &entry, tracked)?,
                "integrations.json" => gathered.file(SeparateFile::Integrations, false, &entry, tracked)?,
                "policies.md" | ".local" => {
                    let to = piton.join(&name);
                    claim(&to)?;
                    steps.push(Step::Move {
                        from: entry,
                        to,
                        tracking: tracked,
                    });
                }
                _ => return Err(MigrateError::Unknown { path: entry }),
            }
        }
    }

    // .agents/: MCP servers move to .piton/; the rest goes into config.toml.
    let agents = root.join(".agents");
    for name in ["mcp.json", "mcp.local.json"] {
        let from = agents.join(name);
        if from.is_file() {
            let to = piton.join(name);
            claim(&to)?;
            let tracking = tracking(&root, &from)?;
            steps.push(Step::Move { from, to, tracking });
        }
    }
    for name in [
        "permissions.json",
        "permissions.local.json",
        "permissions.host.json",
        "permissions.sandbox.json",
        "permissions.host.local.json",
        "permissions.sandbox.local.json",
        "web-search.json",
        "web-search.local.json",
    ] {
        let from = agents.join(name);
        if from.is_file()
            && let Some((kind, local)) = SeparateFile::from_name(name)
        {
            let tracked = tracking(&root, &from)?;
            gathered.file(kind, local, &from, tracked)?;
        }
    }

    // nono overlays into [sandbox], .piton.toml into [workers].
    for (path, local) in [
        (root.join(".nono.json"), false),
        (piton.join("nono.json"), false),
        (root.join(".nono.local.json"), true),
        (piton.join("nono.local.json"), true),
    ] {
        if path.is_file() {
            let sandbox = convert::nono_profile(&read(&path)?, &path)?;
            let table = daemon_table(DaemonFile {
                sandbox,
                ..DaemonFile::default()
            });
            let tracked = tracking(&root, &path)?;
            gathered.add(table, local, &path, tracked);
        }
    }
    let workers_file = root.join(".piton.toml");
    if workers_file.is_file() {
        let workers = convert::project_toml(&read(&workers_file)?, &workers_file)?;
        let table = daemon_table(DaemonFile {
            workers,
            ..DaemonFile::default()
        });
        let tracked = tracking(&root, &workers_file)?;
        gathered.add(table, false, &workers_file, tracked);
    }

    // The separate files of .piton/ before config.toml.
    if piton.is_dir() {
        for path in read_dir(&piton)? {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some((kind, local)) = SeparateFile::from_name(&name) {
                let tracked = tracking(&root, &path)?;
                gathered.file(kind, local, &path, tracked)?;
            }
        }
    }

    let notes = std::mem::take(&mut gathered.notes);
    for step in gathered.steps(&piton, LevelKind::Repository)? {
        match &step {
            Step::Convert { to, .. } => claim(to)?,
            Step::Move { .. } | Step::Ignore { .. } | Step::Relink { .. } | Step::RemoveDir { .. } => {}
        }
        steps.push(step);
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
    Ok(Plan { root, steps, notes })
}

/// Settings from earlier files, gathered into one level's `config.toml` and `config.local.toml`.
#[derive(Default)]
struct Gathered {
    shared: toml::Table,
    local: toml::Table,
    from: Vec<(PathBuf, Tracking)>,
    notes: Vec<String>,
}

impl Gathered {
    fn file(&mut self, kind: SeparateFile, local: bool, path: &Path, tracking: Tracking) -> Result<(), MigrateError> {
        let tables = convert::separate_file(kind, &read(path)?, path, local)?;
        convert::merge_tables(&mut self.shared, tables.shared);
        convert::merge_tables(&mut self.local, tables.local);
        self.notes.extend(tables.notes);
        self.from.push((path.to_path_buf(), tracking));
        Ok(())
    }

    fn add(&mut self, table: toml::Table, local: bool, path: &Path, tracking: Tracking) {
        let into = if local { &mut self.local } else { &mut self.shared };
        convert::merge_tables(into, table);
        self.from.push((path.to_path_buf(), tracking));
    }

    /// The files to write in `dir`, each checked as its readers will read it. The sources are removed with the last.
    fn steps(self, dir: &Path, level: LevelKind) -> Result<Vec<Step>, MigrateError> {
        let Gathered {
            shared, local, from, ..
        } = self;
        let mut steps = Vec::new();
        for (table, sharing, stage) in [
            (shared, Sharing::Shared, true),
            // A local file never goes into Git.
            (local, Sharing::Local, false),
        ] {
            if table.is_empty() {
                continue;
            }
            let to = dir.join(sharing.file_name());
            let text = render(table, &to)?;
            let placement = match sharing {
                Sharing::Shared => Placement::Shared,
                Sharing::Local => Placement::Local,
            };
            crate::preferences::validate_text(&text, &to, level, placement)
                .map_err(|e| MigrateError::Invalid { reason: e.to_string() })?;
            steps.push(Step::Convert {
                from: Vec::new(),
                to,
                text,
                stage,
            });
        }
        // The sources go with the first file written, so committed ones are replaced by the staged config.toml.
        if let Some(Step::Convert { from: first, .. }) = steps.first_mut() {
            *first = from;
        }
        Ok(steps)
    }
}

fn daemon_table(file: DaemonFile) -> toml::Table {
    toml::Table::try_from(file).unwrap_or_default()
}

/// The order tables appear in a converted file: how the daemon runs, then what agents may do, then workflows.
const ORDER: &[&str] = &[
    "socket",
    "log",
    "pi",
    "env",
    "stop",
    "tailnet",
    "host",
    "client",
    "development",
    "sandbox",
    "workers",
    "permissions",
    "web_search",
    "branches",
    "tracker",
    "forge",
];

/// A converted file as a person would write it: tables in [`ORDER`], grant and rule lists one item per line, and small
/// tables of values inline (`repository = { owner = "o", repo = "r" }`).
fn render(mut table: toml::Table, path: &Path) -> Result<String, MigrateError> {
    let mut ordered = toml::Table::new();
    for key in ORDER {
        if let Some(value) = table.remove(*key) {
            ordered.insert((*key).to_string(), value);
        }
    }
    ordered.extend(table);
    let text = toml::to_string(&ordered).map_err(|e| MigrateError::Invalid {
        reason: format!("{}: {e}", path.display()),
    })?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| MigrateError::Invalid {
        reason: format!("{}: {e}", path.display()),
    })?;
    for (key, item) in doc.as_table_mut().iter_mut() {
        // Grant and rule lists one per line; their sub-tables (`[permissions.sandbox]`) stay tables.
        let lists = matches!(key.get(), "sandbox" | "permissions");
        if let toml_edit::Item::Table(table) = item {
            tidy(table, lists, !lists);
        }
    }
    Ok(doc.to_string())
}

/// Lists one item per line (`multiline`), and child tables of plain values inline when they fit on a line (`inline`).
/// Siblings take one form: when any of a table's child tables of plain values doesn't fit inline, none is inlined (a
/// label catalog reads as a list of `[...values.name]` tables, not a mix).
fn tidy(table: &mut toml_edit::Table, multiline: bool, inline: bool) {
    const WIDTH: usize = 100;
    let plain = |child: &toml_edit::Table| child.iter().all(|(_, item)| matches!(item, toml_edit::Item::Value(_)));
    let inline = inline
        && table
            .iter()
            .filter_map(|(_, item)| item.as_table())
            .filter(|child| plain(child))
            .all(|child| child.clone().into_inline_table().to_string().len() <= WIDTH);
    for (mut key, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Value(toml_edit::Value::Array(array)) if multiline && !array.is_empty() => {
                for value in array.iter_mut() {
                    value.decor_mut().set_prefix("\n    ");
                    value.decor_mut().set_suffix("");
                }
                array.set_trailing("\n");
                array.set_trailing_comma(false);
            }
            toml_edit::Item::Table(child) => {
                let plain = inline && child.iter().all(|(_, item)| matches!(item, toml_edit::Item::Value(_)));
                let candidate = plain.then(|| child.clone().into_inline_table());
                match candidate {
                    Some(mut table) if table.to_string().len() <= WIDTH => {
                        table.fmt();
                        *item = toml_edit::Item::Value(toml_edit::Value::InlineTable(table));
                        key.leaf_decor_mut().set_suffix(" ");
                    }
                    Some(_) | None => tidy(child, multiline, inline),
                }
            }
            toml_edit::Item::None | toml_edit::Item::Value(_) | toml_edit::Item::ArrayOfTables(_) => {}
        }
    }
}

/// The global tier's conversion. The global folder is usually the user's dotfiles (its files links into them), so this
/// is printed for the user to apply there; nothing is written.
#[derive(Debug, Default)]
pub struct GlobalPlan {
    /// The new files and their content.
    pub files: Vec<(PathBuf, String)>,
    /// Files whose content is in the new files: to remove.
    pub remove: Vec<PathBuf>,
    /// Files and folders to move as they are.
    pub moves: Vec<(PathBuf, PathBuf)>,
    pub notes: Vec<String>,
}

pub fn global(paths: &Paths) -> Result<GlobalPlan, MigrateError> {
    let mut gathered = Gathered::default();
    let mut plan = GlobalPlan::default();
    if paths.global.is_dir() {
        for path in read_dir(&paths.global)? {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some((kind, local)) = SeparateFile::from_name(&name) {
                gathered.file(kind, local, &path, Tracking::Untracked)?;
            }
        }
    }
    // Files piton kept in pi's agent folder.
    let pi_dir = &paths.pi_agent_dir;
    for name in [
        "permissions.json",
        "permissions.host.json",
        "permissions.sandbox.json",
        "web-search.json",
        "workflows.json",
    ] {
        let path = pi_dir.join(name);
        if path.is_file()
            && let Some((kind, local)) = SeparateFile::from_name(name)
        {
            gathered.file(kind, local, &path, Tracking::Untracked)?;
        }
    }
    for (from, to) in [
        (pi_dir.join("mcp.json"), paths.global.join("mcp.json")),
        (pi_dir.join("rules"), paths.global.join("agents/rules")),
    ] {
        if from.exists() {
            if to.symlink_metadata().is_ok() {
                return Err(MigrateError::Conflict { path: to });
            }
            plan.moves.push((from, to));
        }
    }
    plan.notes = std::mem::take(&mut gathered.notes);
    for step in gathered.steps(&paths.global, LevelKind::Global)? {
        match step {
            Step::Convert { from, to, text, .. } => {
                if to.symlink_metadata().is_ok() {
                    return Err(MigrateError::Conflict { path: to });
                }
                plan.remove.extend(from.into_iter().map(|(path, _)| path));
                plan.files.push((to, text));
            }
            Step::Move { .. } | Step::Ignore { .. } | Step::Relink { .. } | Step::RemoveDir { .. } => {}
        }
    }
    Ok(plan)
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
            Step::Convert { from, to, text, stage } => {
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
                if tracked_any && *stage {
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
        write(
            &root.join(".project/integrations.json"),
            r#"{"tracker":{"provider":"github","mcpServer":"github","repository":{"owner":"o","repo":"r"}},"forge":{"provider":"github"}}"#,
        );
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
        assert!(piton.join(".local/gigs/1-x/notes.md").is_file());
        assert!(piton.join("policies.md").is_file() && piton.join("mcp.local.json").is_file());
        assert!(root.join(".agents/rules/r.md").is_file(), "rules stay in .agents/");
        for old in [
            ".nono.json",
            ".nono.local.json",
            ".piton.toml",
            ".agents/permissions.json",
            ".agents/mcp.local.json",
        ] {
            assert!(!root.join(old).exists(), "{old}");
        }

        // Every setting in one file, written as a person would; personal ones in the local file.
        let text = std::fs::read_to_string(piton.join("config.toml")).unwrap();
        assert_eq!(
            text,
            r#"[sandbox]
allow = [
    "$HOME/cache"
]
read_file = [
    "$HOME/f"
]

[workers]
skip = ["apps/.build"]

[permissions]
allow = [
    "read"
]

[branches]
format = "identifier-title"

[tracker]
provider = "github"
mcp_server = "github"
repository = { owner = "o", repo = "r" }

[forge]
provider = "github"
"#
        );
        assert_eq!(
            std::fs::read_to_string(piton.join("config.local.toml")).unwrap(),
            "[sandbox]\nread = [\n    \"$HOME/n\"\n]\n\n[branches]\nusername = \"a\"\n"
        );

        // Committed files replaced in Git; local ones stay out of it.
        let staged = git(&root, &["diff", "--cached", "--name-status"]).unwrap_or_default();
        assert!(staged.contains(".piton/config.toml"), "{staged}");
        assert!(staged.contains(".piton/policies.md"), "{staged}");
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

    /// The layout with a file per setting in `.piton/` (and a global tier of them) becomes `config.toml`.
    #[test]
    fn migrates_separate_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "T"]);
        let piton = root.join(".piton");
        write(
            &piton.join("daemon.toml"),
            "# grants\n[sandbox]\nallow = [\"$HOME/a\"]\n",
        );
        write(
            &piton.join("permissions.json"),
            r#"{"defaultMode":"ask","deny":["read(*.env)"]}"#,
        );
        write(&piton.join("permissions.sandbox.json"), r#"{"allow":["bash(npm *)"]}"#);
        write(&piton.join("workflows.local.json"), r#"{"branches":{"username":"me"}}"#);
        write(&piton.join("mcp.json"), "{\"mcpServers\":{}}\n");
        run(
            &root,
            &[
                "add",
                ".piton/daemon.toml",
                ".piton/permissions.json",
                ".piton/permissions.sandbox.json",
                ".piton/mcp.json",
            ],
        );
        run(&root, &["commit", "-q", "-m", "separate files"]);

        apply(&plan(&root, &[]).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(piton.join("config.toml")).unwrap(),
            "[sandbox]\nallow = [\n    \"$HOME/a\"\n]\n\n[permissions]\ndefault_mode = \"ask\"\ndeny = [\n    \"read(*.env)\"\n]\n\n[permissions.sandbox]\nallow = [\n    \"bash(npm *)\"\n]\n"
        );
        assert_eq!(
            std::fs::read_to_string(piton.join("config.local.toml")).unwrap(),
            "[branches]\nusername = \"me\"\n"
        );
        for old in [
            "daemon.toml",
            "permissions.json",
            "permissions.sandbox.json",
            "workflows.local.json",
        ] {
            assert!(!piton.join(old).exists(), "{old}");
        }
        assert!(piton.join("mcp.json").is_file());
        let tracked = git(&root, &["ls-files", ".piton"]).unwrap_or_default();
        assert_eq!(tracked, ".piton/config.toml\n.piton/mcp.json");
        // The result reads cleanly.
        let workspace = crate::project::resolve(&root).unwrap();
        let paths = Paths {
            home: root.parent().unwrap().to_path_buf(),
            global: root.parent().unwrap().join("global"),
            pi_agent_dir: root.parent().unwrap().join("agent"),
        };
        let resolved = crate::settings::resolve(&workspace, &paths, crate::settings::Runtime::Sandbox).unwrap();
        assert!(resolved.errors().is_empty(), "{:?}", resolved.errors());
        assert!(plan(&root, &[]).unwrap().steps.is_empty());
    }

    #[test]
    fn refuses_before_changing_anything() {
        let (_dir, root) = old_repository();
        write(&root.join(".piton/config.toml"), "\n");
        assert!(matches!(
            plan(&root, &[]),
            Err(MigrateError::Conflict { path }) if path == root.join(".piton/config.toml")
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
        write(&paths.global.join("daemon.toml"), "[sandbox]\nbase = \"mise-dev\"\n");
        write(&paths.global.join("settings.toml"), "[host]\nroots = [\"~/p\"]\n");
        write(
            &paths.global.join("settings.local.toml"),
            "[client]\ntext_font = \"X\"\n",
        );
        write(
            &paths.global.join("workflows.json"),
            r#"{"workflowProject":"~/piton","sessionAnalysis":true,"branches":{"username":"me"}}"#,
        );
        write(&paths.pi_agent_dir.join("permissions.json"), r#"{"ask":["write"]}"#);
        std::fs::create_dir_all(paths.pi_agent_dir.join("rules")).unwrap();
        let plan = global(&paths).unwrap();
        let file = |name: &str| {
            plan.files
                .iter()
                .find(|(path, _)| path.ends_with(name))
                .map(|(_, text)| text.as_str())
                .unwrap_or_else(|| panic!("{name}: {:?}", plan.files))
        };
        assert_eq!(
            file("config.toml"),
            "[host]\nroots = [\"~/p\"]\n\n[development]\nsession_analysis = true\n\n[sandbox]\nbase = \"mise-dev\"\n\n[permissions]\nask = [\n    \"write\"\n]\n"
        );
        assert_eq!(
            file("config.local.toml"),
            "[client]\ntext_font = \"X\"\n\n[branches]\nusername = \"me\"\n"
        );
        assert_eq!(plan.remove.len(), 5);
        assert_eq!(
            plan.moves,
            [(paths.pi_agent_dir.join("rules"), paths.global.join("agents/rules"))]
        );
        assert!(plan.notes[0].contains("workflowProject"), "{:?}", plan.notes);
    }
}
