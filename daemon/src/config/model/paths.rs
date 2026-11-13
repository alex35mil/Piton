//! Where configuration lives: the user's home, the global tier, the folders a repository or parent folder holds, and a
//! level's file pair.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The folder piton's tools read in a repository or a parent folder.
pub const PITON_DIR: &str = ".piton";
/// The folder the agent reads in a repository or a parent folder.
pub const AGENTS_DIR: &str = ".agents";
/// Workflow artifacts in a source checkout's `.piton/`; never configuration. A worker links its source's.
pub const LOCAL_DIR: &str = ".piton/.local";

/// The machine-wide paths configuration depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The user's home folder: bounds the parent-folder walk and expands `~` in values.
    pub home: PathBuf,
    /// The global tier.
    pub global: PathBuf,
}

impl Paths {
    /// From this process: the user database's home and the global tier.
    pub fn current() -> Self {
        Self {
            global: global_tier(),
            home: home(),
        }
    }

    /// The global tier's file pair.
    pub fn global_files(&self) -> FilePair {
        FilePair::in_dir(&self.global)
    }
}

/// The user's home folder, from the user database: a repository's `mise.toml` can set `$HOME` for the processes it
/// runs, and configuration must not follow it. `$HOME` only when the user database has no entry.
pub fn home() -> PathBuf {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|user| user.dir)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The global tier. Inside a pi the daemon launched, `PITON_CONFIG_HOME` names it (set after the mise environment, so
/// a repository can't move it); otherwise `$XDG_CONFIG_HOME/piton`, else `~/.config/piton`. The daemon resolves it
/// once at start, from its own (login-shell) environment.
pub fn global_tier() -> PathBuf {
    global_tier_from(
        std::env::var_os("PITON_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        &home(),
    )
}

fn global_tier_from(piton: Option<PathBuf>, xdg: Option<PathBuf>, home: &Path) -> PathBuf {
    match (piton, xdg) {
        (Some(piton), _) if piton.is_absolute() => piton,
        (_, Some(xdg)) if xdg.is_absolute() => xdg.join("piton"),
        (Some(_) | None, Some(_) | None) => home.join(".config/piton"),
    }
}

/// Expands a leading `~` or `$HOME` with `home` (from the user database, not the environment a repository can set).
/// Other `$VARS` are left for the sandbox backend to expand.
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" || path == "$HOME" {
        return home.to_path_buf();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("$HOME/")) {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

/// Which file of a level's pair: the one that's shared (committed, or through dotfiles) or this machine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sharing {
    /// `config.toml`.
    Shared,
    /// `config.local.toml`.
    Local,
}

impl Sharing {
    pub fn file_name(self) -> &'static str {
        match self {
            Sharing::Shared => "config.toml",
            Sharing::Local => "config.local.toml",
        }
    }

    pub fn other(self) -> Sharing {
        match self {
            Sharing::Shared => Sharing::Local,
            Sharing::Local => Sharing::Shared,
        }
    }
}

/// A level's `config.toml` and `config.local.toml`, existing or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePair {
    pub shared: PathBuf,
    pub local: PathBuf,
}

impl FilePair {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            shared: dir.join(Sharing::Shared.file_name()),
            local: dir.join(Sharing::Local.file_name()),
        }
    }

    pub fn path(&self, sharing: Sharing) -> &Path {
        match sharing {
            Sharing::Shared => &self.shared,
            Sharing::Local => &self.local,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_global_tier_follows_xdg() {
        let home = Path::new("/Users/a");
        assert_eq!(
            global_tier_from(Some("/x/p".into()), Some("/x".into()), home),
            PathBuf::from("/x/p")
        );
        assert_eq!(
            global_tier_from(None, Some("/x".into()), home),
            PathBuf::from("/x/piton")
        );
        assert_eq!(
            global_tier_from(None, None, home),
            PathBuf::from("/Users/a/.config/piton")
        );
        // Relative values are ignored, as the XDG spec says.
        assert_eq!(
            global_tier_from(Some("rel".into()), Some("rel".into()), home),
            PathBuf::from("/Users/a/.config/piton")
        );
    }

    #[test]
    fn home_expansion() {
        let home = Path::new("/Users/a");
        assert_eq!(expand_home("~/x", home), PathBuf::from("/Users/a/x"));
        assert_eq!(expand_home("$HOME/x", home), PathBuf::from("/Users/a/x"));
        assert_eq!(expand_home("~", home), PathBuf::from("/Users/a"));
        assert_eq!(expand_home("/abs", home), PathBuf::from("/abs"));
        assert_eq!(expand_home("$SSH_AUTH_SOCK", home), PathBuf::from("$SSH_AUTH_SOCK"));
    }
}
