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
mod rpc;
mod socket;
mod watch;

pub use protocol::{
    AttachControl, ControlRequest, ControlResponse, EndReason, FRAME_CONTROL, FRAME_RECORD, Hello, PROTOCOL_VERSION,
};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::config::DaemonConfig;
use crate::launch::LaunchMode;
use crate::launch::{self, Env, LaunchError, SessionArg};
use crate::project::{Relation, Workspace};
use crate::session::{self, Hooks, LaunchSpec, SessionHandle, SessionKey, StopReason, lock};

pub struct Daemon {
    /// The configuration in effect: replaced when the global files change. Each launch, session, and request takes
    /// the current one; keys that need a restart are read once at start from `started`.
    config: std::sync::RwLock<Arc<DaemonConfig>>,
    /// The global files as the apps see them: values, where each is set, and keys waiting for a restart.
    app_settings: Mutex<crate::apps::SettingsSnapshot>,
    base_env: Env,
    /// mise for worker trust; `None` when the config turns mise off.
    mise: Option<crate::project::Mise>,
    sessions: Mutex<BTreeMap<SessionKey, SessionHandle>>,
    next_key: AtomicU64,
    next_client: AtomicU64,
    status_changed: Arc<Notify>,
    /// Fires whenever the session list (membership, status, clients, identity, title) may have changed.
    list_changed: tokio::sync::broadcast::Sender<()>,
    /// This host's identity, from the state database (opened once at start; nothing else in it is read yet).
    host_id: String,
    /// Fires when host logging changes, so the expiry task re-arms.
    logging_changed: Arc<Notify>,
    /// Session tokens for `agent.sock`.
    agent_tokens: crate::agent::Tokens,
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
    // What pi will be launched with: if a tool is missing here, sessions fail to start.
    let found: Vec<String> = [
        config.pi.command.first().map(String::as_str).unwrap_or("pi"),
        config.sandbox.command.first().map(String::as_str).unwrap_or("nono"),
        "mise",
        config.tailnet.cli.as_str(),
    ]
    .iter()
    .map(|tool| {
        let resolved = launch::resolve_program(tool, &base_env);
        match resolved.contains('/') {
            true => format!("{tool}={resolved}"),
            false => format!("{tool}=NOT FOUND"),
        }
    })
    .collect();
    // pi's own agent dir: unset means pi's default (~/.pi/agent), not the user's usual setup.
    let agent_dir = base_env
        .get("PI_CODING_AGENT_DIR")
        .map_or("unset (pi's default)", String::as_str);
    crate::log!(
        "environment: {} variables; {}; PI_CODING_AGENT_DIR={agent_dir}",
        base_env.len(),
        found.join(", ")
    );

    let host_id = crate::state::State::open(&config.state_db())?.host_id()?;
    crate::log!("state: {} (host {host_id})", config.state_db().display());
    let mise = config.env.mise.then(|| crate::project::Mise {
        program: launch::resolve_program("mise", &base_env),
        env: base_env.clone(),
    });
    let app_settings = crate::apps::SettingsSnapshot::new(&config.paths);
    let daemon = Arc::new(Daemon {
        config: std::sync::RwLock::new(Arc::new(config)),
        app_settings: Mutex::new(app_settings),
        base_env,
        mise,
        sessions: Mutex::new(BTreeMap::new()),
        next_key: AtomicU64::new(first_session_key()),
        next_client: AtomicU64::new(1),
        status_changed: Arc::new(Notify::new()),
        list_changed: tokio::sync::broadcast::channel(64).0,
        host_id,
        logging_changed: Arc::new(Notify::new()),
        agent_tokens: crate::agent::Tokens::default(),
    });
    tokio::spawn(crate::daemon::awake::run(daemon.clone()));
    tokio::spawn(logging_expiry(daemon.clone()));
    crate::daemon::watch::spawn(daemon.clone());
    tokio::spawn(crate::agent::serve(daemon.clone(), agent_listener));
    let tailnet = daemon.config().tailnet.clone();
    if tailnet.enabled {
        // Remote access is optional: without Tailscale the daemon still serves local clients.
        // Like pi and nono: found through the user's shell PATH, not launchd's (`/usr/local/bin` isn't in it).
        let cli = launch::resolve_program(&tailnet.cli, &daemon.base_env);
        tokio::spawn(crate::apps::serve_tailnet(daemon.clone(), cli, tailnet));
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
    pub fn state(&self) -> crate::apps::DaemonState {
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
            settings: app_settings.info(),
            settings_error: app_settings.error.clone(),
        }
    }

    pub fn status_changed(&self) -> Arc<Notify> {
        self.status_changed.clone()
    }

    pub fn status_changed_broadcast(&self) -> &tokio::sync::broadcast::Sender<()> {
        &self.list_changed
    }

    /// Who may ask over `agent.sock`: one token per live session.
    pub fn agent_tokens(&self) -> &crate::agent::Tokens {
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
                new.tailnet = current.tailnet.clone();
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
        crate::apps::write_setting(&self.config().paths, &change)?;
        crate::log!(
            "config: {} ({:?}) changed from the apps",
            change.change.key,
            change.scope
        );
        self.reload_locked(&mut snapshot);
        Ok(())
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    pub fn session(&self, key: SessionKey) -> Option<SessionHandle> {
        lock(&self.sessions).get(&key).cloned()
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
        mode: LaunchMode,
        session: SessionArg,
        mut env: Env,
        launch_config: crate::config::LaunchConfig,
    ) -> Result<SessionHandle, LaunchError> {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        // How this pi reaches the daemon for protected writes; set after the workspace's environment, so a
        // repository can't change it.
        let token = self.agent_tokens.issue(key).map_err(|source| LaunchError::Spawn {
            program: "session token".into(),
            source,
        })?;
        env.insert(crate::agent::TOKEN_ENV.into(), token);
        env.insert(
            crate::agent::SOCKET_ENV.into(),
            self.config().agent_socket().to_string_lossy().into_owned(),
        );
        let profile = match mode {
            LaunchMode::Sandbox => Some(
                launch::write_profile(&self.config(), &launch_config, &self.base_env, key)
                    .inspect_err(|e| crate::log!("launch refused: {e}"))?,
            ),
            LaunchMode::Host => None,
        };
        let weak = Arc::downgrade(self);
        let hooks = Hooks {
            ended: Box::new(move |key| {
                if let Some(daemon) = weak.upgrade() {
                    daemon.agent_tokens.forget(key);
                    lock(&daemon.sessions).remove(&key);
                    let _ = daemon.list_changed.send(());
                }
            }),
            status_changed: self.status_changed.clone(),
            list_changed: self.list_changed.clone(),
        };
        let relation = match &workspace.relation {
            Relation::Source => String::new(),
            Relation::Worker { source, .. } => format!(", worker of {}", source.display()),
        };
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let protected = crate::protected::Roots {
            source: canonical(&launch_config.source),
            checkout: canonical(&launch_config.checkout),
        };
        let spec = LaunchSpec {
            key,
            workspace,
            mode,
            session,
            env,
            profile,
            protected,
        };
        let resume = match &spec.session {
            SessionArg::New => "new".to_string(),
            SessionArg::Resume(file) => format!("resume {}", file.display()),
        };
        let workspace_text = format!("{}{relation}", spec.workspace.path.display());
        let handle = match session::spawn(self.config(), spec, hooks) {
            Ok(handle) => handle,
            Err(e) => {
                crate::log!("launch failed in {workspace_text} ({mode:?}, {resume}): {e}");
                return Err(e);
            }
        };
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
    ) -> Result<SessionHandle, LaunchError> {
        let workspace = launch::resolve_workspace(workspace).inspect_err(|e| crate::log!("launch refused: {e}"))?;
        let config = self.config();
        let launch_config =
            launch::resolve_config(&config, &workspace).inspect_err(|e| crate::log!("launch refused: {e}"))?;
        let env = launch::workspace_env(&config, &self.base_env, &workspace.path, mode)
            .await
            .inspect_err(|e| crate::log!("launch refused: {e}"))?;
        self.launch(workspace, mode, session, env, launch_config)
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
