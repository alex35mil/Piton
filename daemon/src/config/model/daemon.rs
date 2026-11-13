//! The global tier's tables as one value, and the daemon's options: those tables with the daemon's defaults applied.

use std::path::{Path, PathBuf};

use super::paths::{Paths, expand_home};
use super::tables::{ClientTable, HostTable, RunTable, SandboxTable, WorkersTable};
use crate::wire::LaunchMode;

/// The global tables the daemon runs with, merged (`config.local.toml` over `config.toml`), as written: absent keys
/// stay absent. Only these three can stop the daemon from starting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DaemonTables {
    pub run: RunTable,
    pub sandbox: SandboxTable,
    pub host: HostTable,
}

/// Every global table the apps' settings list shows: the daemon's, plus `[workers]` and `[client]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlobalConfig {
    pub daemon: DaemonTables,
    pub workers: WorkersTable,
    pub client: ClientTable,
}

/// How the daemon runs: the global tables with defaults applied.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
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
    /// Where configuration lives: home and the global tier.
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

impl DaemonConfig {
    /// The defaults, overridden by the global tables.
    pub fn new(tables: &DaemonTables, paths: Paths) -> Self {
        let home = paths.home.clone();
        let DaemonTables { run, sandbox, host } = tables.clone();
        Self {
            socket: run
                .socket
                .map(|s| expand_home(&s, &home))
                .unwrap_or_else(|| home.join(".local/state/piton/piton.sock")),
            log: Some(
                run.log
                    .map(|l| expand_home(&l, &home))
                    .unwrap_or_else(|| crate::diagnostics::root().join("daemon/current.log")),
            ),
            attach_mode: host.attach_mode.unwrap_or(LaunchMode::Sandbox),
            pi: PiConfig {
                command: run.pi.command.unwrap_or_else(|| vec!["pi".into()]),
                args: run.pi.args.unwrap_or_default(),
            },
            sandbox: SandboxConfig {
                command: sandbox.command.unwrap_or_else(|| vec!["nono".into()]),
            },
            env: EnvConfig {
                login_shell: run.env.login_shell.unwrap_or(true),
                mise: run.env.mise.unwrap_or(true),
            },
            stop: StopConfig {
                grace_ms: run.stop.grace_ms.unwrap_or(15_000),
                term_grace_ms: run.stop.term_grace_ms.unwrap_or(3_000),
            },
            empty_grace_ms: host.empty_grace_ms.unwrap_or(10_000),
            tailnet: TailnetConfig {
                enabled: run.tailnet.enabled.unwrap_or(true),
                port: run.tailnet.port.unwrap_or(7357),
                cli: run.tailnet.cli.unwrap_or_else(|| "tailscale".into()),
                heartbeat_ms: run.tailnet.heartbeat_ms.unwrap_or(10_000),
                retry_ms: run.tailnet.retry_ms.unwrap_or(10_000),
            },
            logging: LoggingConfig {
                mode: host.logging.unwrap_or_default(),
            },
            roots: host.roots.unwrap_or_default(),
            paths,
        }
    }

    /// Only defaults: no global file sets anything.
    pub fn defaults(paths: Paths) -> Self {
        Self::new(&DaemonTables::default(), paths)
    }

    /// Only defaults, for this user's home and its default global tier.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        let home = super::paths::home();
        Self::defaults(Paths {
            global: home.join(".config/piton"),
            home,
        })
    }

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
