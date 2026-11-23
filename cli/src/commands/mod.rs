//! The `piton` command line: its arguments, and each command as the user runs it. `daemon` hands over to the daemon
//! (crate::daemon). `config`, `setup`, `state`, `service`, and `completion` work on files and the system directly; the
//! rest are clients of the daemon over its Unix socket.

mod attach;
mod config;
mod control;
mod logging;
mod sandbox;
mod sessions;
mod worker;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use usage_rs::Subcommands;

pub use self::{control::control, logging::logging_state};

/// Owns pi processes so sessions outlive their clients
#[derive(usage_rs::Cli)]
// Unknown flags are errors, as with clap (usage's default treats them as values).
#[usage(bin = "piton", version = crate::VERSION, unknown_flags = "error", completion)]
struct Cli {
    /// The global configuration folder (default $XDG_CONFIG_HOME/piton, else ~/.config/piton).
    #[usage(long, global, env = "PITON_CONFIG_HOME")]
    config_home: Option<PathBuf>,
    /// Daemon socket; overrides the config.
    #[usage(long, global, env = "PITON_SOCKET")]
    socket: Option<PathBuf>,
    #[usage(subcommand)]
    command: Cmd,
}

#[derive(Subcommands)]
enum Cmd {
    /// Run the daemon in the foreground.
    Daemon,
    /// Connect stdio to a new pi in the current directory (pi.nvim's connector).
    Attach,
    /// List live sessions.
    Ls {
        #[usage(long)]
        json: bool,
    },
    /// Start a session in a workspace.
    Start {
        /// Workspace (default: current directory).
        workspace: Option<PathBuf>,
        /// Run pi on the host instead of in the sandbox.
        #[usage(long)]
        host: bool,
        /// First prompt to send.
        #[usage(long)]
        prompt: Option<String>,
    },
    /// Stop a session: pi session id, session file, or piton key.
    Stop {
        #[usage(long)]
        session: String,
    },
    /// Manage the launchd user agent that runs the daemon.
    Service {
        #[usage(subcommand)]
        action: ServiceAction,
    },
    /// Create, list, and delete workers: copy-on-write clones of a checkout for agents.
    Worker {
        #[usage(subcommand)]
        action: WorkerAction,
        /// Print the daemon's JSON response (for scripts and editor tooling). Refusals exit 4.
        #[usage(long, global)]
        json: bool,
    },
    /// Switch host logging (through the daemon, until it restarts) and manage the paired-iPhone log collector.
    Logging {
        #[usage(subcommand)]
        action: LoggingAction,
    },
    /// Show the resolved configuration of a workspace (reads files; the daemon needn't run).
    Config {
        #[usage(subcommand)]
        action: ConfigAction,
    },
    /// Show a workspace's sandbox, and why a path is allowed or denied in it.
    Sandbox {
        #[usage(subcommand)]
        action: SandboxAction,
    },
    /// Apply the sandbox policy the daemon passes in the environment to this process, then run COMMAND (how the daemon
    /// starts a sandboxed pi).
    #[usage(name = "sandbox-exec", hide)]
    SandboxExec {
        #[usage(double_dash = "required")]
        command: Vec<String>,
    },
    /// Run COMMAND in the launch environment the daemon passes past fnox (how the daemon starts a host-mode pi behind
    /// fnox's credential proxy).
    #[usage(name = "launch-exec", hide)]
    LaunchExec {
        #[usage(double_dash = "required")]
        command: Vec<String>,
    },
    /// Tunnel stdin and stdout to HOST:PORT through the gate in `HTTPS_PROXY` (ssh's ProxyCommand in a sandbox with a
    /// filtered network).
    #[usage(name = "gate-connect", hide)]
    GateConnect { host: String, port: u16 },
    /// Check what this host needs (the Mac app's onboarding runs this), and do the steps piton can do itself.
    Setup {
        #[usage(subcommand)]
        action: SetupAction,
        /// Print JSON (for the Mac app).
        #[usage(long, global)]
        json: bool,
    },
    /// Move this host's state database (its identity, and later sessions and drafts) to a replacement Mac.
    State {
        #[usage(subcommand)]
        action: StateAction,
    },
    /// Print the tools a repository can configure for itself (environment managers, agents, editors) and the
    /// repository files each reads, as JSON (for the pi package's incoming-PR review).
    Tools,
    /// Print a shell completion script.
    Completion {
        #[usage(long, choices("bash", "zsh", "fish"))]
        shell: String,
    },
}

#[derive(Subcommands)]
enum SetupAction {
    /// Report every step: ok, missing (with its fix), or failed. Exits 0 when every step is ok, 3 otherwise.
    Check {
        /// The `piton` the CLI link points to (default: this binary; the Mac app passes the bundled one).
        #[usage(long)]
        binary: Option<PathBuf>,
        /// Where the CLI link goes (default ~/.local/bin/piton).
        #[usage(long)]
        link: Option<PathBuf>,
    },
    /// Do one step piton can do itself: configuration, cli, or state.
    Apply {
        step: String,
        #[usage(long)]
        binary: Option<PathBuf>,
        #[usage(long)]
        link: Option<PathBuf>,
    },
}

#[derive(Subcommands)]
enum StateAction {
    /// Write a copy of the state database to FILE (which must not exist). Safe while the daemon runs.
    Export { file: PathBuf },
    /// Replace the state database with FILE, keeping its host identity. Stop the daemon first
    /// (`piton service uninstall`), then install it again.
    Import {
        file: PathBuf,
        /// Replace a database that already holds state.
        #[usage(long)]
        force: bool,
    },
}

#[derive(Subcommands)]
enum LoggingAction {
    /// Show the daemon's logging policy, its configured default, and the log directory.
    Status,
    /// Info level: no detailed traces or phone collection. Retained logs are not deleted.
    Info,
    /// Debug level (detailed traces, phone collection) for an hour, or until switched back or the daemon restarts.
    Debug {
        #[usage(long)]
        until_disabled: bool,
    },
    /// Run the paired-iPhone collector in the foreground (host only).
    Collect {
        #[usage(long)]
        device: String,
    },
    /// Install the independent collector LaunchAgent (host only).
    Install {
        #[usage(long)]
        device: String,
    },
    /// Stop and remove the collector LaunchAgent (host only).
    Uninstall,
}

#[derive(Subcommands)]
enum ConfigAction {
    /// Print everything resolved for the workspace as JSON (for the pi package).
    Resolve {
        /// Workspace (default: current directory).
        #[usage(long)]
        workspace: Option<PathBuf>,
        /// host or sandbox (default: from PITON_RUNTIME).
        #[usage(long)]
        runtime: Option<String>,
        /// Only the global tier's settings for the pi package that never depend on a project (`[development]` from
        /// `config.host.toml`).
        #[usage(long)]
        global: bool,
    },
    /// Print one resolved part as JSON, by its name in `config resolve`: a table (sandbox, workers, permissions,
    /// branches, tracker, forge, web_search), mcp, mcp_global, policies, or instructions.
    Get {
        part: String,
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
    /// Set KEY (dotted, like `branches.format`) in the repository's `.piton/config.toml`, the global tier's with
    /// --global, or the global `config.host.toml` / `config.client.toml` with --scope host|client; the `.local` file of
    /// the pair with --local. VALUE is JSON (an object becomes a table); text that isn't JSON is a string. With
    /// --changes, KEY is instead a JSON object of keys and values written together (`null` removes a key). With
    /// --dry-run, prints the file's new content as JSON and writes nothing (the pi package writes protected files
    /// through the daemon).
    Set {
        key: String,
        value: Option<String>,
        #[usage(long)]
        changes: bool,
        #[usage(long)]
        workspace: Option<PathBuf>,
        #[usage(long)]
        global: bool,
        /// host or client: one of the global tier's own files.
        #[usage(long)]
        scope: Option<String>,
        #[usage(long)]
        local: bool,
        /// Remove KEY instead.
        #[usage(long)]
        unset: bool,
        #[usage(long)]
        dry_run: bool,
    },
    /// Validate the workspace's configuration, list the files it reads, and warn about a path under another user's home
    /// or a secret in a file Git tracks.
    Check {
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
    /// Whether PATH is a protected file of the workspace, as JSON with `protected: true|false`. Every write to one asks
    /// the user: through the daemon in sessions it launched, in pi itself outside piton. `piton sandbox show` prints
    /// the sandbox rules that also deny the writes.
    Protected {
        path: PathBuf,
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
}

#[derive(Subcommands)]
enum SandboxAction {
    /// Print the sandbox a launch in the workspace gets, as JSON: every rule in order, the last match deciding.
    Show {
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
    /// Whether a sandboxed pi in the workspace may read, write, connect to, or create a socket at PATH, or look up the
    /// Mach service PATH names (`--service`), and which rule decides.
    Why {
        path: PathBuf,
        /// The access checked: one of the flags, or reading when none is given.
        #[usage(arg_group)]
        access: Option<WhyAccess>,
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
}

/// The access `piton sandbox why` checks besides reading: one at most.
#[derive(usage_rs::ArgGroup)]
#[usage(name = "access")]
pub enum WhyAccess {
    /// Check writing (default: reading).
    Write,
    /// Check connecting: to a Unix socket (PATH), or to a host or port (`host:port`, `localhost:PORT`).
    Connect,
    /// Check creating a Unix socket.
    Bind,
    /// Check looking up a Mach service: PATH is the service's name.
    Service,
}

#[derive(Subcommands)]
enum WorkerAction {
    /// Clone the source of FROM into `<parent>/<repo>--<name>`.
    Create {
        name: String,
        /// The source checkout or any of its workers (default: current directory).
        #[usage(long)]
        from: Option<PathBuf>,
        #[usage(long)]
        label: Option<String>,
        #[usage(long)]
        issue: Option<String>,
        /// Approve this commit's changes to protected files: Piton writes them, the rest of the update runs in the sandbox.
        #[usage(long, value_name = "SHA")]
        approve_source_update: Option<String>,
    },
    /// List the workers of FROM's source, with their live sessions.
    Ls {
        #[usage(long)]
        from: Option<PathBuf>,
    },
    /// Stop the worker's sessions and delete it.
    Delete {
        path: PathBuf,
        /// Delete despite uncommitted changes or unpushed commits.
        #[usage(long)]
        force: bool,
        /// What happens to the source when its update changes protected files (default: refuse, so you can decide).
        #[usage(arg_group)]
        source_update: Option<SourceUpdateFlag>,
    },
}

/// The answer to a source update that changes protected files: one of the two, or neither.
#[derive(usage_rs::ArgGroup)]
#[usage(name = "source-update")]
enum SourceUpdateFlag {
    /// Approve this commit's changes to protected files: Piton writes them, the rest of the update runs in the sandbox.
    #[usage(long = "approve-source-update", value_name = "SHA")]
    ApproveSourceUpdate(String),
    /// Leave the source as it is.
    #[usage(long = "skip-source-update")]
    SkipSourceUpdate,
}

#[derive(Subcommands)]
enum ServiceAction {
    /// Run this binary's daemon at login and restart it if it crashes (replaces a loaded agent).
    Install,
    /// Stop the agent and remove it.
    Uninstall,
    /// Show launchd's view of the agent.
    Status,
}

/// Parses the command line and runs the command.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    // Commands that need neither the config nor the runtime.
    match &cli.command {
        Cmd::Service { action } => return service(action, cli.config_home.as_deref()),
        Cmd::Config { action } => return config::run(action),
        Cmd::SandboxExec { command } => {
            let command: Vec<std::ffi::OsString> = command.iter().map(Into::into).collect();
            let error = crate::launch::sandbox::exec(&command);
            eprintln!("piton sandbox-exec: {error}");
            return ExitCode::from(126);
        }
        Cmd::LaunchExec { command } => {
            let command: Vec<std::ffi::OsString> = command.iter().map(Into::into).collect();
            let error = crate::launch::handoff::exec(&command);
            eprintln!("piton launch-exec: {error}");
            return ExitCode::from(126);
        }
        Cmd::GateConnect { host, port } => {
            return match crate::gate::ssh::connect(host, *port) {
                Ok(()) => ExitCode::SUCCESS,
                Err(why) => {
                    eprintln!("piton: {host}:{port}: {why}");
                    ExitCode::from(255)
                }
            };
        }
        Cmd::Tools => {
            println!(
                "{}",
                serde_json::to_string(&crate::project::tools::registry()).unwrap_or_default()
            );
            return ExitCode::SUCCESS;
        }
        Cmd::Completion { shell } => {
            let shell = match shell.as_str() {
                "bash" => usage_rs::complete::Shell::Bash,
                "zsh" => usage_rs::complete::Shell::Zsh,
                _ => usage_rs::complete::Shell::Fish,
            };
            print!("{}", Cli::completion_script(shell));
            return ExitCode::SUCCESS;
        }
        Cmd::Daemon
        | Cmd::Attach
        | Cmd::Ls { .. }
        | Cmd::Start { .. }
        | Cmd::Stop { .. }
        | Cmd::Worker { .. }
        | Cmd::State { .. }
        | Cmd::Setup { .. }
        | Cmd::Sandbox { .. }
        | Cmd::Logging { .. } => {}
    }
    // Setup reports a broken configuration (its configuration step) instead of refusing to start over it.
    let loaded = match &cli.command {
        Cmd::Setup { .. } => Ok(crate::config::daemon_lenient(cli.config_home.as_deref())),
        Cmd::Daemon
        | Cmd::Attach
        | Cmd::Ls { .. }
        | Cmd::Start { .. }
        | Cmd::Stop { .. }
        | Cmd::Worker { .. }
        | Cmd::State { .. }
        | Cmd::Logging { .. }
        | Cmd::Sandbox { .. }
        | Cmd::Service { .. }
        | Cmd::Config { .. }
        | Cmd::SandboxExec { .. }
        | Cmd::LaunchExec { .. }
        | Cmd::GateConnect { .. }
        | Cmd::Tools
        | Cmd::Completion { .. } => crate::config::daemon(cli.config_home.as_deref()),
    };
    let config = match loaded {
        Ok(mut config) => match (cli.socket, crate::config::Build::CURRENT) {
            (Some(socket), crate::config::Build::Debug) => {
                config.socket = socket;
                config
            }
            (Some(socket), crate::config::Build::Release) => {
                eprintln!(
                    "piton: --socket / PITON_SOCKET ({}) is for tests and development: this release build always uses {}",
                    socket.display(),
                    crate::config::default_socket(&config.paths.home).display()
                );
                return ExitCode::from(2);
            }
            (None, crate::config::Build::Debug | crate::config::Build::Release) => config,
        },
        Err(e) => {
            crate::logging::startup_error("configuration_failed");
            eprintln!("piton: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            crate::logging::startup_error("runtime_initialization_failed");
            eprintln!("piton: {e}");
            return ExitCode::from(2);
        }
    };
    let code = runtime.block_on(run(cli.command, config));
    crate::logging::flush();
    // `piton attach` may still be blocked reading stdin on a blocking thread; don't wait for it.
    runtime.shutdown_background();
    code
}

async fn run(command: Cmd, config: crate::config::DaemonConfig) -> ExitCode {
    match command {
        Cmd::Service { .. }
        | Cmd::Completion { .. }
        | Cmd::Config { .. }
        | Cmd::SandboxExec { .. }
        | Cmd::LaunchExec { .. }
        | Cmd::GateConnect { .. }
        | Cmd::Tools => {
            unreachable!("handled before the runtime starts")
        }
        Cmd::Sandbox { action } => sandbox::run(action, &config).await,
        Cmd::Daemon => match crate::daemon::run(config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                crate::log!("daemon startup failed: {e}");
                ExitCode::from(2)
            }
        },
        Cmd::Attach => attach::run(&config.socket).await,
        Cmd::Ls { json } => sessions::ls(&config, json).await,
        Cmd::Start {
            workspace,
            host,
            prompt,
        } => sessions::start(&config, workspace, host, prompt).await,
        Cmd::Stop { session } => sessions::stop(&config, session).await,
        Cmd::Worker { action, json } => worker::run(&config, action, json).await,
        Cmd::Logging { action } => logging::run(action, &config).await,
        Cmd::State { action } => state(action, &config).await,
        Cmd::Setup { action, json } => setup(action, json, &config).await,
    }
}

async fn setup(action: SetupAction, json: bool, config: &crate::config::DaemonConfig) -> ExitCode {
    let (binary, link) = match &action {
        SetupAction::Check { binary, link } | SetupAction::Apply { binary, link, .. } => (binary.clone(), link.clone()),
    };
    let binary = binary
        .or_else(|| std::env::current_exe().and_then(std::fs::canonicalize).ok())
        .unwrap_or_else(|| PathBuf::from("piton"));
    let link = link.unwrap_or_else(|| config.paths.home.join(".local/bin/piton"));
    let env: crate::launch::Env = match crate::launch::login_shell_env().await {
        Some(env) => env,
        None => std::env::vars().collect(),
    };
    let tailnet = crate::setup::TailnetCheck::run(config, &env).await;
    let host = crate::setup::Host {
        config,
        env: &env,
        binary,
        link,
        daemon_running: tokio::net::UnixStream::connect(&config.socket).await.is_ok(),
        tailnet,
    };
    match action {
        SetupAction::Check { .. } => {
            let report = crate::setup::check(&host);
            match json {
                true => println!("{}", serde_json::to_string(&report).unwrap_or_default()),
                false => print!("{report}"),
            }
            let all_ok = report
                .steps
                .iter()
                .all(|step| matches!(step.status, crate::setup::Status::Ok { .. }));
            if all_ok { ExitCode::SUCCESS } else { ExitCode::from(3) }
        }
        SetupAction::Apply { step, .. } => match crate::setup::apply(&host, &step) {
            Ok(report) => {
                match json {
                    true => println!("{}", serde_json::to_string(&report).unwrap_or_default()),
                    false => print!("{report}"),
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                match json {
                    true => println!(
                        "{}",
                        serde_json::json!({"format": crate::setup::FORMAT, "error": e.to_string()})
                    ),
                    false => eprintln!("piton: {e}"),
                }
                ExitCode::from(1)
            }
        },
    }
}

async fn state(action: StateAction, config: &crate::config::DaemonConfig) -> ExitCode {
    let db = config.state_db();
    let result = match action {
        StateAction::Export { file } => {
            crate::state::export(&db, &file).map(|()| format!("exported {} to {}", db.display(), file.display()))
        }
        StateAction::Import { file, force } => {
            // The daemon holds the database open; replacing it under a running daemon would lose the swap.
            if tokio::net::UnixStream::connect(&config.socket).await.is_ok() {
                eprintln!(
                    "piton: the daemon is running on {}; stop it first (`piton service uninstall`)",
                    config.socket.display()
                );
                return ExitCode::from(1);
            }
            crate::state::import(&file, &db, force)
                .map(|()| format!("imported {} into {}", file.display(), db.display()))
        }
    };
    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("piton: {e}");
            ExitCode::from(1)
        }
    }
}

fn service(action: &ServiceAction, config: Option<&Path>) -> ExitCode {
    let result = match action {
        ServiceAction::Install => crate::setup::install_service(config).map(|path| {
            println!("installed {} ({})", crate::setup::SERVICE_LABEL, path.display());
        }),
        ServiceAction::Uninstall => crate::setup::uninstall_service().map(|path| {
            println!("removed {} ({})", crate::setup::SERVICE_LABEL, path.display());
        }),
        ServiceAction::Status => crate::setup::service_status().map(|text| print!("{text}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("piton: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn why(flags: &[&str]) -> Result<Option<WhyAccess>, String> {
        let argv: Vec<&std::ffi::OsStr> = ["piton", "sandbox", "why"]
            .iter()
            .chain(flags)
            .chain(&["/x"])
            .map(std::ffi::OsStr::new)
            .collect();
        let Cmd::Sandbox {
            action: SandboxAction::Why { access, .. },
        } = Cli::try_parse_from(&argv).map_err(|e| format!("{e:?}"))?.command
        else {
            return Err("not sandbox why".into());
        };
        Ok(access)
    }

    /// `sandbox why` checks one access: reading by default, or the one flag given; two are refused by the parser.
    #[test]
    fn sandbox_why_takes_one_access() {
        assert!(matches!(why(&[]), Ok(None)));
        assert!(matches!(why(&["--write"]), Ok(Some(WhyAccess::Write))));
        assert!(matches!(why(&["--connect"]), Ok(Some(WhyAccess::Connect))));
        assert!(matches!(why(&["--bind"]), Ok(Some(WhyAccess::Bind))));
        assert!(matches!(why(&["--service"]), Ok(Some(WhyAccess::Service))));
        for pair in [["--write", "--bind"], ["--connect", "--write"], ["--bind", "--connect"]] {
            assert!(why(&pair).is_err(), "{pair:?}");
        }
    }
}
