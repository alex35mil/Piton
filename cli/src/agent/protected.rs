//! Protected files: what an agent may not change without the user's approval, because it decides what the agent may do
//! or what runs: every configuration file the user edits by hand.
//!
//! - In the checkout ([`ProtectedRoots`]): the source checkout's `.piton/` (except `.local/`, the workflow artifacts),
//!   and at any depth in the workspace's checkout: `.pi/`, and fnox's config files (which secrets go where, and the
//!   commands fnox runs to get them). `.pi/settings.json.lock` itself isn't protected: pi creates that lock folder to
//!   read the project's settings, and it decides nothing.
//! - Outside it ([`Area`]): the launch's configuration levels, pi's agent directories (except the runtime state pi
//!   writes itself), the files fnox loads, and the real targets of the symlinks in those levels and agent directories
//!   (a global tier linked into a dotfiles repository).
//!
//! Two layers use this one definition: the sandbox, which closes every one of them to every process (the checkout's by
//! the Seatbelt rules here, the rest by `launch/sandbox/resolve.rs` from the same [`link_entries`] and [`fixed_entries`]),
//! and the daemon's check of a write an agent asks for over `agent.sock` ([`Protected::check`], written only after the
//! user approves it). Nothing else is ever protected: not the daemon's state, its control socket, or a credential. See
//! "Protected files" in docs/design.md.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{LOCAL_DIR, PITON_DIR};

/// The variable pi gets its session's [`Protected`] in (JSON), so `piton config protected` answers as the daemon checks.
pub const PROTECTED_ENV: &str = "PITON_PROTECTED";

/// Configuration outside the checkout's protected files. Paths are canonical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Area {
    /// A configuration level of either chain, and everything in it (the source's `.piton/` and the workspace's own
    /// `.agents/` aren't levels here: the first is the checkout's, the second is agent-editable).
    Level { dir: PathBuf },
    /// pi's agent directory, everything in it except its runtime state (`launch/sandbox/policy.rs`, `AGENT_DIR_WRITES`).
    AgentDir { dir: PathBuf },
    /// One file fnox loads for the launch, at its real path.
    FnoxFile { path: PathBuf },
    /// The real target of a symlink in a level or an agent directory, and the link it's reached through.
    LinkTarget {
        link: PathBuf,
        target: PathBuf,
        shape: Shape,
    },
}

/// What a link target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// A folder: everything in it.
    Folder,
    /// Exactly the file.
    File,
}

/// What a protected file controls: what the review says about it, since the diff is the user's only check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controls {
    /// It can loosen the sandbox: the global `config.toml`, a `config.local.toml`.
    Sandbox,
    /// It runs code in every session: pi's extensions, packages, settings, and trust; MCP servers; how the daemon
    /// starts pi.
    Code,
    /// It decides where secrets go: a fnox config.
    Secrets,
    /// Any other configuration: nothing to warn about, so the review shows no label.
    Other,
}

impl Controls {
    /// The note after the path in the review's prompt, if any.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Controls::Sandbox => Some("can loosen the sandbox"),
            Controls::Code => Some("runs code in every session"),
            Controls::Secrets => Some("decides where secrets go"),
            Controls::Other => None,
        }
    }
}

/// A protected file: where a write to it starts from (it never leaves that folder, and follows no symlink below it),
/// and what it controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedFile {
    pub root: PathBuf,
    pub controls: Controls,
}

/// One session's protected files: the checkout's, and the configuration outside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protected {
    /// The checkout's protected files: the canonical source and checkout.
    pub source: PathBuf,
    pub checkout: PathBuf,
    pub areas: Vec<Area>,
    /// The global tier (canonical): its `config.toml` can loosen the sandbox.
    pub global: PathBuf,
    /// What no area outside the checkout ever reaches: the daemon's state folder, the credential denies, and the
    /// user's `"deny"` entries.
    pub excluded: Vec<PathBuf>,
}

/// What a session's protection is built from.
pub struct ProtectedFacts<'a> {
    pub source: &'a Path,
    pub checkout: &'a Path,
    /// The existing configuration levels of both chains (`LaunchConfig::levels`).
    pub levels: &'a [PathBuf],
    /// pi's agent directories, as named: the host's and the launch's.
    pub agent_dirs: &'a [PathBuf],
    /// The files fnox loads for the launch.
    pub fnox_files: &'a [PathBuf],
    pub global: &'a Path,
    /// The user's home folder: an area holding it is never protected.
    pub home: &'a Path,
    /// The daemon's state folder, the credential denies, and the user's `"deny"` entries, expanded.
    pub excluded: &'a [PathBuf],
}

impl Protected {
    pub fn new(facts: &ProtectedFacts) -> Self {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let mut areas: Vec<Area> = Vec::new();
        let add = |area: Area, areas: &mut Vec<Area>| {
            if !areas.contains(&area) {
                areas.push(area);
            }
        };
        for level in facts.levels {
            match level_kind(level, facts.source, facts.checkout) {
                LevelRole::Own | LevelRole::Editable => {}
                LevelRole::Configuration => add(Area::Level { dir: canonical(level) }, &mut areas),
            }
        }
        let excluded: Vec<PathBuf> = facts.excluded.iter().map(|p| canonical_target(p)).collect();
        // An area that holds the home folder, a checkout, or anything excluded, or sits inside something excluded, is
        // left out whole: a link the agent planted in a level it could edit (a parent folder's `.agents/`, from a
        // session there) mustn't make a credential or the workspace proposable.
        let held = [
            canonical(facts.home),
            canonical(facts.source),
            canonical(facts.checkout),
        ];
        let reaches_out = |area: &Path| {
            held.iter().any(|h| under(h, area)) || excluded.iter().any(|e| under(e, area) || under(area, e))
        };
        for dir in facts.agent_dirs {
            let dir = canonical(dir);
            if !reaches_out(&dir) {
                add(Area::AgentDir { dir }, &mut areas);
            }
        }
        for file in facts.fnox_files {
            add(Area::FnoxFile { path: canonical(file) }, &mut areas);
        }
        for (link, target) in link_entries(facts.levels, facts.agent_dirs, facts.source, facts.checkout) {
            if reaches_out(&target) {
                continue;
            }
            let shape = match target.is_dir() {
                true => Shape::Folder,
                false => Shape::File,
            };
            // Reached by its name in its folder's real path, as a request through it resolves.
            let link = canonical(link.parent().unwrap_or(&link)).join(link.file_name().unwrap_or_default());
            add(Area::LinkTarget { link, target, shape }, &mut areas);
        }
        Self {
            source: canonical(facts.source),
            checkout: canonical(facts.checkout),
            areas,
            global: canonical(facts.global),
            excluded,
        }
    }

    /// The protected file at `path` (canonical, absolute: [`canonical_target`] of what was asked for), or `None` when
    /// it isn't one. Names compare without case, as APFS does by default.
    pub fn check(&self, path: &Path) -> Option<ProtectedFile> {
        let roots = ProtectedRoots {
            source: self.source.clone(),
            checkout: self.checkout.clone(),
        };
        if let Some(root) = roots.protected_root(path) {
            return Some(ProtectedFile {
                root: root.to_path_buf(),
                controls: self.controls(path),
            });
        }
        // Outside the checkout's own files, what an area reaches never includes the daemon's state or a credential.
        if self.excluded.iter().any(|e| under(path, e)) {
            return None;
        }
        // The nearest area decides: the global tier holds its `agents/` level, a linked folder can sit in a level.
        let found = self.areas.iter().filter_map(|area| {
            let (root, reached) = match area {
                Area::Level { dir } => (strictly_under(path, dir).then(|| dir.clone())?, path.to_path_buf()),
                Area::AgentDir { dir } => {
                    let relative = relative_to(path, dir)?;
                    if relative.as_os_str().is_empty() || crate::launch::sandbox::agent_dir_runtime(&relative) {
                        return None;
                    }
                    (dir.clone(), path.to_path_buf())
                }
                Area::FnoxFile { path: file } => (
                    folded(path)
                        .eq(&folded(file))
                        .then(|| file.parent().map(Path::to_path_buf))??,
                    path.to_path_buf(),
                ),
                Area::LinkTarget { link, target, shape } => match shape {
                    Shape::Folder => {
                        let relative = relative_to(path, target).filter(|r| !r.as_os_str().is_empty())?;
                        (target.clone(), link.join(relative))
                    }
                    Shape::File => (
                        folded(path)
                            .eq(&folded(target))
                            .then(|| target.parent().map(Path::to_path_buf))??,
                        link.clone(),
                    ),
                },
            };
            Some((root, reached))
        });
        found
            .max_by_key(|(root, _)| root.components().count())
            .map(|(root, reached)| ProtectedFile {
                root,
                controls: self.controls(&reached),
            })
    }

    /// What the file at `path` (as reached: a linked file by its link) controls.
    fn controls(&self, path: &Path) -> Controls {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let fnox_file = self
            .areas
            .iter()
            .any(|a| matches!(a, Area::FnoxFile { path: f } if folded(f) == folded(path)));
        if fnox_file || (under(path, &self.checkout) && is_fnox_config(&name)) {
            return Controls::Secrets;
        }
        let own_piton = self.source.join(PITON_DIR);
        let in_level = |p: &Path| {
            under(p, &own_piton)
                || under(p, &self.global)
                || self
                    .areas
                    .iter()
                    .any(|a| matches!(a, Area::Level { dir } if under(p, dir)))
        };
        if in_level(path) {
            let parent_is_level = path.parent().is_some_and(|parent| {
                folded(parent) == folded(&own_piton)
                    || folded(parent) == folded(&self.global)
                    || self
                        .areas
                        .iter()
                        .any(|a| matches!(a, Area::Level { dir } if folded(dir) == folded(parent)))
            });
            let global_file = |file: &str| folded(path) == folded(&self.global.join(file));
            return match name.as_str() {
                "config.local.toml" if parent_is_level => Controls::Sandbox,
                "config.toml" if global_file("config.toml") => Controls::Sandbox,
                "config.host.toml" | "config.host.local.toml" if global_file(&name) => Controls::Code,
                "mcp.json" | "mcp.local.json" if parent_is_level => Controls::Code,
                _ => Controls::Other,
            };
        }
        for area in &self.areas {
            if let Area::AgentDir { dir } = area
                && let Some(relative) = relative_to(path, dir)
            {
                return agent_dir_controls(&relative);
            }
        }
        // A project's `.pi/`, as pi reads it from the working folder: the part after the last `.pi`.
        if let Some(relative) = relative_to(path, &self.checkout) {
            let parts = lower_parts(&relative);
            if let Some(at) = parts.iter().rposition(|p| p == PI_DIR) {
                return match parts[at + 1..] {
                    [ref first, ..] if first == "extensions" => Controls::Code,
                    [ref only] if only == "settings.json" || only == "mcp.json" => Controls::Code,
                    _ => Controls::Other,
                };
            }
        }
        Controls::Other
    }
}

/// What a file in pi's agent directory controls, from its path relative to the directory.
fn agent_dir_controls(relative: &Path) -> Controls {
    let parts = lower_parts(relative);
    match parts.as_slice() {
        [first, ..] if ["extensions", "npm", "git", "bin"].contains(&first.as_str()) => Controls::Code,
        [only] if ["settings.json", "trust.json", "mcp.json"].contains(&only.as_str()) => Controls::Code,
        _ => Controls::Other,
    }
}

/// How a level folder takes part in protection.
enum LevelRole {
    /// The source's `.piton/`: the checkout's protected files cover it.
    Own,
    /// The workspace's own `.agents/`: agent-editable.
    Editable,
    /// Every other level.
    Configuration,
}

fn level_kind(level: &Path, source: &Path, checkout: &Path) -> LevelRole {
    if level == source.join(PITON_DIR) {
        LevelRole::Own
    } else if level == checkout.join(crate::config::AGENTS_DIR) {
        LevelRole::Editable
    } else {
        LevelRole::Configuration
    }
}

/// The symlinks in the configuration levels and pi's agent directories, each with its canonical target: writing through
/// a link writes its target, so the target is protected (and closed in the sandbox) too. Left out: the workspace's own
/// `.agents/` (agent-editable), the source's `.piton/.local` (the workflow artifacts), and an agent directory's runtime
/// entries. Links are as named.
pub fn link_entries(
    levels: &[PathBuf],
    agent_dirs: &[PathBuf],
    source: &Path,
    checkout: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let levels = levels
        .iter()
        .filter_map(|level| match level_kind(level, source, checkout) {
            LevelRole::Editable => None,
            LevelRole::Own => Some((level, Folder::OwnPiton)),
            LevelRole::Configuration => Some((level, Folder::Level)),
        });
    let agents = agent_dirs.iter().map(|dir| (dir, Folder::AgentDir));
    for (folder, kind) in levels.chain(agents) {
        let Ok(entries) = std::fs::read_dir(folder) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            let name = path.file_name().map(Path::new).unwrap_or(Path::new(""));
            let skipped = match kind {
                Folder::OwnPiton => name.to_string_lossy().eq_ignore_ascii_case(".local"),
                Folder::Level => false,
                Folder::AgentDir => crate::launch::sandbox::agent_dir_runtime(name),
            };
            let is_link = path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
            if !skipped
                && is_link
                && let Ok(target) = std::fs::canonicalize(&path)
                && !out.iter().any(|(_, t)| *t == target)
            {
                out.push((path, target));
            }
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Folder {
    OwnPiton,
    Level,
    AgentDir,
}

/// Every folder entry on the way to each of `paths`, and the path itself, as named and at its real path: denied writes,
/// they keep a protected area where the user approved it (renamed, removed, or replaced, a folder would let what's
/// inside be changed under another name). What's in those folders stays as the other rules decide. Not `/`.
pub fn fixed_entries(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for path in paths {
        let real = canonical_target(path);
        for path in [path.as_path(), real.as_path()] {
            for entry in path.ancestors().filter(|a| a.parent().is_some()) {
                if !entries.iter().any(|e| e == entry) {
                    entries.push(entry.to_path_buf());
                }
            }
        }
    }
    entries
}

fn folded(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// `path` relative to `root`, compared without case, keeping `path`'s own case.
fn relative_to(path: &Path, root: &Path) -> Option<PathBuf> {
    folded(path).strip_prefix(folded(root)).ok()?;
    let depth = root.components().count();
    Some(path.components().skip(depth).collect())
}

fn under(path: &Path, root: &Path) -> bool {
    folded(path).starts_with(folded(root))
}

fn strictly_under(path: &Path, root: &Path) -> bool {
    under(path, root) && folded(path) != folded(root)
}

fn lower_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => None,
        })
        .collect()
}

const PI_DIR: &str = ".pi";
/// The `proper-lockfile` lock pi takes on `.pi/settings.json` to read it (docs/pi-findings.md).
const PI_SETTINGS_LOCK: &str = "settings.json.lock";

/// A fnox config file's name (lowercase), as fnox finds them in a folder and its `.config/`: `fnox.toml`, `.fnox.toml`,
/// `fnox.local.toml`, and the profile forms (`fnox.<profile>.toml`, `.fnox.<profile>.local.toml`). Files a config
/// imports by path are checked at launch instead (`fnox config-files`).
fn is_fnox_config(name: &str) -> bool {
    name.strip_prefix('.')
        .unwrap_or(name)
        .strip_prefix("fnox")
        .and_then(|rest| rest.strip_suffix(".toml"))
        .is_some_and(|middle| middle.is_empty() || middle.strip_prefix('.').is_some_and(|profile| !profile.is_empty()))
}

/// The roots protection is computed for: one launch's source and workspace checkouts (canonical).
#[derive(Debug, Clone)]
pub struct ProtectedRoots {
    pub source: PathBuf,
    pub checkout: PathBuf,
}

impl ProtectedRoots {
    /// The checkout a protected `path` (canonical, absolute) is in, which a write to it stays inside: the source for its
    /// `.piton/` (settings, permissions, sandbox grants), the workspace's checkout for `.pi/` (pi's settings and
    /// extensions, which run code) and fnox's config files. `None` when it isn't protected. Every protected write asks the user. Names compare case-insensitively: APFS is case-insensitive by
    /// default, so `.PI/settings.json` is the file pi reads as `.pi/settings.json`.
    pub fn protected_root(&self, path: &Path) -> Option<&Path> {
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        let path = lower(path);
        let source = lower(&self.source);
        if path.starts_with(source.join(PITON_DIR)) && !path.starts_with(source.join(LOCAL_DIR)) {
            return Some(&self.source);
        }
        let relative = path.strip_prefix(lower(&self.checkout)).ok()?;
        let parts: Vec<&str> = relative
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => name.to_str(),
                Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => None,
            })
            .collect();
        if let [.., PI_DIR, PI_SETTINGS_LOCK] = parts.as_slice() {
            return None;
        }
        let fnox = parts.last().is_some_and(|name| is_fnox_config(name));
        (parts.contains(&PI_DIR) || fnox).then_some(self.checkout.as_path())
    }

    /// The Seatbelt rules for a sandboxed launch: deny every write to the protected paths. Paths are canonical, as
    /// Seatbelt compares them.
    pub fn seatbelt_rules(&self) -> Vec<String> {
        // Regexes throughout, every protected name in any case: Seatbelt compares paths as written, and APFS treats
        // `.PITON` and `.piton` as one folder. `.piton/.local` stays writable in any case too.
        let source = regex_escape(&self.source.to_string_lossy());
        let checkout = regex_escape(&self.checkout.to_string_lossy());
        let piton = any_case(PITON_DIR);
        let local = any_case(".local");
        vec![
            format!(
                "(deny file-write* (require-all (regex #\"^{source}/{piton}(/|$)\") (require-not (regex #\"^{source}/{piton}/{local}(/|$)\"))))"
            ),
            format!(
                "(deny file-write* (regex #\"^{checkout}/(.+/)?{}(/|$)\"))",
                any_case(PI_DIR)
            ),
            // The lock folder alone, not what's in it: pi creates, touches, and removes it.
            format!(
                "(allow file-write* (regex #\"^{checkout}/(.+/)?{}/{}$\"))",
                any_case(PI_DIR),
                any_case(PI_SETTINGS_LOCK)
            ),
            // fnox's config files by name ([`is_fnox_config`]), after the lock's allow: a `.pi/fnox.toml` stays denied.
            format!(
                "(deny file-write* (regex #\"^{checkout}/(.+/)?\\.?{}(\\.[^/]+)?\\.{}$\"))",
                any_case("fnox"),
                any_case("toml")
            ),
        ]
    }
}

/// A path as the file system will resolve it: canonical when it exists; otherwise its nearest existing ancestor made
/// canonical, with the rest appended (a new file, possibly in new folders). `..` and `.` are resolved first, so a
/// request can't step out of a protected folder by name.
pub fn canonical_target(path: &Path) -> PathBuf {
    let clean = lexical(path);
    if let Ok(real) = std::fs::canonicalize(&clean) {
        return real;
    }
    let mut rest = Vec::new();
    let mut base = clean.as_path();
    while let Some(parent) = base.parent() {
        if let Some(name) = base.file_name() {
            rest.push(name.to_os_string());
        }
        base = parent;
        if let Ok(real) = std::fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |path, name| path.join(name));
        }
    }
    clean
}

/// `path` with `.` and `..` resolved by name, following no symlink.
pub fn lexical(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => clean.push(component),
        }
    }
    clean
}

/// A name as a regex matching it in any case: `.pi` becomes `\.[pP][iI]`.
fn any_case(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            c if c.is_ascii_alphabetic() => format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase()),
            '.' => "\\.".into(),
            c => c.to_string(),
        })
        .collect()
}

/// A path as a literal inside a Seatbelt regex (`#"..."`): regex metacharacters and the literal's quote escaped.
fn regex_escape(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home with the global tier and pi's agent directory linked into a dotfiles repository, a parent level, a
    /// project, a worker of it, and fnox's global config.
    struct Home {
        _dir: tempfile::TempDir,
        home: PathBuf,
        dotfiles: PathBuf,
        protected: Protected,
    }

    fn home() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let dotfiles = home.join("dotfiles");
        let global = home.join(".config/piton");
        let agent = home.join(".pi/agent");
        let parent = home.join("p/.piton");
        let source = home.join("p/repo");
        let checkout = home.join("p/repo--w");
        for d in [
            dotfiles.join("piton"),
            dotfiles.join("pi/extensions"),
            global.join("agents"),
            agent.join("sessions"),
            parent.clone(),
            source.join(".piton/.local"),
            checkout.join(".agents"),
            home.join(".config/fnox"),
            home.join(".local/state/piton"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(dotfiles.join("piton/config.toml"), "").unwrap();
        std::os::unix::fs::symlink(dotfiles.join("piton/config.toml"), global.join("config.toml")).unwrap();
        std::fs::write(dotfiles.join("pi/settings.json"), "{}").unwrap();
        std::os::unix::fs::symlink(dotfiles.join("pi/settings.json"), agent.join("settings.json")).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("pi/extensions"), agent.join("extensions")).unwrap();
        // A runtime entry that's a link (planted, or the user's): never followed.
        std::os::unix::fs::symlink(home.join(".zshrc"), agent.join("crashes.json")).unwrap();
        std::fs::write(home.join(".config/fnox/config.toml"), "").unwrap();
        // Links an agent planted in a parent folder's `.agents/` from a session there (where it's that session's own):
        // to a credential folder, a user deny, the home folder, and the checkout. None makes those proposable.
        let parent_agents = home.join("p/.agents");
        std::fs::create_dir_all(&parent_agents).unwrap();
        for (name, target) in [
            ("ssh", home.join(".ssh")),
            ("secret", home.join("secret")),
            ("home", home.clone()),
            ("repo", source.clone()),
        ] {
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(&target, parent_agents.join(name)).unwrap();
        }
        let levels = [
            global.clone(),
            global.join("agents"),
            parent.clone(),
            parent_agents,
            source.join(".piton"),
            checkout.join(".agents"),
        ];
        let protected = Protected::new(&ProtectedFacts {
            source: &source,
            checkout: &checkout,
            levels: &levels,
            agent_dirs: &[agent.clone(), agent],
            fnox_files: &[home.join(".config/fnox/config.toml")],
            global: &global,
            home: &home,
            excluded: &[home.join(".local/state/piton"), home.join(".ssh"), home.join("secret")],
        });
        Home {
            _dir: dir,
            home,
            dotfiles,
            protected,
        }
    }

    /// Every configuration file the user edits by hand is protected, with the folder its write starts from; nothing
    /// else is.
    #[test]
    fn protects_the_configuration_outside_the_checkout() {
        let h = home();
        let p = &h.protected;
        let root = |path: &Path| p.check(&canonical_target(path)).map(|f| f.root);
        let home = &h.home;
        let global = home.join(".config/piton");
        let agent = home.join(".pi/agent");
        // Levels: their files, a new one too; the folder itself isn't a file.
        assert_eq!(root(&global.join("config.host.toml")), Some(global.clone()));
        assert_eq!(root(&global.join("agents/rules/r.md")), Some(global.join("agents")));
        assert_eq!(
            root(&home.join("p/.piton/config.local.toml")),
            Some(home.join("p/.piton"))
        );
        assert_eq!(root(&global), None);
        // The checkout's own, as before: the source's `.piton/`, not its `.local/`; the worker's `.agents/` is the
        // agent's.
        assert_eq!(root(&home.join("p/repo/.piton/config.toml")), Some(home.join("p/repo")));
        assert_eq!(root(&home.join("p/repo/.piton/.local/x")), None);
        assert_eq!(root(&home.join("p/repo--w/.agents/rules/r.md")), None);
        assert_eq!(
            root(&home.join("p/repo--w/.pi/settings.json")),
            Some(home.join("p/repo--w"))
        );
        // pi's agent directory, not its runtime state.
        assert_eq!(root(&agent.join("keybindings.json")), Some(agent.clone()));
        assert_eq!(root(&agent.join("npm/x/index.js")), Some(agent.clone()));
        for runtime in [
            "sessions/s.jsonl",
            "auth.json",
            "settings.json.lock",
            "mcp-auth-refresh-1.lock",
            "AUTH.JSON",
        ] {
            assert_eq!(root(&agent.join(runtime)), None, "{runtime}");
        }
        // Through a link: written at the real path, from the link target's folder; a linked folder is its own root.
        let settings = h.dotfiles.join("pi/settings.json");
        assert_eq!(canonical_target(&agent.join("settings.json")), settings);
        assert_eq!(root(&settings), Some(h.dotfiles.join("pi")));
        assert_eq!(
            root(&h.dotfiles.join("piton/config.toml")),
            Some(h.dotfiles.join("piton"))
        );
        assert_eq!(
            root(&agent.join("extensions/new.ts")),
            Some(h.dotfiles.join("pi/extensions"))
        );
        // Beside a linked file isn't protected; a runtime entry's link isn't followed.
        assert_eq!(root(&h.dotfiles.join("pi/README.md")), None);
        assert_eq!(root(&h.dotfiles.join("piton/other.toml")), None);
        assert_eq!(root(&home.join(".zshrc")), None);
        // fnox's files, not their folders or what's beside them.
        assert_eq!(
            root(&home.join(".config/fnox/config.toml")),
            Some(home.join(".config/fnox"))
        );
        assert_eq!(root(&home.join(".config/fnox/other.toml")), None);
        // Never the daemon's state, its sockets, credentials, the user's denies, or anything else, even through a link
        // planted in a level (the parent's `.agents/`, which a session there could edit).
        for never in [
            ".local/state/piton/piton.db",
            ".local/state/piton/piton.sock",
            ".ssh/id_ed25519",
            ".ssh/authorized_keys",
            "secret/x",
            ".zshrc",
            "notes.txt",
            "p/repo/src/main.rs",
            "p/repo--w/src/main.rs",
        ] {
            assert_eq!(root(&home.join(never)), None, "{never}");
        }
        assert_eq!(root(&home.join("p/.agents/ssh/authorized_keys")), None);
        assert_eq!(root(&home.join("p/.agents/rules/r.md")), Some(home.join("p/.agents")));
        // Any case: APFS is case-insensitive.
        assert_eq!(root(&global.join("CONFIG.HOST.TOML")), Some(global.clone()));
    }

    /// An area that reaches the daemon's state (an agent directory a host launch's environment named there) never makes
    /// it protected. The checkout's own files stay protected wherever the state folder is (a debug build's socket can
    /// move it).
    #[test]
    fn the_daemons_state_is_never_protected() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let state = home.join(".local/state/piton");
        std::fs::create_dir_all(&state).unwrap();
        let p = Protected::new(&ProtectedFacts {
            source: &home.join("repo"),
            checkout: &home.join("repo"),
            levels: &[],
            agent_dirs: &[home.join(".local/pi"), state.join("agent")],
            fnox_files: &[],
            global: &home.join(".config/piton"),
            home: &home,
            excluded: std::slice::from_ref(&state),
        });
        assert!(p.check(&state.join("piton.db")).is_none());
        assert!(p.check(&state.join("agent/settings.json")).is_none());
        assert!(p.check(&home.join(".local/pi/settings.json")).is_some());
        let moved = Protected::new(&ProtectedFacts {
            source: &home.join("repo"),
            checkout: &home.join("repo"),
            levels: &[],
            agent_dirs: &[],
            fnox_files: &[],
            global: &home.join(".config/piton"),
            home: &home,
            excluded: std::slice::from_ref(&home),
        });
        assert!(moved.check(&home.join("repo/.piton/config.toml")).is_some());
    }

    /// What each file controls, for the review's label; a linked file by the path it's reached through.
    #[test]
    fn labels_what_a_file_controls() {
        let h = home();
        let controls = |path: &Path| h.protected.check(&canonical_target(path)).map(|f| f.controls);
        let home = &h.home;
        let global = home.join(".config/piton");
        let agent = home.join(".pi/agent");
        let cases = [
            (global.join("config.toml"), Controls::Sandbox),
            (global.join("config.local.toml"), Controls::Sandbox),
            (home.join("p/.piton/config.local.toml"), Controls::Sandbox),
            (home.join("p/repo/.piton/config.local.toml"), Controls::Sandbox),
            (home.join("p/.piton/config.toml"), Controls::Other),
            (home.join("p/repo/.piton/config.toml"), Controls::Other),
            (global.join("config.host.toml"), Controls::Code),
            (global.join("config.host.local.toml"), Controls::Code),
            (global.join("config.client.toml"), Controls::Other),
            (global.join("mcp.json"), Controls::Code),
            (home.join("p/repo/.piton/mcp.local.json"), Controls::Code),
            (global.join("policies.md"), Controls::Other),
            (global.join("agents/rules/r.md"), Controls::Other),
            (agent.join("settings.json"), Controls::Code),
            (agent.join("trust.json"), Controls::Code),
            (agent.join("mcp.json"), Controls::Code),
            (agent.join("extensions/x.ts"), Controls::Code),
            (agent.join("npm/node_modules/p/index.js"), Controls::Code),
            (agent.join("git/github.com/a/b/index.ts"), Controls::Code),
            (agent.join("bin/rg"), Controls::Code),
            (agent.join("keybindings.json"), Controls::Other),
            (agent.join("prompts/p.md"), Controls::Other),
            (home.join("p/repo--w/.pi/extensions/x.ts"), Controls::Code),
            (home.join("p/repo--w/apps/.pi/settings.json"), Controls::Code),
            (home.join("p/repo--w/.pi/prompts/p.md"), Controls::Other),
            (home.join("p/repo--w/fnox.toml"), Controls::Secrets),
            (home.join(".config/fnox/config.toml"), Controls::Secrets),
            // Linked into dotfiles: by the link.
            (h.dotfiles.join("piton/config.toml"), Controls::Sandbox),
            (h.dotfiles.join("pi/settings.json"), Controls::Code),
            (h.dotfiles.join("pi/extensions/x.ts"), Controls::Code),
        ];
        for (path, expected) in cases {
            assert_eq!(controls(&path), Some(expected), "{}", path.display());
        }
    }

    /// What a session hands pi round-trips: `piton config protected` answers as the daemon checks.
    #[test]
    fn round_trips_through_the_environment() {
        let h = home();
        let json = serde_json::to_string(&h.protected).unwrap();
        let back: Protected = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h.protected);
    }

    /// What the sandbox fixes in place: each folder on the way, as named and real, never `/`.
    #[test]
    fn fixes_every_folder_on_the_way() {
        let h = home();
        let link = h.home.join(".config/piton/config.toml");
        let entries = fixed_entries(std::slice::from_ref(&link));
        for entry in [
            link.clone(),
            h.home.join(".config/piton"),
            h.home.join(".config"),
            h.dotfiles.join("piton/config.toml"),
            h.dotfiles.join("piton"),
            h.dotfiles.clone(),
        ] {
            assert!(entries.contains(&entry), "{}", entry.display());
        }
        assert!(!entries.contains(&PathBuf::from("/")));
    }

    fn roots() -> ProtectedRoots {
        ProtectedRoots {
            source: "/u/p/repo".into(),
            checkout: "/u/p/repo--w".into(),
        }
    }

    #[test]
    fn classifies_paths() {
        let r = roots();
        let c = |p: &str| r.protected_root(Path::new(p)).map(Path::to_path_buf);
        let source = Some(PathBuf::from("/u/p/repo"));
        let checkout = Some(PathBuf::from("/u/p/repo--w"));
        assert_eq!(c("/u/p/repo/.piton/config.toml"), source);
        assert_eq!(c("/u/p/repo/.piton/.local/gigs/x/notes.md"), None);
        // A worker's own `.piton/` isn't read, so it isn't protected; its `.pi/` is.
        assert_eq!(c("/u/p/repo--w/.piton/config.toml"), None);
        assert_eq!(c("/u/p/repo--w/.pi/settings.json"), checkout);
        assert_eq!(c("/u/p/repo--w/apps/sub/.pi/extensions/x.ts"), checkout);
        // Skills are agent-editable: their `allowed-tools` grant nothing unless the user opts in.
        assert_eq!(c("/u/p/repo--w/.agents/skills/deploy/SKILL.md"), None);
        assert_eq!(c("/u/p/repo--w/apps/.agents/skills/tools/run.md"), None);
        // Any case: APFS is case-insensitive.
        assert_eq!(c("/u/p/repo--w/.PI/settings.json"), checkout);
        assert_eq!(c("/u/p/repo/.PITON/config.toml"), source);
        assert_eq!(c("/u/p/repo/.piton/.LOCAL/x"), None);
        assert_eq!(c("/u/p/repo--w/.agents/rules/r.md"), None);
        assert_eq!(c("/u/p/repo--w/src/main.rs"), None);
        assert_eq!(c("/u/p/repo--w/apps/.pix/f"), None);
        assert_eq!(c("/elsewhere/.pi/x"), None);
        // The lock pi takes to read the project's settings, and only it.
        assert_eq!(c("/u/p/repo--w/.pi/settings.json.lock"), None);
        assert_eq!(c("/u/p/repo--w/apps/.PI/Settings.json.LOCK"), None);
        assert_eq!(c("/u/p/repo--w/.pi/settings.json.lock/x"), checkout);
        assert_eq!(c("/u/p/repo--w/.pi/extensions/settings.json.lock"), checkout);
        // fnox's config files, by name at any depth, in any case.
        for name in [
            "fnox.toml",
            ".fnox.toml",
            ".config/fnox.toml",
            "fnox.local.toml",
            ".fnox.local.toml",
            ".config/fnox.local.toml",
            "fnox.dev.toml",
            ".fnox.dev.local.toml",
            "apps/sub/fnox.toml",
            "FNOX.TOML",
        ] {
            assert_eq!(c(&format!("/u/p/repo--w/{name}")), checkout, "{name}");
        }
        for name in [
            "fnox.json",
            "myfnox.toml",
            "fnox.toml.bak",
            "fnox..toml",
            "fnox.toml/x",
            "docs/fnox.md",
        ] {
            assert_eq!(c(&format!("/u/p/repo--w/{name}")), None, "{name}");
        }
    }

    /// The Seatbelt rule covers exactly the names the daemon's check does.
    #[test]
    fn fnox_rule_matches_the_names() {
        let rule = &roots().seatbelt_rules()[3];
        let pattern = rule
            .split_once("#\"")
            .and_then(|(_, rest)| rest.split_once("\"))"))
            .map(|(pattern, _)| pattern)
            .unwrap();
        let regex = regex_lite::Regex::new(pattern).unwrap();
        for name in [
            "fnox.toml",
            ".fnox.toml",
            ".config/fnox.local.toml",
            "a/b/fnox.dev.toml",
            ".FNOX.Toml",
            "fnox.json",
            "myfnox.toml",
            "fnox.toml.bak",
            "fnox..toml",
            "fnox.toml/x",
        ] {
            let path = format!("/u/p/repo--w/{name}");
            assert_eq!(
                regex.is_match(&path),
                roots().protected_root(Path::new(&path)).is_some(),
                "{name}"
            );
        }
    }

    #[test]
    /// The rules `cli/tests/probe/protected.sh` checks on the host (it builds them the same way).
    fn rules_are_the_ones_the_host_check_proves() {
        let r = ProtectedRoots {
            source: "/u/p/re.po".into(),
            checkout: "/u/p/re.po".into(),
        };
        assert_eq!(
            r.seatbelt_rules(),
            [
                r#"(deny file-write* (require-all (regex #"^/u/p/re\.po/\.[pP][iI][tT][oO][nN](/|$)") (require-not (regex #"^/u/p/re\.po/\.[pP][iI][tT][oO][nN]/\.[lL][oO][cC][aA][lL](/|$)"))))"#,
                r#"(deny file-write* (regex #"^/u/p/re\.po/(.+/)?\.[pP][iI](/|$)"))"#,
                r#"(allow file-write* (regex #"^/u/p/re\.po/(.+/)?\.[pP][iI]/[sS][eE][tT][tT][iI][nN][gG][sS]\.[jJ][sS][oO][nN]\.[lL][oO][cC][kK]$"))"#,
                r#"(deny file-write* (regex #"^/u/p/re\.po/(.+/)?\.?[fF][nN][oO][xX](\.[^/]+)?\.[tT][oO][mM][lL]$"))"#,
            ]
        );
    }
}
