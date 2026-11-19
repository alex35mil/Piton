//! The daemon's options: the host file (`config.host[.local].toml`) with the daemon's defaults applied. Only the host
//! file can stop the daemon from starting.

use std::path::{Path, PathBuf};

use super::{
    host::HostFile,
    paths::{Paths, expand_home},
};
use crate::launch::LaunchMode;

/// How the daemon runs: the host file with defaults applied.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Unix socket of the daemon. Its folder is the state folder the sandbox closes. Moving it is for tests and
    /// development, in debug builds only ([`Build`]): the Mac app always connects to the default
    /// (`Endpoint.defaultLocal` in PitonKit), so a daemon on another socket runs without it.
    pub socket: PathBuf,
    /// A `socket` the host file sets that this build ignores: release builds always use the default.
    pub ignored_socket: Option<PathBuf>,
    /// Daemon log file (appended). Lines also go to stderr.
    pub log: Option<PathBuf>,
    /// Mode for sessions launched by `piton attach` when none is given.
    pub default_attach_mode: LaunchMode,
    /// Hold the Mac awake while sessions are live.
    pub keep_awake: bool,
    pub pi: PiConfig,
    pub env: EnvConfig,
    pub stop: StopConfig,
    /// How long a pi with no messages, no run, and no pending dialog may stay without clients.
    pub empty_grace_ms: u64,
    pub tailnet: TailnetConfig,
    /// The external tools piton runs; whether each is used depends on what's installed (`crate::tools`).
    pub tools: ToolsConfig,
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
    /// How often to ping each client; one that answers none of three pings in a row is dropped.
    pub heartbeat_ms: u64,
    /// While Tailscale isn't up (not started yet at login, logged out), how often to try again to listen.
    pub retry_ms: u64,
}

/// Host logging: the daemon, the Mac app, and the phone-log collector. The apps and `piton logging` switch it at runtime
/// until the daemon restarts; this is what it starts with and returns to when a timed switch expires.
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    pub mode: crate::logging::Mode,
}

#[derive(Debug, Clone)]
pub struct PiConfig {
    /// pi executable and leading arguments; piton adds `--mode rpc`, then `args`, then session arguments.
    pub command: Vec<String>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EnvConfig {
    /// Start from the login shell's environment (captured once at daemon start) instead of the daemon's own.
    pub login_shell: bool,
}

/// `[tools]`: one setting per external tool.
#[derive(Debug, Clone)]
pub struct ToolsConfig {
    pub mise: ToolConfig,
    pub fnox: ToolConfig,
    pub tailscale: ToolConfig,
}

#[derive(Debug, Clone)]
pub struct ToolConfig {
    pub choice: ToolChoice,
    /// The executable and leading arguments, found through the login shell's PATH.
    pub command: Vec<String>,
}

/// Whether to use a tool: `enabled` in the file, or unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    /// Unset: used when it's installed, off otherwise.
    Auto,
    /// `enabled = true`: used, and an error when it isn't installed.
    On,
    /// `enabled = false`.
    Off,
}

impl ToolConfig {
    fn new(table: super::host::ToolTable, name: &str) -> Self {
        Self {
            choice: match table.enabled {
                None => ToolChoice::Auto,
                Some(true) => ToolChoice::On,
                Some(false) => ToolChoice::Off,
            },
            command: table.command.unwrap_or_else(|| vec![name.into()]),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StopConfig {
    /// After closing pi's stdin, how long pi may take before SIGTERM.
    pub grace_ms: u64,
    /// After SIGTERM, how long before SIGKILL.
    pub term_grace_ms: u64,
}

/// The kind of build running: whether the daemon's socket may be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// `cargo build`, as the tests run it: the socket may be moved (`socket`, `--socket`, `PITON_SOCKET`).
    Debug,
    /// `cargo install`, the installed `piton`: the socket, and so the state folder the sandbox closes, is always the
    /// default. A moved socket would close its whole folder to every sandboxed session (`/tmp`, or the home folder).
    Release,
}

impl Build {
    pub const CURRENT: Build = match cfg!(debug_assertions) {
        true => Build::Debug,
        false => Build::Release,
    };
}

/// Where the daemon's socket is when nothing moves it.
pub fn default_socket(home: &Path) -> PathBuf {
    home.join(".local/state/piton/piton.sock")
}

impl DaemonConfig {
    /// The defaults, overridden by the host file, for this build.
    pub fn new(host: &HostFile, paths: Paths) -> Self {
        Self::for_build(host, paths, Build::CURRENT)
    }

    /// The defaults, overridden by the host file, as `build` reads it.
    pub fn for_build(host: &HostFile, paths: Paths, build: Build) -> Self {
        let home = paths.home.clone();
        let host = host.clone();
        let configured = host.socket.map(|s| expand_home(&s, &home));
        let (socket, ignored_socket) = match (build, configured) {
            (Build::Debug, Some(socket)) => (socket, None),
            (Build::Release, Some(socket)) => (default_socket(&home), Some(socket)),
            (Build::Debug | Build::Release, None) => (default_socket(&home), None),
        };
        Self {
            socket,
            ignored_socket,
            log: Some(
                host.log
                    .map(|l| expand_home(&l, &home))
                    .unwrap_or_else(|| crate::logging::root().join("daemon/current.log")),
            ),
            default_attach_mode: host.default_attach_mode.unwrap_or(LaunchMode::Sandbox),
            keep_awake: host.keep_awake.unwrap_or(true),
            pi: PiConfig {
                command: host.pi.command.unwrap_or_else(|| vec!["pi".into()]),
                args: host.pi.args.unwrap_or_default(),
            },
            env: EnvConfig {
                login_shell: host.env.login_shell.unwrap_or(true),
            },
            stop: StopConfig {
                grace_ms: host.stop.grace_ms.unwrap_or(15_000),
                term_grace_ms: host.stop.term_grace_ms.unwrap_or(3_000),
            },
            empty_grace_ms: host.empty_grace_ms.unwrap_or(10_000),
            tailnet: TailnetConfig {
                enabled: host.tailnet.enabled.unwrap_or(true),
                port: host.tailnet.port.unwrap_or(7357),
                heartbeat_ms: host.tailnet.heartbeat_ms.unwrap_or(10_000),
                retry_ms: host.tailnet.retry_ms.unwrap_or(10_000),
            },
            tools: ToolsConfig {
                mise: ToolConfig::new(host.tools.mise, "mise"),
                fnox: ToolConfig::new(host.tools.fnox, "fnox"),
                tailscale: ToolConfig::new(host.tools.tailscale, "tailscale"),
            },
            logging: LoggingConfig {
                mode: host.logging.unwrap_or_default(),
            },
            roots: host.roots.unwrap_or_default(),
            paths,
        }
    }

    /// Only defaults: no host file sets anything.
    pub fn defaults(paths: Paths) -> Self {
        Self::new(&HostFile::default(), paths)
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

    /// A host file's text with the defaults applied, for a fixed home.
    #[cfg(test)]
    pub fn from_host_toml(text: &str) -> Self {
        let host: HostFile = toml::from_str(text).expect("a valid host file");
        Self::new(
            &host,
            Paths {
                home: "/h".into(),
                global: "/h/.config/piton".into(),
            },
        )
    }

    /// The daemon's state folder: next to its socket, closed to every sandbox. Always `~/.local/state/piton` in a
    /// release build; a debug build's moved socket moves it too.
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

    /// The socket sandboxed pis reach the daemon on (see `agent/`).
    pub fn agent_socket(&self) -> PathBuf {
        self.state_dir().join(crate::agent::FILE)
    }

    /// What the daemon log and `piton config check` say when this build ignores the host file's `socket`.
    pub fn ignored_socket_note(&self) -> Option<String> {
        self.ignored_socket.as_ref().map(|ignored| {
            format!(
                "`socket` = {} in config.host.toml is for tests and development: this release build uses {}",
                ignored.display(),
                self.socket.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moved socket moves the state folder the sandbox closes: only a debug build (the tests') may do it.
    #[test]
    fn only_debug_builds_move_the_socket() {
        let host: HostFile = toml::from_str("socket = \"/tmp/x.sock\"\n").unwrap();
        let paths = || Paths {
            home: "/h".into(),
            global: "/h/.config/piton".into(),
        };
        let debug = DaemonConfig::for_build(&host, paths(), Build::Debug);
        assert_eq!(debug.socket, PathBuf::from("/tmp/x.sock"));
        assert_eq!(debug.ignored_socket, None);
        assert_eq!(debug.ignored_socket_note(), None);

        let release = DaemonConfig::for_build(&host, paths(), Build::Release);
        assert_eq!(release.socket, PathBuf::from("/h/.local/state/piton/piton.sock"));
        assert_eq!(release.state_dir(), PathBuf::from("/h/.local/state/piton"));
        assert_eq!(release.ignored_socket, Some(PathBuf::from("/tmp/x.sock")));
        assert!(release.ignored_socket_note().unwrap().contains("/tmp/x.sock"));

        let unset = DaemonConfig::for_build(&HostFile::default(), paths(), Build::Release);
        assert_eq!(unset.socket, PathBuf::from("/h/.local/state/piton/piton.sock"));
        assert_eq!(unset.ignored_socket, None);
    }
}
