//! Protected files: what an agent may not change without the user's approval, because it decides what the agent may do
//! or what runs. The source checkout's `.piton/` (except `.local/`, the workflow artifacts), and at any depth in the
//! workspace's checkout: `.pi/`, and fnox's config files (which secrets go where, and the commands fnox runs to get
//! them). `.pi/settings.json.lock` itself isn't protected: pi creates that lock folder to read the project's settings,
//! and it decides nothing.
//!
//! Two layers use this one definition: the Seatbelt rules every sandboxed launch carries (any write by any process is
//! denied) and the daemon's check of a write an agent asks for over `agent.sock` (written only after the user approves
//! it). See "Protected files" in docs/design.md.

use std::path::{Component, Path, PathBuf};

use crate::config::{LOCAL_DIR, PITON_DIR};

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
