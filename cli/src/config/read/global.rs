//! The global tier's own files, each read on its own: the host file (the daemon's options and this host's preferences),
//! the client file (the apps' preferences), and the global `config.toml` (the defaults every project inherits). Each
//! caller fails only on the files it reads: a broken client file stops the apps' settings list, never the daemon.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{
    parse::{FileOrigin, GlobalFile, read_global},
    resolve::{Resolved, Section, resolve_global_project, section},
};
use crate::config::{
    ConfigError,
    model::{
        client::ClientFile,
        daemon::DaemonConfig,
        host::{DebuggingTable, HostFile},
        level::LevelKind,
        paths::{Paths, Scope},
    },
};

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

/// The global tier's own files that exist (host and client), for `piton config check` to list and check for
/// portability. The global `config.toml` is part of every workspace's chain, so it's listed there.
pub fn own_files(paths: &Paths) -> Vec<FileOrigin> {
    [Scope::Host, Scope::Client]
        .into_iter()
        .flat_map(|scope| {
            paths
                .global_files(scope)
                .paths()
                .filter(|(_, path)| path.is_file())
                .map(|(_, path)| FileOrigin {
                    path: path.to_path_buf(),
                    level: LevelKind::Global,
                })
                .collect::<Vec<_>>()
        })
        .collect()
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
/// a project. Today `[debugging]` from the host file.
#[derive(Debug, Serialize)]
pub struct GlobalResolved {
    pub global: PathBuf,
    #[serde(serialize_with = "section")]
    pub debugging: Section<DebuggingTable>,
    /// The host file's candidates: clients cache the result until one of them changes.
    pub watch: Vec<PathBuf>,
}

pub fn resolve_global(paths: &Paths) -> GlobalResolved {
    let host_files = paths.global_files(Scope::Host);
    GlobalResolved {
        global: paths.global.clone(),
        debugging: host(paths).map(|host| host.debugging),
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
    use crate::{config::ConfigDuration, launch::LaunchMode};

    #[test]
    fn the_host_file_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("config.host.toml"),
            "socket = \"/tmp/s.sock\"\nroots = [\"~/p\"]\ndefault_attach_mode = \"host\"\n[stop]\nterminate_after = \"5ms\"\n[debugging]\nlog_level = \"debug\"\n",
        )
        .unwrap();
        std::fs::write(
            global.join("config.host.local.toml"),
            "keep_awake = false\n[stop]\nkill_after = \"7ms\"\n",
        )
        .unwrap();
        let config = daemon(Some(&global)).unwrap();
        assert_eq!(config.socket, PathBuf::from("/tmp/s.sock"));
        assert_eq!(
            (config.stop.terminate_after, config.stop.kill_after),
            (ConfigDuration::from_millis(5), ConfigDuration::from_millis(7))
        );
        assert_eq!(config.roots, ["~/p"]);
        assert_eq!(config.default_attach_mode, LaunchMode::Host);
        assert_eq!(config.logging.mode, crate::logging::Mode::Debug);
        assert!(!config.keep_awake);
        assert_eq!(config.pi.command, ["pi"]);
        assert_eq!(config.stop_empty_session_after, ConfigDuration::from_millis(10_000));
        assert_eq!(
            config.approval_timeout,
            crate::config::Timeout::After(ConfigDuration::from_millis(600_000))
        );
        assert_eq!(config.integrations.tailnet.port, 7357);
    }

    /// Each global file stands alone: a broken client file or global `config.toml` doesn't stop the daemon.
    #[test]
    fn only_the_host_file_stops_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("config.client.toml"), "text_font = 1\n").unwrap();
        std::fs::write(global.join("config.toml"), "[workers]\nskip = [\"../x\"]\n").unwrap();
        std::fs::write(global.join("config.host.toml"), "[stop]\nterminate_after = \"5s\"\n").unwrap();
        assert_eq!(
            daemon(Some(&global)).unwrap().stop.terminate_after,
            ConfigDuration::from_millis(5_000)
        );
        let all = super::global(&paths(Some(&global)));
        assert!(all.client.is_err());
        assert!(all.project.unwrap().workers.is_err());
        std::fs::write(global.join("config.host.toml"), "[integrations.tailscale]\nprot = 1\n").unwrap();
        assert!(daemon(Some(&global)).is_err());
    }

    /// `piton config check` lists the host and client files and checks them for portability like the chain's.
    #[test]
    fn own_files_are_listed_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("users/me");
        let global = home.join(".config/piton");
        std::fs::create_dir_all(&global).unwrap();
        let host_file = global.join("config.host.toml");
        let other = root.join("users/other/Projects");
        std::fs::write(&host_file, format!("roots = [\"{}\"]\n", other.display())).unwrap();
        std::fs::write(global.join("config.client.toml"), "text_font = \"Inter\"\n").unwrap();
        let paths = Paths {
            home: home.clone(),
            global: global.clone(),
        };
        let files = own_files(&paths);
        assert_eq!(
            files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            [host_file.clone(), global.join("config.client.toml")]
        );
        let warnings = crate::config::portability_warnings(&files, &home);
        assert!(
            matches!(&warnings[..], [crate::config::check::portability::Warning::ForeignHome { path, key, .. }] if *path == host_file && key == "roots"),
            "{warnings:?}"
        );
    }

    #[test]
    fn debugging_comes_from_the_host_file() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("piton");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("config.host.toml"),
            "[debugging]\nsession_analysis = false\n",
        )
        .unwrap();
        let resolved = resolve_global(&paths(Some(&global)));
        assert_eq!(resolved.debugging.unwrap().session_analysis, Some(false));
        assert!(resolved.watch.contains(&global.join("config.host.local.toml")));
    }
}
