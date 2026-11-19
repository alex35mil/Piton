//! Git's control files: what decides which programs Git runs. A sandboxed agent that could write them would make the
//! next Git command on the host (the user's shell, an editor plugin, the daemon) run its code as the user: hooks, and
//! `core.hooksPath`, `core.fsmonitor`, `core.pager` in the configuration, and `commondir`, which points Git at another
//! git directory whose hooks and configuration it then uses. Everything else in `.git` (objects, refs, the index, logs)
//! stays writable, so commits, branches, fetches, and pushes work in the sandbox.
//!
//! Inside any `.git` folder of a launch's checkouts, at any depth: `hooks/` and what's in it, `config`,
//! `config.worktree`, and `commondir`. These name children of a folder called `.git`, so a linked worktree's `.git`
//! file can still be created anywhere (`git worktree add`). The checkout's own top-level `.git` can't be removed,
//! renamed, or replaced. When a checkout's `.git` is a file (a linked worktree or a submodule), that file can't be
//! changed, and the git directory it shares is covered the same way.
//!
//! Not covered: a `.git` entry created below the checkout's top level, which makes Git run in that folder use whatever
//! git directory it names. Denying it would also break `git worktree add` and checking out initialized submodules,
//! which create such `.git` files; Claude Code leaves it open too. (`git clone` and `git init` into the checkout already
//! fail: they write a new `.git/config`.) Nor the git directories Git keeps inside `.git` for linked worktrees (`worktrees/<id>/`, which
//! `git worktree add` writes) and submodules (`modules/<name>/`, which `git submodule update` writes): a planted file
//! there runs only when Git runs in that worktree or submodule. See "Agent access" in docs/design.md.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DOT_GIT: &str = ".git";
/// The entries of a git directory that decide what Git runs.
const CONTROL_FILES: [&str; 3] = ["config", "config.worktree", "commondir"];
const HOOKS: &str = "hooks";

/// The checkouts and git directories one launch protects: canonical, absolute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitControl {
    /// The launch's checkout and its source (the same folder outside workers).
    pub checkouts: Vec<PathBuf>,
    /// Git directories shared with a checkout whose `.git` is a file (`git rev-parse --git-common-dir`): a linked
    /// worktree's main repository, a submodule's folder in its superproject.
    pub common_dirs: Vec<PathBuf>,
}

impl GitControl {
    /// The checkouts' Git control, with each checkout's shared git directory when its `.git` is a file. Read with
    /// hooks and fsmonitor off: the daemon runs this on the host, in a checkout an agent may have written.
    pub fn of(checkouts: &[&Path]) -> Self {
        let mut unique: Vec<PathBuf> = Vec::new();
        for checkout in checkouts {
            if !unique.iter().any(|c| c == checkout) {
                unique.push(checkout.to_path_buf());
            }
        }
        let common_dirs = unique
            .iter()
            .filter(|checkout| checkout.join(DOT_GIT).symlink_metadata().is_ok_and(|m| m.is_file()))
            .filter_map(|checkout| common_dir(checkout))
            .filter(|dir| !unique.iter().any(|c| dir.starts_with(c.join(DOT_GIT))))
            .collect();
        Self {
            checkouts: unique,
            common_dirs,
        }
    }

    /// Whether writing `path` (canonical, absolute) is denied.
    pub fn covers(&self, path: &Path) -> bool {
        self.checkouts.iter().any(|checkout| covers_in_checkout(checkout, path))
            || self.common_dirs.iter().any(|dir| covers_git_dir(dir, path))
    }

    /// The Seatbelt rules: write denies, every name in any case (APFS is case-insensitive by default, so `.GIT/HOOKS`
    /// is `.git/hooks`).
    pub fn seatbelt_rules(&self) -> Vec<String> {
        let dot_git = any_case(DOT_GIT);
        let hooks = any_case(HOOKS);
        let files = CONTROL_FILES.iter().map(|f| any_case(f)).collect::<Vec<_>>().join("|");
        let mut rules = Vec::new();
        for checkout in &self.checkouts {
            let root = regex_escape(&checkout.to_string_lossy());
            // The checkout's own `.git`, file or folder, as an entry: not removed, renamed, or replaced.
            rules.push(format!("(deny file-write* (regex #\"^{root}/{dot_git}$\"))"));
            // Any `.git` folder below it: its hooks and control files.
            rules.push(format!(
                "(deny file-write* (regex #\"^{root}/(.+/)?{dot_git}/{hooks}(/|$)\"))"
            ));
            rules.push(format!(
                "(deny file-write* (regex #\"^{root}/(.+/)?{dot_git}/({files})$\"))"
            ));
        }
        for dir in &self.common_dirs {
            let root = regex_escape(&dir.to_string_lossy());
            rules.push(format!("(deny file-write* (regex #\"^{root}/{hooks}(/|$)\"))"));
            rules.push(format!("(deny file-write* (regex #\"^{root}/({files})$\"))"));
        }
        rules
    }
}

/// Inside `checkout`: its own `.git` entry, or a control entry of any `.git` folder below it.
fn covers_in_checkout(checkout: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(checkout) else {
        return false;
    };
    let names: Vec<String> = relative
        .iter()
        .map(|name| name.to_string_lossy().to_lowercase())
        .collect();
    if names == [DOT_GIT] {
        return true;
    }
    names.iter().enumerate().any(|(at, name)| {
        name == DOT_GIT
            && match &names[at + 1..] {
                [first, ..] if first == HOOKS => true,
                [only] => CONTROL_FILES.contains(&only.as_str()),
                _ => false,
            }
    })
}

/// Inside a git directory: its hooks and control files.
fn covers_git_dir(dir: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(dir) else {
        return false;
    };
    let names: Vec<String> = relative
        .iter()
        .map(|name| name.to_string_lossy().to_lowercase())
        .collect();
    match names.as_slice() {
        [first, ..] if first == HOOKS => true,
        [only] => CONTROL_FILES.contains(&only.as_str()),
        _ => false,
    }
}

/// The git directory `checkout` shares, from Git itself with nothing of the checkout's run.
fn common_dir(checkout: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null"])
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    std::fs::canonicalize(dir).ok()
}

/// A name as a regex matching it in any case: `.git` becomes `\.[gG][iI][tT]`.
fn any_case(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            c if c.is_ascii_alphabetic() => format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase()),
            '.' => "\\.".into(),
            c => c.to_string(),
        })
        .collect()
}

/// A path as a literal inside a Seatbelt regex (`#"..."`).
fn regex_escape(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if "\\.^$*+?()[]{}|".contains(c) || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> GitControl {
        GitControl {
            checkouts: vec!["/u/p/repo".into()],
            common_dirs: vec!["/u/p/main/.git".into()],
        }
    }

    #[test]
    fn covers_what_decides_what_git_runs() {
        let g = control();
        let c = |p: &str| g.covers(Path::new(p));
        for denied in [
            "/u/p/repo/.git",
            "/u/p/repo/.git/hooks",
            "/u/p/repo/.git/hooks/post-merge",
            "/u/p/repo/.git/config",
            "/u/p/repo/.git/config.worktree",
            "/u/p/repo/.git/commondir",
            // Other case: APFS is case-insensitive.
            "/u/p/repo/.GIT/Hooks/pre-commit",
            "/u/p/repo/.git/CONFIG",
            // Any `.git` folder below the checkout: a nested repository, a submodule's own.
            "/u/p/repo/vendor/lib/.git/config",
            "/u/p/repo/vendor/lib/.git/hooks/x",
            // The shared git directory of a checkout whose `.git` is a file.
            "/u/p/main/.git/config",
            "/u/p/main/.git/hooks/post-merge",
            "/u/p/main/.git/commondir",
        ] {
            assert!(c(denied), "{denied}");
        }
        for allowed in [
            "/u/p/repo/.git/index",
            "/u/p/repo/.git/HEAD",
            "/u/p/repo/.git/refs/heads/main",
            "/u/p/repo/.git/objects/ab/cdef",
            "/u/p/repo/.git/info/exclude",
            "/u/p/repo/.git/config.lock2",
            "/u/p/repo/.git/hooksx",
            // A linked worktree's own folder in the git directory: `git worktree add` writes `commondir` there.
            "/u/p/repo/.git/worktrees/rebase/commondir",
            "/u/p/repo/.git/worktrees/rebase/HEAD",
            // A submodule's git directory inside the superproject's (`git submodule update` writes it); a documented
            // limit, like the linked worktrees' folders.
            "/u/p/repo/.git/modules/lib/config",
            "/u/p/repo/.git/modules/lib/hooks/post-checkout",
            // A linked worktree's `.git` file inside the checkout (the `/pr` rebase's scratch worktree).
            "/u/p/repo/.piton/.local/pull-requests/1/rebase/.git",
            "/u/p/repo/src/.git",
            "/u/p/repo/src/main.rs",
            "/u/p/main/.git/index",
            "/u/p/main/.git/worktrees/w/commondir",
            "/u/p/other/.git/config",
        ] {
            assert!(!c(allowed), "{allowed}");
        }
    }

    #[test]
    /// The rules `cli/tests/probe/protected.sh` checks on the host.
    fn seatbelt_rules() {
        let g = GitControl {
            checkouts: vec!["/u/re.po".into()],
            common_dirs: vec!["/u/main/.git".into()],
        };
        assert_eq!(
            g.seatbelt_rules(),
            [
                r#"(deny file-write* (regex #"^/u/re\.po/\.[gG][iI][tT]$"))"#,
                r#"(deny file-write* (regex #"^/u/re\.po/(.+/)?\.[gG][iI][tT]/[hH][oO][oO][kK][sS](/|$)"))"#,
                r#"(deny file-write* (regex #"^/u/re\.po/(.+/)?\.[gG][iI][tT]/([cC][oO][nN][fF][iI][gG]|[cC][oO][nN][fF][iI][gG]\.[wW][oO][rR][kK][tT][rR][eE][eE]|[cC][oO][mM][mM][oO][nN][dD][iI][rR])$"))"#,
                r#"(deny file-write* (regex #"^/u/main/\.git/[hH][oO][oO][kK][sS](/|$)"))"#,
                r#"(deny file-write* (regex #"^/u/main/\.git/([cC][oO][nN][fF][iI][gG]|[cC][oO][nN][fF][iI][gG]\.[wW][oO][rR][kK][tT][rR][eE][eE]|[cC][oO][mM][mM][oO][nN][dD][iI][rR])$"))"#,
            ]
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A linked worktree's checkout covers the repository it shares; a plain checkout has nothing to share.
    #[test]
    fn a_linked_worktree_covers_its_shared_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(
            &main,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        );
        let linked = root.join("linked");
        git(&main, &["worktree", "add", "-q", "--detach", linked.to_str().unwrap()]);

        let plain = GitControl::of(&[&main, &main]);
        assert_eq!(plain.checkouts, std::slice::from_ref(&main));
        assert!(plain.common_dirs.is_empty());

        let g = GitControl::of(&[&linked]);
        assert_eq!(g.common_dirs, [main.join(".git")]);
        assert!(g.covers(&main.join(".git/hooks/post-checkout")));
        assert!(g.covers(&linked.join(".git")), "the worktree's pointer file");
        assert!(!g.covers(&main.join(".git/refs/heads/x")));
    }
}
