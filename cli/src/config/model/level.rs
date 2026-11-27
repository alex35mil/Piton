//! Levels and chains: which folders apply to a workspace, farthest first.
//!
//! - `.piton/`: the source checkout (a worker's source, the checkout around a subfolder), then each ancestor folder
//!   strictly below the home folder, then the global tier. Settings that decide what an agent may do are read from the
//!   source, never from a worker, so an agent can't change them by editing its clone.
//! - `.agents/`: the workspace's own checkout (a worker reads its own copy, so instructions apply where they're
//!   edited), then the same ancestors, then the global tier's `agents/`.

use std::path::{Path, PathBuf};

use super::paths::{AGENTS_DIR, PITON_DIR};
use crate::project::Workspace;

/// What a level is, for scope checks (global-only keys) and messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelKind {
    /// `$XDG_CONFIG_HOME/piton/`.
    Global,
    /// A folder above the checkout and below the home folder.
    Parent,
    /// The checkout itself (the source for `.piton/`, the workspace's own for `.agents/`).
    Repository,
}

/// One folder of a chain: `<dir>/.piton`, `<dir>/.agents`, or the global tier (or its `agents/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    pub kind: LevelKind,
    /// The folder holding the files.
    pub dir: PathBuf,
}

/// Both chains for one workspace.
#[derive(Debug, Clone)]
pub struct Chain {
    /// `.piton/` levels, farthest (global) first.
    pub piton: Vec<Level>,
    /// `.agents/` levels, farthest (global) first.
    pub agents: Vec<Level>,
    /// The checkout whose `.piton/` is read: never a worker.
    pub source: PathBuf,
    /// The workspace's own checkout: a worker's `.agents/` and pi's `.pi/` come from here.
    pub checkout: PathBuf,
    pub global: PathBuf,
}

impl Chain {
    /// The chains for a resolved workspace. `home` bounds the parent walk; `global` is the global tier.
    pub fn for_workspace(workspace: &Workspace, home: &Path, global: &Path) -> Self {
        let source = workspace.source().to_path_buf();
        let checkout = workspace.checkout.clone();
        Self {
            piton: levels(&source, PITON_DIR, home, global.to_path_buf()),
            agents: levels(&checkout, AGENTS_DIR, home, global.join("agents")),
            source,
            checkout,
            global: global.to_path_buf(),
        }
    }

    /// Only the global tier: for the daemon's own settings, which don't depend on a workspace.
    pub fn global_only(global: &Path) -> Self {
        Self {
            piton: vec![Level {
                kind: LevelKind::Global,
                dir: global.to_path_buf(),
            }],
            agents: vec![Level {
                kind: LevelKind::Global,
                dir: global.join("agents"),
            }],
            source: global.to_path_buf(),
            checkout: global.to_path_buf(),
            global: global.to_path_buf(),
        }
    }
}

/// `<checkout>/<name>`, each ancestor strictly below `home` with `<ancestor>/<name>`, and `global`; farthest first.
/// A checkout outside the home folder gets itself and the global tier.
fn levels(checkout: &Path, name: &str, home: &Path, global: PathBuf) -> Vec<Level> {
    let mut nearest_first = vec![Level {
        kind: LevelKind::Repository,
        dir: checkout.join(name),
    }];
    if checkout.starts_with(home) && checkout != home {
        for ancestor in checkout.ancestors().skip(1) {
            if ancestor == home || !ancestor.starts_with(home) {
                break;
            }
            nearest_first.push(Level {
                kind: LevelKind::Parent,
                dir: ancestor.join(name),
            });
        }
    }
    nearest_first.push(Level {
        kind: LevelKind::Global,
        dir: global,
    });
    nearest_first.reverse();
    nearest_first
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Relation;

    fn dirs(levels: &[Level]) -> Vec<(LevelKind, String)> {
        levels
            .iter()
            .map(|l| (l.kind, l.dir.to_string_lossy().into_owned()))
            .collect()
    }

    #[test]
    fn walks_from_the_global_tier_to_the_checkout() {
        let workspace = Workspace {
            path: "/Users/a/Projects/work/repo/sub".into(),
            checkout: "/Users/a/Projects/work/repo".into(),
            relation: Relation::Source,
        };
        let chain = Chain::for_workspace(&workspace, Path::new("/Users/a"), Path::new("/Users/a/.config/piton"));
        assert_eq!(
            dirs(&chain.piton),
            [
                (LevelKind::Global, "/Users/a/.config/piton".into()),
                (LevelKind::Parent, "/Users/a/Projects/.piton".into()),
                (LevelKind::Parent, "/Users/a/Projects/work/.piton".into()),
                (LevelKind::Repository, "/Users/a/Projects/work/repo/.piton".into()),
            ]
        );
        assert_eq!(chain.agents[0].dir, PathBuf::from("/Users/a/.config/piton/agents"));
        assert_eq!(
            chain.agents[3].dir,
            PathBuf::from("/Users/a/Projects/work/repo/.agents")
        );
    }

    /// A worker's `.piton/` comes from its source; its `.agents/` from itself.
    #[test]
    fn a_worker_reads_piton_from_its_source() {
        let workspace = Workspace {
            path: "/Users/a/p/repo.workers/w".into(),
            checkout: "/Users/a/p/repo.workers/w".into(),
            relation: Relation::Worker {
                source: "/Users/a/p/repo".into(),
                name: "w".into(),
            },
        };
        let chain = Chain::for_workspace(&workspace, Path::new("/Users/a"), Path::new("/g"));
        assert_eq!(chain.piton.last().unwrap().dir, PathBuf::from("/Users/a/p/repo/.piton"));
        assert_eq!(
            chain.agents.last().unwrap().dir,
            PathBuf::from("/Users/a/p/repo.workers/w/.agents")
        );
        assert_eq!(chain.source, PathBuf::from("/Users/a/p/repo"));
        assert_eq!(chain.checkout, PathBuf::from("/Users/a/p/repo.workers/w"));
    }

    #[test]
    fn outside_home_only_the_checkout_and_the_global_tier() {
        let workspace = Workspace {
            path: "/opt/repo".into(),
            checkout: "/opt/repo".into(),
            relation: Relation::Source,
        };
        let chain = Chain::for_workspace(&workspace, Path::new("/Users/a"), Path::new("/g"));
        assert_eq!(
            dirs(&chain.piton),
            [
                (LevelKind::Global, "/g".into()),
                (LevelKind::Repository, "/opt/repo/.piton".into())
            ]
        );
    }
}
