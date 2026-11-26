//! The running daemon: startup and shutdown, the registry of live sessions, and reloading the global configuration.
//!
//! - [`socket`]: the Unix socket; a hello picks `rpc`, `control`, or the apps' `view` protocol.
//! - [`control`]: control requests.
//! - [`rpc`]: `piton attach` connections.
//! - [`protocol`]: the local socket's messages, shared with the CLI.
//! - [`awake`], [`watch`]: keep-awake, and reloading on file changes.

mod awake;
mod control;
mod protocol;
mod refusals;
mod rpc;
mod socket;
mod watch;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::{
    net::{UnixListener, UnixStream},
    sync::Notify,
};

pub use self::{
    protocol::{
        AttachControl, ControlRequest, ControlResponse, EndReason, FRAME_CONTROL, FRAME_RECORD, Hello, PROTOCOL_VERSION,
    },
    refusals::Refusal,
};
use crate::{
    config::DaemonConfig,
    launch::{self, Env, LaunchError, LaunchMode, SessionArg},
    project::{Relation, Workspace},
    session::{self, Hooks, LaunchSpec, SessionHandle, SessionKey, StopReason, lock},
};

pub struct Daemon {
    /// The configuration in effect: replaced when the global files change. Each launch, session, and request takes
    /// the current one; keys that need a restart are read once at start from `started`.
    config: std::sync::RwLock<Arc<DaemonConfig>>,
    /// The global files as the apps see them: values, where each is set, and keys waiting for a restart.
    app_settings: Mutex<crate::apps::SettingsSnapshot>,
    base_env: Env,
    /// The external programs and whether each is used, decided at start from `[integrations]` and the login shell's
    /// PATH.
    integrations: crate::integrations::Integrations,
    sessions: Mutex<BTreeMap<SessionKey, SessionHandle>>,
    next_key: AtomicU64,
    next_client: AtomicU64,
    status_changed: Arc<Notify>,
    /// Fires whenever the session list (membership, status, clients, identity, title) may have changed.
    list_changed: tokio::sync::broadcast::Sender<()>,
    /// This host's identity, from the state database.
    host_id: String,
    /// The state database, open for the daemon's life: trust decisions are read at every launch and written by clients.
    state: Mutex<crate::state::State>,
    /// Fires when host logging changes, so the expiry task re-arms.
    logging_changed: Arc<Notify>,
    /// Every live token for `agent.sock`: sessions', calls', and launch helpers'.
    agent_tokens: Arc<crate::agent::Tokens>,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another piton daemon is listening on {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket {path}: {source}")]
    Socket { path: PathBuf, source: std::io::Error },
    #[error("state database: {0}")]
    State(#[from] crate::state::StateError),
}

/// Whether a launch asks the user about trust when a decision is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustCheck {
    /// Refuse with [`LaunchError::TrustNeeded`], so the client asks: every launch a person started.
    Ask,
    /// Launch with what was decided (a restart the daemon starts on its own).
    KeepDecided,
}

/// Why a trust decision can't be recorded or read.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("{0}")]
    Workspace(String),
    #[error(transparent)]
    Decide(crate::trust::DecideError),
}

pub async fn run(config: DaemonConfig) -> Result<(), DaemonError> {
    let socket = config.socket.clone();
    crate::logging::set_policy(crate::logging::State {
        policy: config.logging.mode.policy(),
        default: config.logging.mode,
    });
    if let Some(path) = &config.log
        && let Err(e) = crate::logging::init(path)
    {
        eprintln!("piton: can't open log {}: {e}", path.display());
    }
    // First line of every start, so the log shows which build was running when.
    crate::log!("piton {} starting (pid {})", crate::VERSION, std::process::id());
    if let Some(note) = config.ignored_socket_note() {
        crate::log!("warning: {note}");
    }
    if UnixStream::connect(&socket).await.is_ok() {
        return Err(DaemonError::AlreadyRunning(socket));
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).map_err(|source| DaemonError::Socket {
            path: socket.clone(),
            source,
        })?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(|source| DaemonError::Socket {
        path: socket.clone(),
        source,
    })?;
    set_owner_only(&socket);
    let agent_socket = config.agent_socket();
    let agent_listener = crate::agent::bind(&agent_socket).map_err(|source| DaemonError::Socket {
        path: agent_socket.clone(),
        source,
    })?;

    let mut base_env: Env = match config.env.login_shell {
        true => match launch::login_shell_env().await {
            Some(env) => env,
            None => {
                crate::log!("couldn't capture the login shell environment; using the daemon's own");
                std::env::vars().collect()
            }
        },
        false => std::env::vars().collect(),
    };
    // The launchd marker describes this process, not pi's: a piton run inside a session (tests, the CLI) would
    // otherwise think it's the service and stop writing to stderr.
    base_env.remove("PITON_SERVICE");
    // What pi will be launched with: if it's missing here, sessions fail to start.
    let pi = config.pi.command.first().map(String::as_str).unwrap_or("pi");
    let pi_found = match launch::resolve_program(pi, &base_env) {
        resolved if resolved.contains('/') => format!("{pi}={resolved}"),
        _ => format!("{pi}=NOT FOUND"),
    };
    let integrations = crate::integrations::Integrations::resolve(&config, &base_env);
    // pi's own agent dir: unset means pi's default (~/.pi/agent), not the user's usual setup.
    let agent_dir = base_env
        .get("PI_CODING_AGENT_DIR")
        .map_or("unset (pi's default)", String::as_str);
    crate::log!(
        "environment: {} variables; {pi_found}, {}; PI_CODING_AGENT_DIR={agent_dir}",
        base_env.len(),
        integrations.describe()
    );

    // Socket folders of sessions that ended without their hook running (a crash, a kill).
    launch::claude_sockets::sweep(base_env.get("TMPDIR").map(Path::new), &config.state_dir());
    let state = crate::state::State::open(&config.state_db())?;
    let host_id = state.host_id()?;
    crate::log!("state: {} (host {host_id})", config.state_db().display());
    let app_settings = crate::apps::SettingsSnapshot::new(&config.paths, &integrations);
    let daemon = Arc::new(Daemon {
        config: std::sync::RwLock::new(Arc::new(config)),
        app_settings: Mutex::new(app_settings),
        base_env,
        integrations,
        sessions: Mutex::new(BTreeMap::new()),
        next_key: AtomicU64::new(first_session_key()),
        next_client: AtomicU64::new(1),
        status_changed: Arc::new(Notify::new()),
        list_changed: tokio::sync::broadcast::channel(64).0,
        host_id,
        state: Mutex::new(state),
        logging_changed: Arc::new(Notify::new()),
        agent_tokens: Arc::new(crate::agent::Tokens::default()),
    });
    tokio::spawn(crate::daemon::awake::run(daemon.clone()));
    tokio::spawn(logging_expiry(daemon.clone()));
    crate::daemon::watch::spawn(daemon.clone());
    tokio::spawn(crate::agent::serve(daemon.clone(), agent_listener));
    tokio::spawn(refusals::watch(daemon.clone()));
    let tailnet = daemon.config().integrations.tailnet.clone();
    // Remote access is optional: the apps are served on the tailnet while Tailscale is used, and without it the daemon
    // still serves local clients.
    match &daemon.integrations.tailscale {
        crate::integrations::IntegrationState::Used(tailscale) => {
            tokio::spawn(crate::apps::serve_tailnet(daemon.clone(), tailscale.clone(), tailnet));
        }
        crate::integrations::IntegrationState::Off(crate::integrations::OffReason::Disabled) => {
            crate::log!("tailnet: not listening: tailscale is turned off")
        }
        crate::integrations::IntegrationState::Off(crate::integrations::OffReason::NotInstalled) => {
            crate::log!("tailnet: not listening: tailscale isn't installed")
        }
        crate::integrations::IntegrationState::Unavailable(unavailable) => {
            crate::log!("tailnet: not listening: {unavailable}")
        }
    }
    crate::log!("listening on {} (pid {})", socket.display(), std::process::id());

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|source| {
        DaemonError::Socket {
            path: socket.clone(),
            source,
        }
    })?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let daemon = daemon.clone();
                    tokio::spawn(async move { daemon.serve(stream).await });
                }
                Err(e) => crate::log!("accept failed: {e}"),
            },
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }
    crate::log!("shutting down");
    daemon.shutdown().await;
    crate::log!("stopped");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&agent_socket);
    Ok(())
}

/// Pushes the daemon state when a timed debug policy expires, so the apps show the return to the default.
async fn logging_expiry(daemon: Arc<Daemon>) {
    let changed = daemon.logging_changed.clone();
    loop {
        let deadline = match crate::logging::current_policy().policy {
            crate::logging::Policy::Debug {
                expiration: crate::logging::Expiration::Until { unix_ms },
            } => Some(unix_ms),
            crate::logging::Policy::Info
            | crate::logging::Policy::Debug {
                expiration: crate::logging::Expiration::Manual,
            } => None,
        };
        match deadline {
            Some(unix_ms) => {
                let wait = std::time::Duration::from_millis(unix_ms.saturating_sub(crate::logging::now_ms()));
                tokio::select! {
                    () = tokio::time::sleep(wait) => {
                        crate::log!("logging: debug expired");
                        let _ = daemon.list_changed.send(());
                    }
                    () = changed.notified() => {}
                }
            }
            None => changed.notified().await,
        }
    }
}

/// Session keys are unique across daemon restarts, so a client resuming "session N" after a restart can't land on a
/// different session that reused the number: each run starts from the current time in milliseconds. Stays exact in
/// Swift's `UInt64` and JavaScript's doubles (below 2^53) for a few hundred thousand years.
fn first_session_key() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_millis() as u64)
}

fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

impl Daemon {
    /// Live pi processes (working, idle, or stopping).
    pub fn session_count(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// Whether keep-awake is enabled: `keep_awake` in the host file, on by default.
    pub fn keep_awake(&self) -> bool {
        self.config().keep_awake
    }

    /// Switches host logging until the policy expires or the daemon restarts. Clients see the change in the pushed
    /// daemon state; the expiry task pushes it again when a deadline passes.
    pub fn set_logging(&self, policy: crate::logging::Policy) {
        let default = self.config().logging.mode;
        crate::log!("logging: {policy:?} (default {default:?})");
        crate::logging::set_policy(crate::logging::State { policy, default });
        self.logging_changed.notify_one();
        let _ = self.list_changed.send(());
    }

    /// Daemon-wide state for the apps. `awake` is what keep-awake does with the current sessions; `awake` follows
    /// the same rule (a failed `caffeinate` is logged), so a reply never shows a stale value from before that task
    /// caught up.
    /// Client settings are resolved for `device`, the asking connection's.
    pub fn state(&self, device: crate::config::Device) -> crate::apps::DaemonState {
        let sessions = self.session_count();
        let keep_awake = self.keep_awake();
        let app_settings = lock(&self.app_settings);
        crate::apps::DaemonState {
            version: crate::VERSION.to_string(),
            host_id: self.host_id.clone(),
            keep_awake,
            awake: keep_awake && sessions > 0,
            sessions,
            logging: crate::logging::current_policy(),
            settings: app_settings.info(device),
            settings_error: app_settings.error.clone(),
        }
    }

    /// mise for worker trust: `None` when it's off; a refusal when it's enabled but not installed.
    fn worker_mise(&self) -> Result<Option<crate::project::Mise>, crate::project::WorkerRefusal> {
        let mise = self
            .integrations
            .mise
            .optional()
            .map_err(crate::project::WorkerRefusal::integration_unavailable)?;
        Ok(mise.map(|mise| crate::project::Mise {
            program: mise.program.clone(),
            args: mise.args.clone(),
            env: self.base_env.clone(),
        }))
    }

    pub fn status_changed(&self) -> Arc<Notify> {
        self.status_changed.clone()
    }

    pub fn status_changed_broadcast(&self) -> &tokio::sync::broadcast::Sender<()> {
        &self.list_changed
    }

    /// Who may ask over `agent.sock`: every live caller's token.
    pub fn agent_tokens(&self) -> &Arc<crate::agent::Tokens> {
        &self.agent_tokens
    }

    /// The configuration in effect now.
    pub fn config(&self) -> Arc<DaemonConfig> {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Rereads the global files after a change (from the apps or by hand). A valid result replaces the configuration
    /// (the socket stays the running one) and is pushed to the apps; an invalid one keeps the last good values and
    /// tells the apps why.
    pub fn reload_config(&self) {
        let mut snapshot = lock(&self.app_settings);
        self.reload_locked(&mut snapshot);
    }

    /// Rereads with the app settings lock held, so a write and the reload after it never interleave with another
    /// writer or the watcher, and an older read never replaces a newer configuration.
    fn reload_locked(&self, snapshot: &mut crate::apps::SettingsSnapshot) {
        let current = self.config();
        // The daemon's options from the host file alone: a broken client file stops only the apps' list.
        match crate::config::host(&current.paths) {
            Ok(host) => {
                let mut new = crate::config::DaemonConfig::new(&host, current.paths.clone());
                // What needs a restart keeps the values the daemon started with; the apps see them as pending.
                new.socket = current.socket.clone();
                new.log = current.log.clone();
                new.env = current.env.clone();
                new.integrations = current.integrations.clone();
                let mode = new.logging.mode;
                let keep_awake = new.keep_awake;
                *self.config.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(new);
                if mode != current.logging.mode {
                    let mut state = crate::logging::current_policy();
                    state.default = mode;
                    crate::logging::set_policy(state);
                    self.logging_changed.notify_one();
                }
                if keep_awake != current.keep_awake {
                    crate::log!("keep-awake turned {}", if keep_awake { "on" } else { "off" });
                    self.status_changed.notify_one();
                }
                crate::log!("config: reloaded the global files");
            }
            Err(e) => {
                crate::log!("config: the global files can't be used ({e}); keeping the last good values");
            }
        }
        snapshot.reload(&current.paths);
        let _ = self.list_changed.send(());
    }

    /// Applies a change from the apps to its global file, then rereads the files.
    pub fn set_app_setting(&self, change: crate::apps::SettingChange) -> Result<(), crate::apps::SettingError> {
        let mut snapshot = lock(&self.app_settings);
        // A value the apps show read-only (a preset with options) is edited in the file: saving the list control's
        // names would drop the options.
        snapshot.check_editable(&change)?;
        crate::apps::write_setting(&self.config().paths, &change)?;
        crate::log!("config: {} ({:?}) changed from the apps", change.key, change.scope);
        self.reload_locked(&mut snapshot);
        Ok(())
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    pub fn session(&self, key: SessionKey) -> Option<SessionHandle> {
        lock(&self.sessions).get(&key).cloned()
    }

    /// The live session a sandbox tag belongs to: pi's (`<key>-<random>`, set at launch) or one of its calls'
    /// (`<key>-<random>-<n>`).
    fn session_by_tag(&self, tag: &str) -> Option<SessionHandle> {
        let key = tag.split_once('-')?.0.parse().ok()?;
        self.session(key).filter(|s| {
            s.tag
                .as_deref()
                .is_some_and(|own| tag == own || tag.strip_prefix(own).is_some_and(|rest| rest.starts_with('-')))
        })
    }

    pub fn sessions_info(&self) -> Vec<crate::session::SessionInfo> {
        lock(&self.sessions).values().map(|s| s.snapshot().info()).collect()
    }

    async fn shutdown(&self) {
        let handles: Vec<_> = lock(&self.sessions).values().cloned().collect();
        let mut stops = tokio::task::JoinSet::new();
        for handle in handles {
            stops.spawn(async move { handle.stop(StopReason::DaemonShutdown).await });
        }
        while stops.join_next().await.is_some() {}
    }

    fn launch(
        self: &Arc<Self>,
        workspace: Workspace,
        session: SessionArg,
        prepared: launch::Prepared,
        launch_config: crate::config::LaunchConfig,
        trust: &crate::trust::TrustState,
    ) -> Result<SessionHandle, LaunchError> {
        let config = self.config();
        let (mode, mut env, mut policy, proxy, mut commands, warnings, agent_dirs, sockets) = match prepared {
            launch::Prepared::Host { env, proxy } => {
                // pi's agent directories as the sandbox would name them: the host's (the daemon's environment) and
                // this launch's (its environment).
                let named = |env: &crate::launch::Env| {
                    launch::sandbox::agent_dir(
                        env.get("PI_CODING_AGENT_DIR")
                            .filter(|v| !v.is_empty())
                            .map(String::as_str),
                        &config.paths.home,
                        &workspace.path,
                    )
                };
                let agent_dirs = [named(&self.base_env), named(&env)];
                (LaunchMode::Host, env, None, proxy, None, Vec::new(), agent_dirs, None)
            }
            launch::Prepared::Sandbox(sandboxed) => {
                let launch::Sandboxed {
                    mut env,
                    resolution,
                    proxy,
                    commands,
                    warnings,
                    sockets,
                } = *sandboxed;
                // The preset's socket folder of this session's own. Without it, `claude` uses the shared folder as
                // before (its probes meet the block question).
                let sockets = sockets.filter(|folder| match launch::claude_sockets::create(folder) {
                    Ok(()) => true,
                    Err(e) => {
                        crate::log!(
                            "sandbox in {}: can't create {}: {e}",
                            workspace.path.display(),
                            folder.display()
                        );
                        env.remove(launch::claude_sockets::VAR);
                        false
                    }
                });
                for link in &resolution.links {
                    crate::log!("sandbox in {}: {link}", workspace.path.display());
                }
                for warning in &warnings {
                    crate::log!("sandbox in {}: {warning}", workspace.path.display());
                }
                (
                    LaunchMode::Sandbox,
                    env,
                    Some(resolution.policy),
                    proxy,
                    Some(commands),
                    warnings,
                    resolution.agent_dirs,
                    sockets,
                )
            }
        };
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        // The launch's tag: the kernel's record of a refusal by this session's sandbox (pi's, and any command the daemon
        // runs for it) carries it, so the log watcher routes it here.
        let tag = format!("{key}-{}", crate::agent::random_hex(8));
        if let Some(policy) = &mut policy {
            policy.tag = tag.clone();
        }
        if let Some(commands) = &mut commands {
            commands.set_tag(&tag);
        }
        // How this pi reaches the daemon (`agent.sock`); set after the workspace's environment, so a repository can't
        // change it. Behind fnox, pi is fnox's child: the helper that becomes pi reports its process with a one-time
        // launch token (and fnox's proxy, for a filtered network's gate).
        let tokens = self
            .agent_tokens
            .issue_session(key, proxy.is_some())
            .map_err(|source| LaunchError::Spawn {
                program: "session token".into(),
                source,
            })?;
        env.insert(crate::agent::TOKEN_ENV.into(), tokens.session);
        env.insert(
            crate::agent::SOCKET_ENV.into(),
            config.agent_socket().to_string_lossy().into_owned(),
        );
        // What this session's protected files are, the one definition the daemon checks a write against; pi asks
        // `piton config protected`, which answers from it. Set after the workspace's environment too.
        let fnox_files: &[std::path::PathBuf] = proxy.as_ref().map_or(&[], |p| p.files.as_slice());
        let mut excluded = launch::sandbox::never_configuration(&launch_config, &config.paths.home, &self.base_env);
        excluded.push(config.state_dir());
        let protected = crate::agent::Protected::new(&crate::agent::ProtectedFacts {
            source: &launch_config.source,
            checkout: &launch_config.checkout,
            levels: &launch_config.levels,
            agent_dirs: &agent_dirs,
            fnox_files,
            global: &config.paths.global,
            home: &config.paths.home,
            excluded: &excluded,
        });
        match serde_json::to_string(&protected) {
            Ok(json) => {
                env.insert(crate::agent::PROTECTED_ENV.into(), json);
            }
            Err(e) => crate::log!("session {key}: can't describe its protected files to pi: {e}"),
        }
        // Which of the repository's grants this session keeps, for `piton config resolve` inside it: a commit made
        // during the session can't widen them. Set after the workspace's environment too. A filter that can't be
        // described keeps nothing.
        env.insert(
            crate::trust::FILTER_ENV.into(),
            serde_json::to_string(&trust.filter()).unwrap_or_else(|_| "{\"kind\":\"none\"}".into()),
        );
        let weak = Arc::downgrade(self);
        let restarting = Arc::downgrade(self);
        let hooks = Hooks {
            ended: Box::new(move |key| {
                if let Some(folder) = &sockets {
                    launch::claude_sockets::remove(folder);
                }
                if let Some(daemon) = weak.upgrade() {
                    daemon.agent_tokens.forget(key);
                    lock(&daemon.sessions).remove(&key);
                    let _ = daemon.list_changed.send(());
                }
            }),
            restart: Box::new(move |key| {
                if let Some(daemon) = restarting.upgrade() {
                    tokio::spawn(async move { daemon.restart_session(key).await });
                }
            }),
            status_changed: self.status_changed.clone(),
            list_changed: self.list_changed.clone(),
            network_entry_saved: {
                let daemon = Arc::downgrade(self);
                Box::new(move |target, file| {
                    let Some(daemon) = daemon.upgrade() else { return };
                    for session in lock(&daemon.sessions).values() {
                        let _ = session.tx.send(session::SessionCmd::NetworkEntrySaved {
                            target: target.clone(),
                            file: file.clone(),
                        });
                    }
                })
            },
        };
        let relation = match &workspace.relation {
            Relation::Source => String::new(),
            Relation::Worker { source, .. } => format!(", worker of {}", source.display()),
        };
        // A filtered network gets a gate: pi's trusted hosts, and fnox's proxy as the route once it reports it.
        let network = match policy.as_ref().map(|p| &p.network) {
            Some(launch::sandbox::Network::Filtered { .. }) => {
                let builtin = config
                    .pi
                    .command
                    .first()
                    .map(|pi| crate::pi::catalog::builtin_for(pi, &env))
                    .unwrap_or_else(|| Ok(crate::pi::catalog::Builtin::default()))
                    .unwrap_or_else(|e| {
                        crate::log!("session {key}: pi's built-in hosts aren't trusted (asked about instead): {e}");
                        crate::pi::catalog::Builtin::default()
                    });
                let files =
                    crate::pi::catalog::Files::new(agent_dirs[1].clone(), &config.paths.global, &launch_config.levels);
                let ssh = crate::launch::sandbox::network::ssh::ensure(&config.state_dir());
                if ssh.is_none() {
                    crate::log!("session {key}: can't make the ssh wrapper: SSH won't go through the gate");
                }
                // Behind fnox, the helper reports fnox's proxy with its launch token; the gate waits for it.
                let route = match &proxy {
                    Some(_) => None,
                    None => Some(crate::launch::sandbox::network::Route::Direct),
                };
                Some(session::NetworkSpec {
                    ssh,
                    catalog: crate::pi::catalog::Catalog::new(builtin, files),
                    route,
                })
            }
            Some(launch::sandbox::Network::Open { .. } | launch::sandbox::Network::Direct { .. }) | None => None,
        };
        let spec = LaunchSpec {
            key,
            workspace,
            mode,
            session,
            env,
            launch_token: tokens.launch,
            policy,
            protected,
            proxy,
            commands,
            source: launch_config.source.clone(),
            checkout: launch_config.checkout.clone(),
            grants: launch_config.sandbox.commands.clone(),
            levels: launch_config.levels.clone(),
            warnings,
            network,
            project_trusted: trust.pi_trusts(),
        };
        let resume = match &spec.session {
            SessionArg::New => "new".to_string(),
            SessionArg::Resume(file) => format!("resume {}", file.display()),
        };
        let workspace_text = format!("{}{relation}", spec.workspace.path.display());
        let reported = spec.launch_token.is_some();
        let handle = match session::spawn(self.config(), spec, hooks) {
            Ok(handle) => handle,
            Err(e) => {
                self.agent_tokens.forget(key);
                crate::log!("launch failed in {workspace_text} ({mode:?}, {resume}): {e}");
                return Err(e);
            }
        };
        // pi's process is the one started, unless the helper reports it (behind fnox, pi is fnox's child).
        if let (false, Some(pid)) = (reported, handle.snapshot().pid) {
            self.agent_tokens.pi_started(key, pid as i32);
        }
        crate::log!(
            "session {key}: launched pid {} in {workspace_text} ({mode:?}, {resume})",
            handle.snapshot().pid.map_or_else(|| "?".into(), |p| p.to_string())
        );
        lock(&self.sessions).insert(key, handle.clone());
        let _ = self.list_changed.send(());
        // Keep-awake follows live sessions, not only working ones.
        self.status_changed.notify_one();
        Ok(handle)
    }

    pub async fn launch_in(
        self: &Arc<Self>,
        workspace: &Path,
        mode: LaunchMode,
        session: SessionArg,
        trust: TrustCheck,
    ) -> Result<SessionHandle, LaunchError> {
        match mode {
            LaunchMode::Sandbox => {
                launch::check_workspace_links(workspace).inspect_err(|e| crate::log!("launch refused: {e}"))?;
            }
            LaunchMode::Host => {}
        }
        let workspace = launch::resolve_workspace(workspace).inspect_err(|e| crate::log!("launch refused: {e}"))?;
        let config = self.config();
        let state = self
            .trust_state(&workspace)
            .inspect_err(|e| crate::log!("launch refused: {e}"))?;
        match (&state, trust) {
            (
                crate::trust::TrustState::Needed {
                    grants,
                    resources,
                    added,
                    ..
                },
                TrustCheck::Ask,
            ) => {
                let needed = crate::trust::TrustNeeded {
                    source: workspace.source().to_path_buf(),
                    grants: grants.clone(),
                    resources: resources.clone(),
                    added: added.clone(),
                };
                crate::log!("launch refused: {needed}");
                return Err(LaunchError::TrustNeeded(Box::new(needed)));
            }
            (crate::trust::TrustState::Needed { .. }, TrustCheck::KeepDecided)
            | (
                crate::trust::TrustState::NothingToTrust
                | crate::trust::TrustState::Trusted { .. }
                | crate::trust::TrustState::Declined { .. },
                TrustCheck::Ask | TrustCheck::KeepDecided,
            ) => {}
        }
        let launch_config = launch::resolve_config(&config, &workspace, &state.filter())
            .inspect_err(|e| crate::log!("launch refused: {e}"))?;
        let prepared = launch::prepare(
            &config,
            &self.integrations,
            &launch_config,
            &self.base_env,
            &workspace.path,
            mode,
        )
        .await
        .inspect_err(|e| crate::log!("launch refused: {e}"))?;
        self.launch(workspace, session, prepared, launch_config, &state)
    }

    /// A workspace's trust, from its configuration and the user's recorded decision.
    pub fn trust_state(&self, workspace: &Workspace) -> Result<crate::trust::TrustState, LaunchError> {
        let inputs = crate::config::trust_inputs(workspace, &self.config().paths)?;
        let record = lock(&self.state).trust(&inputs.source).map_err(LaunchError::State)?;
        Ok(crate::trust::TrustState::decide(&inputs, record.as_ref()))
    }

    /// Records the user's decision for the repository of `workspace`, checked against the grants they were shown.
    pub fn decide_trust(
        &self,
        workspace: &Path,
        decision: crate::state::Decision,
        shown: &crate::config::Grants,
    ) -> Result<crate::trust::TrustState, TrustError> {
        let workspace = launch::resolve_workspace(workspace).map_err(|e| TrustError::Workspace(e.to_string()))?;
        let inputs = crate::config::trust_inputs(&workspace, &self.config().paths)
            .map_err(|e| TrustError::Workspace(e.to_string()))?;
        crate::trust::record(&lock(&self.state), &inputs, decision, shown).map_err(TrustError::Decide)
    }

    /// Forgets the decision for the repository of `workspace`.
    pub fn reset_trust(&self, workspace: &Path) -> Result<crate::trust::TrustState, TrustError> {
        let workspace = launch::resolve_workspace(workspace).map_err(|e| TrustError::Workspace(e.to_string()))?;
        let inputs = crate::config::trust_inputs(&workspace, &self.config().paths)
            .map_err(|e| TrustError::Workspace(e.to_string()))?;
        let state = lock(&self.state);
        if state
            .reset_trust(&inputs.source)
            .map_err(|e| TrustError::Decide(e.into()))?
        {
            crate::log!("trust: {} reset", inputs.source.display());
        }
        Ok(crate::trust::TrustState::decide(&inputs, None))
    }

    /// Stops a session and launches it again from its file, in the same workspace and mode, with the configuration as
    /// it is now (a sandbox entry the user just saved).
    async fn restart_session(self: &Arc<Self>, key: SessionKey) {
        let Some(handle) = self.session(key) else { return };
        let shared = handle.snapshot();
        let Some(file) = shared.file.clone() else {
            crate::log!("session {key}: not restarted: it has no session file yet");
            return;
        };
        handle.stop(StopReason::Requested).await;
        // A restart nobody asked for never asks: it keeps what was decided.
        match self
            .launch_in(
                &shared.workspace,
                shared.mode,
                SessionArg::Resume(file.into()),
                TrustCheck::KeepDecided,
            )
            .await
        {
            Ok(new) => crate::log!("session {key}: restarted as session {}", new.key),
            Err(e) => crate::log!("session {key}: restart failed: {e}"),
        }
    }

    /// The live session that has `file` open, other than `except`.
    fn live_with_file(&self, file: &str, except: SessionKey) -> Option<SessionHandle> {
        lock(&self.sessions)
            .values()
            .find(|s| s.key != except && s.snapshot().file.as_deref() == Some(file))
            .cloned()
    }

    fn find(&self, query: &str) -> Option<SessionHandle> {
        let sessions = lock(&self.sessions);
        if let Ok(key) = query.parse::<SessionKey>()
            && let Some(s) = sessions.get(&key)
        {
            return Some(s.clone());
        }
        sessions
            .values()
            .find(|s| {
                let shared = s.snapshot();
                shared.id.as_deref() == Some(query) || shared.file.as_deref() == Some(query)
            })
            .cloned()
    }
}
