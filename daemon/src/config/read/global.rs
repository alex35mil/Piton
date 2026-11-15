//! The global tier's own files, each read on its own: the host file (the daemon's options and this host's preferences),
//! the client file (the apps' preferences), and the global `config.toml` (the defaults every project inherits). Each
//! caller fails only on the files it reads: a broken client file stops the apps' settings list, never the daemon.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::parse::{GlobalFile, read_global};
use super::resolve::{Resolved, Section, resolve_global_project, section};
use crate::config::ConfigError;
use crate::config::model::client::ClientFile;
use crate::config::model::daemon::DaemonConfig;
use crate::config::model::host::{DevelopmentTable, HostFile};
use crate::config::model::paths::{Paths, Scope};

/// A global file read as a whole, with its own file and its local one merged (the local one wins per key).
pub fn read_scope<T: GlobalFile>(paths: &Paths) -> Result<T, ConfigError> {
    let mut merged = T::default();
    for (_, path) in paths.global_files(T::SCOPE).paths() {
        if let Some(value) = read_global::<T>(path)? {
            merged.merge(value);
        }
    }
    Ok(merged)
}

/// The host file.
pub fn host(paths: &Paths) -> Result<HostFile, ConfigError> {
    read_scope(paths)
}

/// The client file.
pub fn client(paths: &Paths) -> Result<ClientFile, ConfigError> {
    read_scope(paths)
}

/// Every global file the apps' settings list shows, each with its own result.
#[derive(Debug)]
pub struct GlobalConfig {
    pub host: Result<HostFile, ConfigError>,
    pub client: Result<ClientFile, ConfigError>,
    /// The global `config.toml`'s `[sandbox]` and `[workers]`.
    pub project: Result<Resolved, ConfigError>,
}

impl GlobalConfig {
    /// Every global file's problem, by part name: `host`, `client`, and the global `config.toml`'s parts.
    pub fn errors(&self) -> Vec<(String, String)> {
        let mut errors = Vec::new();
        if let Err(e) = &self.host {
            errors.push(("host".to_string(), e.to_string()));
        }
        if let Err(e) = &self.client {
            errors.push(("client".to_string(), e.to_string()));
        }
        match &self.project {
            Ok(resolved) => errors.extend(
                resolved
                    .errors()
                    .into_iter()
                    .map(|(part, e)| (part.to_string(), e.to_string())),
            ),
            Err(e) => errors.push(("config".to_string(), e.to_string())),
        }
        errors
    }
}

/// The global tier's files, each read on its own.
pub fn global(paths: &Paths) -> GlobalConfig {
    GlobalConfig {
        host: host(paths),
        client: client(paths),
        project: resolve_global_project(paths),
    }
}

/// Every path whose change can change the global files: the candidate files of each scope (the folders are watched).
pub fn global_candidates(paths: &Paths) -> Vec<PathBuf> {
    Scope::GLOBAL
        .into_iter()
        .flat_map(|scope| {
            paths
                .global_files(scope)
                .paths()
                .map(|(_, path)| path.to_path_buf())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// What `piton config resolve --global` prints: the global tier's settings the pi package reads, which never depend on
/// a project. Today `[development]` from the host file.
#[derive(Debug, Serialize)]
pub struct GlobalResolved {
    pub global: PathBuf,
    #[serde(serialize_with = "section")]
    pub development: Section<DevelopmentTable>,
    /// The host file's candidates: clients cache the result until one of them changes.
    pub watch: Vec<PathBuf>,
}

pub fn resolve_global(paths: &Paths) -> GlobalResolved {
    let host_files = paths.global_files(Scope::Host);
    GlobalResolved {
        global: paths.global.clone(),
        development: host(paths).map(|host| host.development),
        watch: host_files.paths().map(|(_, path)| path.to_path_buf()).collect(),
    }
}

/// The paths of this process, with `config_home` (`--config-home`) as the global tier when given.
pub fn paths(config_home: Option<&Path>) -> Paths {
    let mut paths = Paths::current();
    if let Some(dir) = config_home {
        paths.global = dir.to_path_buf();
    }
    paths
}

/// The daemon's options from the host file.
pub fn daemon(config_home: Option<&Path>) -> Result<DaemonConfig, ConfigError> {
    let paths = paths(config_home);
    Ok(DaemonConfig::new(&host(&paths)?, paths))
}

/// The daemon's options as far as the host file can be read, defaults otherwise. For `piton setup`, whose job is to
/// report a broken configuration rather than refuse to start over it.
pub fn daemon_lenient(config_home: Option<&Path>) -> DaemonConfig {
    let paths = paths(config_home);
    DaemonConfig::new(&host(&paths).unwrap_or_default(), paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::LaunchMode;

    #[test]
    fn the_host_file_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("config.host.toml"),
            "socket = \"/tmp/s.sock\"\nroots = [\"~/p\"]\ndefault_attach_mode = \"host\"\nlogging = \"debug\"\n[stop]\ngrace_ms = 5\n[sandbox]\ncommand = [\"/bin/nono\"]\n",
        )
        .unwrap();
        std::fs::write(
            global.join("config.host.local.toml"),
            "keep_awake = false\n[stop]\nterm_grace_ms = 7\n",
        )
        .unwrap();
        let config = daemon(Some(&global)).unwrap();
        assert_eq!(config.socket, PathBuf::from("/tmp/s.sock"));
        assert_eq!((config.stop.grace_ms, config.stop.term_grace_ms), (5, 7));
        assert_eq!(config.sandbox.command, ["/bin/nono"]);
        assert_eq!(config.roots, ["~/p"]);
        assert_eq!(config.default_attach_mode, LaunchMode::Host);
        assert_eq!(config.logging.mode, crate::diagnostics::Mode::Debug);
        assert!(!config.keep_awake);
        assert_eq!(config.pi.command, ["pi"]);
        assert_eq!(config.empty_grace_ms, 10_000);
    }

    /// Each global file stands alone: a broken client file or global `config.toml` doesn't stop the daemon.
    #[test]
    fn only_the_host_file_stops_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("config.client.toml"), "text_font = 1\n").unwrap();
        std::fs::write(global.join("config.toml"), "[workers]\nskip = [\"../x\"]\n").unwrap();
        std::fs::write(global.join("config.host.toml"), "[stop]\ngrace_ms = 5\n").unwrap();
        assert_eq!(daemon(Some(&global)).unwrap().stop.grace_ms, 5);
        let all = super::global(&paths(Some(&global)));
        assert!(all.client.is_err());
        assert!(all.project.unwrap().workers.is_err());
        std::fs::write(global.join("config.host.toml"), "[tailnet]\nprot = 1\n").unwrap();
        assert!(daemon(Some(&global)).is_err());
    }

    #[test]
    fn development_comes_from_the_host_file() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("config.host.toml"),
            "[development]\nsession_analysis = false\n",
        )
        .unwrap();
        let resolved = resolve_global(&paths(Some(&global)));
        assert_eq!(resolved.development.unwrap().session_analysis, Some(false));
        assert!(resolved.watch.contains(&global.join("config.host.local.toml")));
    }
}
