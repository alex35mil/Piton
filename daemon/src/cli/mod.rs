//! The `piton` command line: its arguments, and each command as the user runs it. `daemon` hands over to the daemon
//! (daemon.rs); every other command is a client of it over the Unix socket, except `service` and `completion`.

mod attach;
mod config;
mod control;
mod logging;
mod sessions;
mod worker;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use usage_rs::Subcommands;

pub use control::control;
pub use logging::logging_state;

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
        /// Run pi on the host instead of in the nono sandbox.
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
    /// Print a shell completion script.
    Completion {
        #[usage(long, choices("bash", "zsh", "fish"))]
        shell: String,
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
        /// Only the global tier: personal settings that never depend on a project.
        #[usage(long)]
        global: bool,
    },
    /// Print one resolved file as JSON: integrations, workflows, permissions, mcp, daemon, settings.
    Get {
        file: String,
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
    /// Validate the workspace's configuration and list the files it reads.
    Check {
        #[usage(long)]
        workspace: Option<PathBuf>,
    },
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
    },
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
        | Cmd::Logging { .. } => {}
    }
    let config = match crate::config::load(cli.config_home.as_deref()) {
        Ok(mut config) => {
            if let Some(socket) = cli.socket {
                config.socket = socket;
            }
            config
        }
        Err(e) => {
            crate::log::startup_error("configuration_failed");
            eprintln!("piton: {e}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            crate::log::startup_error("runtime_initialization_failed");
            eprintln!("piton: {e}");
            return ExitCode::from(2);
        }
    };
    let code = runtime.block_on(run(cli.command, config));
    crate::log::flush();
    // `piton attach` may still be blocked reading stdin on a blocking thread; don't wait for it.
    runtime.shutdown_background();
    code
}

async fn run(command: Cmd, config: crate::config::Config) -> ExitCode {
    match command {
        Cmd::Service { .. } | Cmd::Completion { .. } | Cmd::Config { .. } => {
            unreachable!("handled before the runtime starts")
        }
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
    }
}

fn service(action: &ServiceAction, config: Option<&Path>) -> ExitCode {
    let result = match action {
        ServiceAction::Install => crate::service::install(config).map(|path| {
            println!("installed {} ({})", crate::service::LABEL, path.display());
        }),
        ServiceAction::Uninstall => crate::service::uninstall().map(|path| {
            println!("removed {} ({})", crate::service::LABEL, path.display());
        }),
        ServiceAction::Status => crate::service::status().map(|text| print!("{text}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("piton: {e}");
            ExitCode::from(1)
        }
    }
}
