//! Protected files: what an agent may not change without the user's approval, because it decides what the agent may do
//! or what runs. The source checkout's `.piton/` (except `.local/`, the workflow artifacts), and `.pi/` and
//! `.agents/skills/` at any depth in the workspace's checkout.
//!
//! Two layers use this one definition: the Seatbelt rules every sandboxed launch carries (any write by any process is
//! denied) and the daemon's check of a write an agent asks for over `agent.sock` (written only after the user approves
//! it). See "Protected files" in docs/design.md.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::settings::{LOCAL_DIR, PITON_DIR};

/// What a protected path is, which decides whether a change needs the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "file", rename_all = "snake_case")]
pub enum Protected {
    /// A file in the source's `.piton/`: settings, permissions, sandbox grants.
    Configuration,
    /// A file under a `.pi/` folder: pi's own project settings and extensions, which run code.
    Pi,
    /// A file under an `.agents/skills/<skill>/` folder.
    Skill(SkillFile),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillFile {
    /// `SKILL.md`: its `allowed-tools` grant permissions.
    Definition,
    /// A script the skill can run.
    Script,
    /// Anything else (references, assets).
    Support,
}

/// The roots protection is computed for: one launch's source and workspace checkouts (canonical).
#[derive(Debug, Clone)]
pub struct Roots {
    pub source: PathBuf,
    pub checkout: PathBuf,
}

impl Roots {
    /// What `path` (canonical, absolute) is, or None when it isn't protected.
    pub fn classify(&self, path: &Path) -> Option<Protected> {
        let configuration = self.source.join(PITON_DIR);
        if path.starts_with(&configuration) && !path.starts_with(self.source.join(LOCAL_DIR)) {
            return Some(Protected::Configuration);
        }
        let relative = path.strip_prefix(&self.checkout).ok()?;
        let parts: Vec<&str> = relative
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => name.to_str(),
                Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => None,
            })
            .collect();
        if parts.contains(&".pi") {
            return Some(Protected::Pi);
        }
        let skills = parts.windows(2).position(|w| w == [".agents", "skills"])?;
        let inside = &parts[skills + 2..];
        Some(Protected::Skill(match inside {
            [_skill, "SKILL.md"] => SkillFile::Definition,
            [_skill, rest @ ..] if is_script(rest) => SkillFile::Script,
            [] | [_] | [_, ..] => SkillFile::Support,
        }))
    }

    /// The Seatbelt rules for a sandboxed launch: deny every write to the protected paths. Paths are canonical, as
    /// Seatbelt compares them.
    pub fn seatbelt_rules(&self) -> Vec<String> {
        let source = quote(&self.source.to_string_lossy());
        let checkout = regex_escape(&self.checkout.to_string_lossy());
        vec![
            format!(
                "(deny file-write* (require-all (subpath \"{source}/{PITON_DIR}\") (require-not (subpath \"{source}/{LOCAL_DIR}\"))))"
            ),
            format!("(deny file-write* (regex #\"^{checkout}/(.+/)?\\.pi(/|$)\"))"),
            format!("(deny file-write* (regex #\"^{checkout}/(.+/)?\\.agents/skills(/|$)\"))"),
        ]
    }
}

/// A path as the file system will resolve it: canonical when it exists; otherwise its nearest existing ancestor made
/// canonical, with the rest appended (a new file, possibly in new folders). `..` and `.` are resolved first, so a
/// request can't step out of a protected folder by name.
pub fn canonical_target(path: &Path) -> PathBuf {
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

/// A skill file is a script when it's in a `scripts/` folder or has a script's extension.
fn is_script(path: &[&str]) -> bool {
    let in_scripts = path.len() > 1 && path[..path.len() - 1].contains(&"scripts");
    let name = path.last().copied().unwrap_or_default();
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    in_scripts
        || matches!(
            extension,
            Some("sh" | "bash" | "zsh" | "fish" | "py" | "js" | "mjs" | "cjs" | "ts" | "rb" | "pl")
        )
}

/// A path inside a Seatbelt string literal.
fn quote(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
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

    fn roots() -> Roots {
        Roots {
            source: "/u/p/repo".into(),
            checkout: "/u/p/repo--w".into(),
        }
    }

    #[test]
    fn classifies_paths() {
        let r = roots();
        let c = |p: &str| r.classify(Path::new(p));
        assert_eq!(c("/u/p/repo/.piton/permissions.json"), Some(Protected::Configuration));
        assert_eq!(c("/u/p/repo/.piton/.local/gigs/x/notes.md"), None);
        // A worker's own `.piton/` isn't read, so it isn't protected; its `.pi/` and skills are.
        assert_eq!(c("/u/p/repo--w/.piton/permissions.json"), None);
        assert_eq!(c("/u/p/repo--w/.pi/settings.json"), Some(Protected::Pi));
        assert_eq!(c("/u/p/repo--w/apps/sub/.pi/extensions/x.ts"), Some(Protected::Pi));
        assert_eq!(
            c("/u/p/repo--w/.agents/skills/deploy/SKILL.md"),
            Some(Protected::Skill(SkillFile::Definition))
        );
        assert_eq!(
            c("/u/p/repo--w/apps/.agents/skills/deploy/scripts/run"),
            Some(Protected::Skill(SkillFile::Script))
        );
        assert_eq!(
            c("/u/p/repo--w/.agents/skills/deploy/tool.py"),
            Some(Protected::Skill(SkillFile::Script))
        );
        assert_eq!(
            c("/u/p/repo--w/.agents/skills/deploy/reference.md"),
            Some(Protected::Skill(SkillFile::Support))
        );
        assert_eq!(c("/u/p/repo--w/.agents/rules/r.md"), None);
        assert_eq!(c("/u/p/repo--w/src/main.rs"), None);
        assert_eq!(c("/u/p/repo--w/apps/.pix/f"), None);
        assert_eq!(c("/elsewhere/.pi/x"), None);
    }

    #[test]
    fn rules_are_the_ones_the_host_check_proved() {
        let r = Roots {
            source: "/u/p/re.po".into(),
            checkout: "/u/p/re.po".into(),
        };
        assert_eq!(
            r.seatbelt_rules(),
            [
                r#"(deny file-write* (require-all (subpath "/u/p/re.po/.piton") (require-not (subpath "/u/p/re.po/.piton/.local"))))"#,
                r#"(deny file-write* (regex #"^/u/p/re\.po/(.+/)?\.pi(/|$)"))"#,
                r#"(deny file-write* (regex #"^/u/p/re\.po/(.+/)?\.agents/skills(/|$)"))"#,
            ]
        );
    }
}
