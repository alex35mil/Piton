//! The global tier's tables: what the daemon runs with and the apps show.

use std::path::Path;

use super::resolve::{Resolved, resolve_global};
use crate::config::ConfigError;
use crate::config::model::daemon::{DaemonConfig, GlobalConfig};
use crate::config::model::paths::Paths;

impl GlobalConfig {
    /// Every global table, or the first that can't be used.
    pub fn from_resolved(resolved: Resolved) -> Result<Self, ConfigError> {
        Ok(Self {
            run: resolved.run?,
            sandbox: resolved.sandbox?,
            workers: resolved.workers?,
            host: resolved.host?,
            client: resolved.client?,
        })
    }

    /// Each table that can be used; defaults for the rest.
    fn lenient(resolved: Resolved) -> Self {
        Self {
            run: resolved.run.unwrap_or_default(),
            sandbox: resolved.sandbox.unwrap_or_default(),
            workers: resolved.workers.unwrap_or_default(),
            host: resolved.host.unwrap_or_default(),
            client: resolved.client.unwrap_or_default(),
        }
    }
}

/// The global tables at `paths`, or why they can't be used.
pub fn global(paths: &Paths) -> Result<GlobalConfig, ConfigError> {
    GlobalConfig::from_resolved(resolve_global(paths)?)
}

/// The paths of this process, with `config_home` (`--config-home`) as the global tier when given.
pub fn paths(config_home: Option<&Path>) -> Paths {
    let mut paths = Paths::current();
    if let Some(dir) = config_home {
        paths.global = dir.to_path_buf();
    }
    paths
}

/// The daemon's options from the global tier.
pub fn daemon(config_home: Option<&Path>) -> Result<DaemonConfig, ConfigError> {
    let paths = paths(config_home);
    Ok(DaemonConfig::new(&global(&paths)?, paths))
}

/// The daemon's options as far as the global tier can be read: each table that can be used, defaults for the rest.
/// For `piton setup`, whose job is to report a broken configuration rather than refuse to start over it.
pub fn daemon_lenient(config_home: Option<&Path>) -> DaemonConfig {
    let paths = paths(config_home);
    let global = match resolve_global(&paths) {
        Ok(resolved) => GlobalConfig::lenient(resolved),
        Err(_) => GlobalConfig::default(),
    };
    DaemonConfig::new(&global, paths)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::wire::LaunchMode;

    #[test]
    fn global_files_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("config.toml"),
            "socket = \"/tmp/s.sock\"\n[stop]\ngrace_ms = 5\n[sandbox]\ncommand = [\"/bin/nono\"]\n[host]\nroots = [\"~/p\"]\nattach_mode = \"host\"\nlogging = \"debug\"\n",
        )
        .unwrap();
        std::fs::write(global.join("config.local.toml"), "[stop]\nterm_grace_ms = 7\n").unwrap();
        let config = daemon(Some(&global)).unwrap();
        assert_eq!(config.socket, PathBuf::from("/tmp/s.sock"));
        assert_eq!((config.stop.grace_ms, config.stop.term_grace_ms), (5, 7));
        assert_eq!(config.sandbox.command, ["/bin/nono"]);
        assert_eq!(config.roots, ["~/p"]);
        assert_eq!(config.attach_mode, LaunchMode::Host);
        assert_eq!(config.logging.mode, crate::diagnostics::Mode::Debug);
        assert_eq!(config.pi.command, ["pi"]);
        assert_eq!(config.empty_grace_ms, 10_000);
    }
}
