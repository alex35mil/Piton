//! The daemon: session registry, Unix socket server, `rpc` connections (with interception and moves), and control
//! requests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc};

use crate::config::Config;
use crate::launch::{self, Env, LaunchError, SessionArg};
use crate::project::{Relation, Workspace};
use crate::session::{
    self, Frame, Hooks, Internal, LaunchSpec, Outstanding, SessionCmd, SessionHandle, SessionKey, StopReason, lock,
};
use crate::wire::{
    AttachControl, ControlRequest, ControlResponse, FRAME_CONTROL, FRAME_RECORD, Hello, LaunchMode, PROTOCOL_VERSION,
    WorkerEntry,
};

pub struct Daemon {
    /// The configuration in effect: replaced when the global files change. Each launch, session, and request takes
    /// the current one; keys that need a restart are read once at start from `started`.
    config: std::sync::RwLock<Arc<Config>>,
    /// The global files as the apps see them: values, where each is set, and keys waiting for a restart.
    preferences: Mutex<crate::preferences::Snapshot>,
    base_env: Env,
    /// mise for worker trust; `None` when the config turns mise off.
    mise: Option<crate::worker::Mise>,
    sessions: Mutex<BTreeMap<SessionKey, SessionHandle>>,
    next_key: AtomicU64,
    next_client: AtomicU64,
    status_changed: Arc<Notify>,
    /// Fires whenever the session list (membership, status, clients, identity, title) may have changed.
    list_changed: tokio::sync::broadcast::Sender<()>,
    /// Whether keep-awake is enabled: state, kept in the database, so it survives restarts. On by default.
    keep_awake: std::sync::atomic::AtomicBool,
    /// The host's state database. Writes are rare and small, so one connection behind a lock.
    state: Mutex<crate::state::State>,
    /// This host's identity, from the database.
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

pub async fn run(config: Config) -> Result<(), DaemonError> {
    let socket = config.socket.clone();
    crate::diagnostics::set(crate::diagnostics::State {
        policy: config.logging.mode.policy(),
        default: config.logging.mode,
    });
    if let Some(path) = &config.log
        && let Err(e) = crate::log::init(path)
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

    let state = crate::state::State::open(&config.state_db())?;
    let host_id = state.host_id()?;
    let keep_awake = state.bool(crate::state::Setting::KeepAwake)?.unwrap_or(true);
    crate::log!("state: {} (host {host_id})", config.state_db().display());
    let mise = config.env.mise.then(|| crate::worker::Mise {
        program: launch::resolve_program("mise", &base_env),
        env: base_env.clone(),
    });
    let preferences = crate::preferences::Snapshot::new(&config);
    let daemon = Arc::new(Daemon {
        config: std::sync::RwLock::new(Arc::new(config)),
        preferences: Mutex::new(preferences),
        base_env,
        mise,
        sessions: Mutex::new(BTreeMap::new()),
        next_key: AtomicU64::new(first_session_key()),
        next_client: AtomicU64::new(1),
        status_changed: Arc::new(Notify::new()),
        list_changed: tokio::sync::broadcast::channel(64).0,
        keep_awake: std::sync::atomic::AtomicBool::new(keep_awake),
        state: Mutex::new(state),
        host_id,
        logging_changed: Arc::new(Notify::new()),
        agent_tokens: crate::agent::Tokens::default(),
    });
    tokio::spawn(crate::awake::run(daemon.clone()));
    tokio::spawn(logging_expiry(daemon.clone()));
    crate::watch::spawn(daemon.clone());
    tokio::spawn(crate::agent::serve(daemon.clone(), agent_listener));
    let tailnet = daemon.config().tailnet.clone();
    if tailnet.enabled {
        // Remote access is optional: without Tailscale the daemon still serves local clients.
        // Like pi and nono: found through the user's shell PATH, not launchd's (`/usr/local/bin` isn't in it).
        let cli = launch::resolve_program(&tailnet.cli, &daemon.base_env);
        tokio::spawn(crate::websocket::serve_tailnet(daemon.clone(), cli, tailnet));
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
        let deadline = match crate::diagnostics::current().policy {
            crate::diagnostics::Policy::Debug {
                expiration: crate::diagnostics::Expiration::Until { unix_ms },
            } => Some(unix_ms),
            crate::diagnostics::Policy::Info
            | crate::diagnostics::Policy::Debug {
                expiration: crate::diagnostics::Expiration::Manual,
            } => None,
        };
        match deadline {
            Some(unix_ms) => {
                let wait = std::time::Duration::from_millis(unix_ms.saturating_sub(crate::diagnostics::now_ms()));
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

    pub fn keep_awake(&self) -> bool {
        self.keep_awake.load(Ordering::Relaxed)
    }

    /// Turns keep-awake on or off, and keeps the choice across restarts.
    pub fn set_keep_awake(&self, on: bool) -> Result<(), crate::state::StateError> {
        lock(&self.state).set_bool(crate::state::Setting::KeepAwake, on)?;
        let was = self.keep_awake.swap(on, Ordering::Relaxed);
        if was != on {
            crate::log!("keep-awake turned {}", if on { "on" } else { "off" });
            self.status_changed.notify_one();
            let _ = self.list_changed.send(());
        }
        Ok(())
    }

    /// Switches host logging until the policy expires or the daemon restarts. Clients see the change in the pushed
    /// daemon state; the expiry task pushes it again when a deadline passes.
    pub fn set_logging(&self, policy: crate::diagnostics::Policy) {
        let default = self.config().logging.mode;
        crate::log!("logging: {policy:?} (default {default:?})");
        crate::diagnostics::set(crate::diagnostics::State { policy, default });
        self.logging_changed.notify_one();
        let _ = self.list_changed.send(());
    }

    /// Daemon-wide state for the apps. `awake` is what keep-awake does with the current sessions; awake.rs follows
    /// the same rule (a failed `caffeinate` is logged), so a reply never shows a stale value from before that task
    /// caught up.
    pub fn state(&self) -> crate::wire::DaemonState {
        let sessions = self.session_count();
        let keep_awake = self.keep_awake();
        let preferences = lock(&self.preferences);
        crate::wire::DaemonState {
            version: crate::VERSION.to_string(),
            host_id: self.host_id.clone(),
            keep_awake,
            awake: keep_awake && sessions > 0,
            sessions,
            logging: crate::diagnostics::current(),
            settings: preferences.info(keep_awake),
            settings_error: preferences.error.clone(),
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
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Rereads the global files after a change (from the apps or by hand). A valid result replaces the configuration
    /// (the socket stays the running one) and is pushed to the apps; an invalid one keeps the last good values and
    /// tells the apps why.
    pub fn reload_preferences(&self) {
        let mut snapshot = lock(&self.preferences);
        self.reload_locked(&mut snapshot);
    }

    /// Rereads with the preferences lock held, so a write and the reload after it never interleave with another
    /// writer or the watcher, and an older read never replaces a newer configuration.
    fn reload_locked(&self, snapshot: &mut crate::preferences::Snapshot) {
        let current = self.config();
        match snapshot.reload(current.paths.clone()) {
            crate::preferences::Read::Good(new) => {
                let mut new = *new;
                // What needs a restart keeps the values the daemon started with; the apps see them as pending.
                new.socket = current.socket.clone();
                new.log = current.log.clone();
                new.env = current.env.clone();
                new.tailnet = current.tailnet.clone();
                let mode = new.logging.mode;
                *self.config.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(new);
                if mode != current.logging.mode {
                    let mut state = crate::diagnostics::current();
                    state.default = mode;
                    crate::diagnostics::set(state);
                    self.logging_changed.notify_one();
                }
                crate::log!("settings: reloaded the global files");
            }
            crate::preferences::Read::Kept => {
                crate::log!("settings: the global files can't be used; keeping the last good values");
            }
        }
        let _ = self.list_changed.send(());
    }

    /// Applies a change from the apps: state to the database, configuration to its global file.
    pub fn set_preference(&self, change: crate::preferences::Change) -> Result<(), crate::preferences::SetError> {
        match change {
            crate::preferences::Change::State(crate::state::Setting::KeepAwake, value) => {
                self.set_keep_awake(value.unwrap_or(true))?;
            }
            crate::preferences::Change::Config { key, value, target } => {
                let mut snapshot = lock(&self.preferences);
                crate::preferences::write(&self.config().paths, key, value, target)?;
                crate::log!("settings: {key} changed from the apps");
                self.reload_locked(&mut snapshot);
            }
        }
        Ok(())
    }

    pub fn next_client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    pub fn session(&self, key: SessionKey) -> Option<SessionHandle> {
        lock(&self.sessions).get(&key).cloned()
    }

    pub fn sessions_info(&self) -> Vec<crate::wire::SessionInfo> {
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
        settings: crate::settings::DaemonSettings,
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
                launch::write_profile(&self.config(), &settings, &self.base_env, key)
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
            source: canonical(&settings.source),
            checkout: canonical(&settings.checkout),
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
        let settings =
            launch::resolve_settings(&config, &workspace).inspect_err(|e| crate::log!("launch refused: {e}"))?;
        let env = launch::workspace_env(&config, &self.base_env, &workspace.path, mode)
            .await
            .inspect_err(|e| crate::log!("launch refused: {e}"))?;
        self.launch(workspace, mode, session, env, settings)
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

    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let hello = match lines.next_line().await {
            Ok(Some(line)) => serde_json::from_str::<Hello>(&line),
            Ok(None) | Err(_) => return,
        };
        match hello {
            Ok(Hello::Rpc {
                version,
                workspace,
                client: _,
            }) if version == PROTOCOL_VERSION => {
                self.serve_rpc(workspace, lines, write).await;
            }
            Ok(Hello::Control { version }) if version == PROTOCOL_VERSION => {
                let response = match lines.next_line().await {
                    Ok(Some(line)) => match serde_json::from_str::<ControlRequest>(&line) {
                        Ok(request) => self.control(request).await,
                        Err(e) => ControlResponse::Error {
                            message: format!("invalid request: {e}"),
                        },
                    },
                    Ok(None) | Err(_) => return,
                };
                let _ = write_json_line(&mut write, &response).await;
            }
            Ok(Hello::View { version, client }) if version == PROTOCOL_VERSION => {
                crate::log!("view client connected ({})", client.as_deref().unwrap_or("unnamed"));
                let (in_tx, in_rx) = mpsc::unbounded_channel::<String>();
                let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Arc<str>>();
                let writer = tokio::spawn(async move {
                    while let Some(line) = out_rx.recv().await {
                        if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                            break;
                        }
                    }
                });
                let reader = tokio::spawn(async move {
                    while let Ok(Some(line)) = lines.next_line().await {
                        if in_tx.send(line).is_err() {
                            break;
                        }
                    }
                });
                crate::viewconn::serve(self.clone(), in_rx, out_tx).await;
                reader.abort();
                let _ = writer.await;
                crate::log!("view client disconnected");
            }
            Ok(Hello::Rpc { .. } | Hello::Control { .. } | Hello::View { .. }) => {
                let message = format!("unsupported protocol version (daemon speaks {PROTOCOL_VERSION})");
                let _ = write_control(&mut write, &AttachControl::Error { message }).await;
            }
            Err(e) => {
                let _ = write_control(
                    &mut write,
                    &AttachControl::Error {
                        message: format!("invalid hello: {e}"),
                    },
                )
                .await;
            }
        }
    }

    async fn control(self: &Arc<Self>, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::List => {
                let sessions = lock(&self.sessions).values().map(|s| s.snapshot().info()).collect();
                ControlResponse::Sessions { sessions }
            }
            ControlRequest::Start {
                workspace,
                mode,
                prompt,
            } => match self.launch_in(&workspace, mode, SessionArg::New).await {
                Ok(handle) => {
                    if let Some(text) = prompt {
                        let _ = handle.tx.send(SessionCmd::Internal(Internal::Prompt { text }));
                    }
                    ControlResponse::Started {
                        session: handle.snapshot().info(),
                    }
                }
                Err(e) => ControlResponse::Error { message: e.to_string() },
            },
            ControlRequest::Stop { session } => {
                let Some(handle) = self.find(&session) else {
                    return ControlResponse::Error {
                        message: format!("no live session matches {session}"),
                    };
                };
                crate::log!("session {}: stop requested ({session})", handle.key);
                let forced = handle
                    .stop(StopReason::Requested)
                    .await
                    .is_some_and(|outcome| outcome.forced);
                ControlResponse::Stopped { forced }
            }
            ControlRequest::WorkerCreate(request) => {
                let mise = self.mise.clone();
                let paths = self.config().paths.clone();
                let created =
                    tokio::task::spawn_blocking(move || crate::worker::create(&request, mise.as_ref(), &paths)).await;
                match created {
                    Ok(Ok(worker)) => ControlResponse::Worker { worker },
                    Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
                    Err(e) => ControlResponse::Error { message: e.to_string() },
                }
            }
            ControlRequest::WorkerList { from } => {
                match tokio::task::spawn_blocking(move || crate::worker::list(&from)).await {
                    Ok(Ok(workers)) => {
                        let sessions: Vec<_> = lock(&self.sessions).values().map(|s| s.snapshot().info()).collect();
                        let workers = workers
                            .into_iter()
                            .map(|worker| WorkerEntry {
                                sessions: sessions
                                    .iter()
                                    .filter(|s| s.workspace.starts_with(&worker.path))
                                    .cloned()
                                    .collect(),
                                worker,
                            })
                            .collect();
                        ControlResponse::Workers { workers }
                    }
                    Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
                    Err(e) => ControlResponse::Error { message: e.to_string() },
                }
            }
            ControlRequest::WorkerDelete { path, force } => self.delete_worker(path, force).await,
            ControlRequest::Logging => ControlResponse::Logging {
                logging: crate::diagnostics::current(),
            },
            ControlRequest::SetLogging { policy } => {
                self.set_logging(policy);
                ControlResponse::Logging {
                    logging: crate::diagnostics::current(),
                }
            }
        }
    }

    /// Stops the worker's sessions (after checking it can be deleted), then deletes its files.
    async fn delete_worker(self: &Arc<Self>, path: PathBuf, force: bool) -> ControlResponse {
        let checked = tokio::task::spawn_blocking(move || {
            let worker = crate::worker::info(&path)?;
            if !force {
                let lines = crate::worker::leftovers(&worker.path);
                if !lines.is_empty() {
                    return Err(crate::worker::WorkerRefusal::Leftovers {
                        path: worker.path,
                        lines,
                    });
                }
            }
            Ok(worker)
        })
        .await;
        let worker = match checked {
            Ok(Ok(worker)) => worker,
            Ok(Err(refusal)) => return ControlResponse::WorkerRefused { refusal },
            Err(e) => return ControlResponse::Error { message: e.to_string() },
        };
        let handles: Vec<_> = lock(&self.sessions)
            .values()
            // Sessions in the worker or in a folder inside it.
            .filter(|s| s.snapshot().workspace.starts_with(&worker.path))
            .cloned()
            .collect();
        let stopped = handles.len();
        let mut stops = tokio::task::JoinSet::new();
        for handle in handles {
            crate::log!("session {}: stopping for worker delete", handle.key);
            stops.spawn(async move { handle.stop(StopReason::Requested).await });
        }
        while stops.join_next().await.is_some() {}
        // Leftovers were checked above (or forced); the sessions just stopped can't have added commits worth keeping
        // without the user seeing them, but check again unless forced.
        let mise = self.mise.clone();
        match tokio::task::spawn_blocking(move || crate::worker::remove(&worker, force, mise.as_ref())).await {
            Ok(Ok(warning)) => ControlResponse::WorkerDeleted { stopped, warning },
            Ok(Err(refusal)) => ControlResponse::WorkerRefused { refusal },
            Err(e) => ControlResponse::Error { message: e.to_string() },
        }
    }

    async fn serve_rpc(
        self: Arc<Self>,
        workspace: PathBuf,
        mut lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
        write: OwnedWriteHalf,
    ) {
        let client = self.next_client.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        let writer = tokio::spawn(write_frames(write, rx));

        let mode = self.config().attach_mode;
        let mut current = match self.launch_in(&workspace, mode, SessionArg::New).await {
            Ok(handle) => handle,
            Err(e) => {
                let _ = tx.send(Frame::Control(Arc::new(AttachControl::Error {
                    message: e.to_string(),
                })));
                drop(tx);
                let _ = writer.await;
                return;
            }
        };
        let _ = tx.send(Frame::Control(Arc::new(AttachControl::Welcome {
            session: current.key,
        })));
        crate::log!(
            "client {client}: attached to session {} ({})",
            current.key,
            workspace.display()
        );
        current.attach(client, tx.clone()).await;

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(Value::Object(command)) = serde_json::from_str::<Value>(&line) else {
                let _ = tx.send(Frame::Record(
                    json!({"type":"response","command":"parse","success":false,"error":"Failed to parse command"})
                        .to_string()
                        .into(),
                ));
                continue;
            };
            match self.intercept(&current, &command).await {
                Intercept::Forward => {
                    let _ = current.tx.send(SessionCmd::FromClient { client, command });
                }
                Intercept::Reject(error) => {
                    crate::log!(
                        "client {client}: rejected {} on session {}: {error}",
                        command_type(&command),
                        current.key
                    );
                    let _ = tx.send(Frame::Record(response(&command, Err(error)).to_string().into()));
                }
                Intercept::MoveTo(target) => {
                    let old = current;
                    crate::log!(
                        "client {client}: {} moves session {} -> {}",
                        command_type(&command),
                        old.key,
                        target.key
                    );
                    let outstanding = old.detach(client).await;
                    for o in outstanding {
                        let _ = tx.send(Frame::Record(moved_response(&o).to_string().into()));
                    }
                    let _ = tx.send(Frame::Record(
                        response(&command, Ok(success_data(&command))).to_string().into(),
                    ));
                    target.attach(client, tx.clone()).await;
                    current = target;
                }
            }
        }
        crate::log!("client {client}: detached from session {}", current.key);
        current.detach(client).await;
        drop(tx);
        let _ = writer.await;
    }

    async fn intercept(self: &Arc<Self>, current: &SessionHandle, command: &Map<String, Value>) -> Intercept {
        let kind = command.get("type").and_then(Value::as_str).unwrap_or_default();
        let shared = current.snapshot();
        let is_shared = shared.clients > 1;
        match kind {
            "switch_session" => {
                let Some(path) = command.get("sessionPath").and_then(Value::as_str) else {
                    return Intercept::Forward;
                };
                if let Some(target) = self.live_with_file(path, current.key) {
                    return Intercept::MoveTo(target);
                }
                if !is_shared {
                    return Intercept::Forward;
                }
                match self
                    .launch_in(&shared.workspace, shared.mode, SessionArg::Resume(path.into()))
                    .await
                {
                    Ok(target) => Intercept::MoveTo(target),
                    Err(e) => Intercept::Reject(e.to_string()),
                }
            }
            "new_session" if is_shared => match self.launch_in(&shared.workspace, shared.mode, SessionArg::New).await {
                Ok(target) => Intercept::MoveTo(target),
                Err(e) => Intercept::Reject(e.to_string()),
            },
            "fork" | "clone" if is_shared => {
                Intercept::Reject(format!("{kind} isn't supported while other clients share this session"))
            }
            _ => Intercept::Forward,
        }
    }
}

fn command_type(command: &Map<String, Value>) -> &str {
    command.get("type").and_then(Value::as_str).unwrap_or("?")
}

enum Intercept {
    Forward,
    Reject(String),
    MoveTo(SessionHandle),
}

fn response(command: &Map<String, Value>, result: Result<Value, String>) -> Value {
    let mut out = Map::new();
    if let Some(id) = command.get("id") {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), json!("response"));
    out.insert("command".into(), command.get("type").cloned().unwrap_or(Value::Null));
    match result {
        Ok(data) => {
            out.insert("success".into(), json!(true));
            out.insert("data".into(), data);
        }
        Err(error) => {
            out.insert("success".into(), json!(false));
            out.insert("error".into(), json!(error));
        }
    }
    Value::Object(out)
}

fn success_data(command: &Map<String, Value>) -> Value {
    match command.get("type").and_then(Value::as_str) {
        Some("switch_session" | "new_session") => json!({"cancelled": false}),
        Some(_) | None => json!({}),
    }
}

fn moved_response(outstanding: &Outstanding) -> Value {
    let mut out = Map::new();
    if let Some(id) = &outstanding.original_id {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), json!("response"));
    out.insert("command".into(), json!(outstanding.command));
    out.insert("success".into(), json!(false));
    out.insert("error".into(), json!("connection moved to another session"));
    Value::Object(out)
}

async fn write_frames(mut write: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Frame>) {
    let mut buf = Vec::new();
    while let Some(frame) = rx.recv().await {
        buf.clear();
        let end = match &frame {
            Frame::Record(line) => {
                buf.push(FRAME_RECORD);
                buf.extend_from_slice(line.as_bytes());
                false
            }
            Frame::Control(control) => {
                buf.push(FRAME_CONTROL);
                buf.extend_from_slice(serde_json::to_string(control.as_ref()).unwrap_or_default().as_bytes());
                matches!(
                    control.as_ref(),
                    AttachControl::End { .. } | AttachControl::Error { .. }
                )
            }
        };
        buf.push(b'\n');
        if write.write_all(&buf).await.is_err() || end {
            break;
        }
    }
    let _ = write.shutdown().await;
}

async fn write_control(write: &mut OwnedWriteHalf, control: &AttachControl) -> std::io::Result<()> {
    let mut line = vec![FRAME_CONTROL];
    line.extend_from_slice(serde_json::to_string(control).unwrap_or_default().as_bytes());
    line.push(b'\n');
    write.write_all(&line).await
}

async fn write_json_line<T: serde::Serialize>(write: &mut OwnedWriteHalf, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).unwrap_or_default();
    line.push(b'\n');
    write.write_all(&line).await
}
