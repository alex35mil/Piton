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

    /// The global tier's pair of `scope`.
    pub fn global_files(&self, scope: Scope) -> FilePair {
        FilePair::of(&self.global, scope)
    }
}

/// Which configuration a file holds: a project's (`config.toml` at every level of the chain), or one of the global
/// tier's own files. The scope decides the file names and who reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `config.toml` / `config.local.toml` at every level: a project's settings, the global one its defaults.
    Project,
    /// `config.host.toml` / `config.host.local.toml`, global only: how the daemon runs and this host's preferences.
    Host,
    /// `config.client.toml`, global only, no local file: the apps' preferences, served to every client.
    Client,
}

impl Scope {
    pub const GLOBAL: [Scope; 3] = [Scope::Project, Scope::Host, Scope::Client];

    /// The shared file: every scope has one.
    pub fn shared_name(self) -> &'static str {
        match self {
            Scope::Project => "config.toml",
            Scope::Host => "config.host.toml",
            Scope::Client => "config.client.toml",
        }
    }

    /// The local file, when the scope has one (the client file doesn't: per-device values live in each app).
    pub fn local_name(self) -> Option<&'static str> {
        match self {
            Scope::Project => Some("config.local.toml"),
            Scope::Host => Some("config.host.local.toml"),
            Scope::Client => None,
        }
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

/// The global tier of this process: `$XDG_CONFIG_HOME/piton`, else `~/.config/piton`. `--config-home` replaces it, and
/// inside a session, the session's (`facts`) does. The daemon resolves it once at start, from its own (login-shell)
/// environment.
pub fn global_tier() -> PathBuf {
    global_tier_from(std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from), &home())
}

fn global_tier_from(xdg: Option<PathBuf>, home: &Path) -> PathBuf {
    match xdg {
        Some(xdg) if xdg.is_absolute() => xdg.join("piton"),
        Some(_) | None => home.join(".config/piton"),
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
    pub fn other(self) -> Sharing {
        match self {
            Sharing::Shared => Sharing::Local,
            Sharing::Local => Sharing::Shared,
        }
    }
}

/// A level's files of one scope, existing or not: the shared one, and the local one when the scope has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePair {
    pub scope: Scope,
    pub shared: PathBuf,
    pub local: Option<PathBuf>,
}

impl FilePair {
    /// `dir`'s files of `scope`.
    pub fn of(dir: &Path, scope: Scope) -> Self {
        Self {
            scope,
            shared: dir.join(scope.shared_name()),
            local: scope.local_name().map(|name| dir.join(name)),
        }
    }

    /// A project level's `config.toml` and `config.local.toml`.
    pub fn project(dir: &Path) -> Self {
        Self::of(dir, Scope::Project)
    }

    /// The file of `sharing`, or `None` when the scope has no such file.
    pub fn path(&self, sharing: Sharing) -> Option<&Path> {
        match sharing {
            Sharing::Shared => Some(&self.shared),
            Sharing::Local => self.local.as_deref(),
        }
    }

    /// Every file of the pair, in merge order: the shared file, then the local one.
    pub fn paths(&self) -> impl Iterator<Item = (Sharing, &Path)> {
        std::iter::once((Sharing::Shared, self.shared.as_path()))
            .chain(self.local.as_deref().map(|local| (Sharing::Local, local)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_global_tier_follows_xdg() {
        let home = Path::new("/Users/a");
        assert_eq!(global_tier_from(Some("/x".into()), home), PathBuf::from("/x/piton"));
        assert_eq!(global_tier_from(None, home), PathBuf::from("/Users/a/.config/piton"));
        // A relative value is ignored, as the XDG spec says.
        assert_eq!(
            global_tier_from(Some("rel".into()), home),
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
