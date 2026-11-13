//! Watches the global configuration files, so a hand edit (or a dotfiles pull) applies without a restart: the daemon
//! rereads them and pushes the new settings to the apps. Keys that need a restart are reported as pending.
//!
//! The watched folders follow the files: the global folder (or, until it exists, its nearest existing parent, so its
//! creation is noticed) and the folder of each file's symlink target. They're recomputed after every change, so a
//! folder created later or a file linked into dotfiles later is picked up.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::daemon::Daemon;

/// Changes closer together than this are one reread (an editor's save is several events).
const SETTLE: Duration = Duration::from_millis(200);

pub fn spawn(daemon: Arc<Daemon>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = tx.send(());
        }
    }) {
        Ok(watcher) => watcher,
        Err(e) => {
            crate::log!("settings: can't watch the global files ({e}); changes apply at the next start");
            return;
        }
    };
    let mut watching = Watching {
        watcher,
        dirs: BTreeSet::new(),
    };
    watching.sync(&daemon);
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            tokio::time::sleep(SETTLE).await;
            while rx.try_recv().is_ok() {}
            let reloading = daemon.clone();
            let _ = tokio::task::spawn_blocking(move || reloading.reload_preferences()).await;
            watching.sync(&daemon);
        }
    });
}

struct Watching {
    watcher: RecommendedWatcher,
    dirs: BTreeSet<PathBuf>,
}

impl Watching {
    /// Watches the folders the files are in now, and stops watching the ones they left.
    fn sync(&mut self, daemon: &Daemon) {
        let wanted: BTreeSet<PathBuf> = crate::preferences::watched(&daemon.config().paths)
            .into_iter()
            .filter_map(nearest_existing)
            .collect();
        for gone in self.dirs.difference(&wanted).cloned().collect::<Vec<_>>() {
            let _ = self.watcher.unwatch(&gone);
            self.dirs.remove(&gone);
        }
        for dir in wanted {
            if self.dirs.contains(&dir) {
                continue;
            }
            match self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.dirs.insert(dir);
                }
                Err(e) => crate::log!("settings: can't watch {} ({e})", dir.display()),
            }
        }
    }
}

/// `dir`, or its nearest parent that exists.
fn nearest_existing(dir: PathBuf) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.is_dir()).map(PathBuf::from)
}
