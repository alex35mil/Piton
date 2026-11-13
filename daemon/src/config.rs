//! The daemon's own configuration, from the global tier (`$XDG_CONFIG_HOME/piton/`, or `--config-home` /
//! `PITON_CONFIG_HOME`): `daemon[.local].toml` says how the daemon runs, `settings[.local].toml` `[host]` holds the
//! preferences the apps change. Reading and merging is settings/'s; this turns the result into values with defaults.

use std::path::{Path, PathBuf};

use crate::settings::{DaemonFile, Paths, SettingsError, SettingsFile, expand_home as expand_with};
use crate::wire::LaunchMode;

#[derive(Debug, Clone)]
pub struct Config {
    /// Unix socket of the daemon. An override for tests and development: the Mac app always connects to the default
    /// (`Endpoint.defaultLocal` in PitonKit), so a daemon on another socket runs without it.
    pub socket: PathBuf,
    /// Daemon log file (appended). Lines also go to stderr.
    pub log: Option<PathBuf>,
    /// Mode for sessions launched by `piton attach`.
    pub attach_mode: LaunchMode,
    pub pi: PiConfig,
    pub sandbox: SandboxConfig,
    pub env: EnvConfig,
    pub stop: StopConfig,
    /// How long a pi with no messages, no run, and no pending dialog may stay without clients.
    pub empty_grace_ms: u64,
    pub tailnet: TailnetConfig,
    pub logging: LoggingConfig,
    /// Folders whose workspaces the native apps may browse and start sessions in. None by default: the apps can't
    /// start sessions until the owner lists some. `piton attach` and the CLI aren't limited. A leading `~/` is
    /// expanded.
    pub roots: Vec<String>,
    /// Where the settings chains are: home, the global tier, pi's agent folder.
    pub paths: Paths,
}

/// The `view` protocol over WebSocket on this Mac's tailnet addresses, for the owner's other devices.
#[derive(Debug, Clone)]
pub struct TailnetConfig {
    pub enabled: bool,
    pub port: u16,
    /// The tailscale CLI.
    pub cli: String,
    /// How often to ping each client; one that answers none of three pings in a row is dropped.
    pub heartbeat_ms: u64,
    /// While Tailscale isn't up (not started yet at login, logged out), how often to try again to listen.
    pub retry_ms: u64,
}

/// Host logging: the daemon, the Mac app, and the phone-log collector. The apps and `piton logging` switch it at runtime
/// until the daemon restarts; this is what it starts with and returns to when a timed switch expires.
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    pub mode: crate::diagnostics::Mode,
}

#[derive(Debug, Clone)]
pub struct PiConfig {
    /// pi executable and leading arguments; piton adds `--mode rpc`, then `args`, then session arguments.
    pub command: Vec<String>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// nono executable. Grants come from each launch's `[sandbox]` chain (sandbox.rs).
    pub command: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EnvConfig {
    /// Start from the login shell's environment (captured once at daemon start) instead of the daemon's own.
    pub login_shell: bool,
    /// Add `mise env -C <workspace> --json` for every launch.
    pub mise: bool,
}

#[derive(Debug, Clone)]
pub struct StopConfig {
    /// After closing pi's stdin, how long pi may take before SIGTERM.
    pub grace_ms: u64,
    /// After SIGTERM, how long before SIGKILL.
    pub term_grace_ms: u64,
}

impl Config {
    /// The defaults, overridden by the merged global files.
    pub fn from_files(daemon: DaemonFile, settings: SettingsFile, paths: Paths) -> Self {
        let home = paths.home.clone();
        let host = settings.host;
        Self {
            socket: daemon
                .socket
                .map(|s| expand_with(&s, &home))
                .unwrap_or_else(|| home.join(".local/state/piton/piton.sock")),
            log: Some(
                daemon
                    .log
                    .map(|l| expand_with(&l, &home))
                    .unwrap_or_else(|| crate::diagnostics::root().join("daemon/current.log")),
            ),
            attach_mode: host.attach_mode.unwrap_or(LaunchMode::Sandbox),
            pi: PiConfig {
                command: daemon.pi.command.unwrap_or_else(|| vec!["pi".into()]),
                args: daemon.pi.args.unwrap_or_default(),
            },
            sandbox: SandboxConfig {
                command: daemon.sandbox.command.unwrap_or_else(|| vec!["nono".into()]),
            },
            env: EnvConfig {
                login_shell: daemon.env.login_shell.unwrap_or(true),
                mise: daemon.env.mise.unwrap_or(true),
            },
            stop: StopConfig {
                grace_ms: daemon.stop.grace_ms.unwrap_or(15_000),
                term_grace_ms: daemon.stop.term_grace_ms.unwrap_or(3_000),
            },
            empty_grace_ms: host.empty_grace_ms.unwrap_or(10_000),
            tailnet: TailnetConfig {
                enabled: daemon.tailnet.enabled.unwrap_or(true),
                port: daemon.tailnet.port.unwrap_or(7357),
                cli: daemon.tailnet.cli.unwrap_or_else(|| "tailscale".into()),
                heartbeat_ms: daemon.tailnet.heartbeat_ms.unwrap_or(10_000),
                retry_ms: daemon.tailnet.retry_ms.unwrap_or(10_000),
            },
            logging: LoggingConfig {
                mode: host.logging.unwrap_or_default(),
            },
            roots: host.roots.unwrap_or_default(),
            paths,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let paths = Paths {
            home: home(),
            global: home().join(".config/piton"),
            pi_agent_dir: home().join(".pi/agent"),
        };
        Self::from_files(DaemonFile::default(), SettingsFile::default(), paths)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

/// Loads the global tier: `config_home` when given, else the one [`Paths::current`] finds.
pub fn load(config_home: Option<&Path>) -> Result<Config, ConfigError> {
    let mut paths = Paths::current();
    if let Some(dir) = config_home {
        paths.global = dir.to_path_buf();
    }
    let resolved = crate::settings::resolve_global(&paths)?;
    Ok(Config::from_files(resolved.daemon?, resolved.settings?, paths))
}

/// The global tier as far as it can be read: each part that resolves, defaults for the rest. For `piton setup`, whose
/// job is to report a broken configuration rather than refuse to start over it.
pub fn load_lenient(config_home: Option<&Path>) -> Config {
    let mut paths = Paths::current();
    if let Some(dir) = config_home {
        paths.global = dir.to_path_buf();
    }
    let (daemon, settings) = match crate::settings::resolve_global(&paths) {
        Ok(resolved) => (
            resolved.daemon.unwrap_or_default(),
            resolved.settings.unwrap_or_default(),
        ),
        Err(_) => (DaemonFile::default(), SettingsFile::default()),
    };
    Config::from_files(daemon, settings, paths)
}

impl Config {
    /// The daemon's state folder: next to its socket, outside every sandbox.
    pub fn state_dir(&self) -> PathBuf {
        self.socket
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.paths.home.join(".local/state/piton"))
    }

    /// The host's state database.
    pub fn state_db(&self) -> PathBuf {
        self.state_dir().join(crate::state::FILE)
    }

    /// The socket sandboxed pis reach the daemon on (see agent.rs).
    pub fn agent_socket(&self) -> PathBuf {
        self.state_dir().join(crate::agent::FILE)
    }
}

/// The user's home folder, from the user database (see settings::home).
pub fn home() -> PathBuf {
    crate::settings::home()
}

pub fn expand_home(path: &str) -> PathBuf {
    expand_with(path, &home())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut paths = Paths::current();
        paths.global = global;
        paths.pi_agent_dir = dir.path().join("agent");
        let resolved = crate::settings::resolve_global(&paths).unwrap();
        let config = Config::from_files(resolved.daemon.unwrap(), resolved.settings.unwrap(), paths);
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
