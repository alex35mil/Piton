//! One task per pi process. It owns pi's stdin and stdout, routes responses to the client that asked, broadcasts
//! events, keeps the replay state that neither `get_state` nor `get_entries` contains, and runs the stop sequence.

mod commands;
mod info;
mod lifts;
mod network;
mod notice;
mod prompts;
mod replay;
mod view;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Notify, mpsc, oneshot},
    time::{Instant, sleep_until},
};

pub use self::{
    info::{Checkout, SessionInfo, SessionStatus},
    view::DEFAULT_WINDOW as DEFAULT_VIEW_WINDOW,
};
use self::{
    prompts::PromptQueue,
    replay::Replay,
    view::{Change, StateInput, View},
};
use crate::{
    config::DaemonConfig,
    daemon::{AttachControl, EndReason},
    launch::{self, Env, LaunchError, LaunchMode, SessionArg},
};

pub type ClientId = u64;
pub type SessionKey = u64;

/// A line for one client: a pi record or a control message.
#[derive(Debug, Clone)]
pub enum Frame {
    Record(Arc<str>),
    Control(Arc<AttachControl>),
}

pub type ClientTx = mpsc::UnboundedSender<Frame>;

/// A native client watching the session view: JSON lines (snapshot, changes, replies).
pub type WatchTx = mpsc::UnboundedSender<Arc<str>>;

/// Where a watcher resumes from.
#[derive(Debug, Clone, Copy)]
pub enum WatchFrom {
    /// A snapshot of the newest `limit` items.
    Snapshot { limit: usize },
    /// The changes after `seq`, or a snapshot when they're no longer kept.
    Seq { seq: u64, limit: usize },
}

/// A command from a native client, turned into pi RPC by the session task.
#[derive(Debug, Clone)]
pub enum ViewCommand {
    /// `message` is the client's id for this message; see [`Sent`].
    Send {
        message: String,
        text: String,
        mode: SendMode,
    },
    Abort,
    Answer {
        request_id: String,
        answer: Value,
    },
    /// A typed answer to a Piton question (`session.decide`).
    Decide {
        question: String,
        answer: Value,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum SendMode {
    /// A prompt when idle; queued as a follow-up when a run is active.
    Prompt,
    Steer,
    FollowUp,
}

/// A reply to a native client's request.
pub type ViewReply = oneshot::Sender<Result<Value, String>>;

/// A client command that was forwarded to pi and hasn't been answered.
#[derive(Debug, Clone)]
pub struct Outstanding {
    pub original_id: Option<Value>,
    pub command: String,
}

/// The commands piton itself sends to pi.
#[derive(Debug, Clone)]
pub enum Internal {
    /// The session's identity (id, file, name, model): at launch and after the conversation changes.
    GetState,
    /// The conversation, to (re)build the session view.
    GetEntries,
    /// A first prompt (`piton start --prompt`, `session.start {prompt}`).
    Prompt { text: String },
}

impl Internal {
    /// pi's name for the command, as in its responses.
    fn kind(&self) -> &'static str {
        match self {
            Internal::GetState => "get_state",
            Internal::GetEntries => "get_entries",
            Internal::Prompt { .. } => "prompt",
        }
    }

    fn rpc(&self, id: String) -> Value {
        match self {
            Internal::GetState | Internal::GetEntries => json!({"id": id, "type": self.kind()}),
            Internal::Prompt { text } => json!({"id": id, "type": self.kind(), "message": text}),
        }
    }
}

pub enum SessionCmd {
    Attach {
        client: ClientId,
        tx: ClientTx,
        done: oneshot::Sender<()>,
    },
    Detach {
        client: ClientId,
        done: oneshot::Sender<Vec<Outstanding>>,
    },
    FromClient {
        client: ClientId,
        command: Map<String, Value>,
    },
    /// A command from piton itself; its response only updates the session's own state.
    Internal(Internal),
    Watch {
        watcher: ClientId,
        tx: WatchTx,
        from: WatchFrom,
    },
    Unwatch {
        watcher: ClientId,
    },
    History {
        before: view::ItemId,
        limit: usize,
        reply: ViewReply,
    },
    ToolOutput {
        call_id: String,
        reply: ViewReply,
    },
    Image {
        image: view::ImageId,
        reply: ViewReply,
    },
    View {
        watcher: ClientId,
        command: ViewCommand,
        reply: ViewReply,
    },
    Stop {
        reason: StopReason,
        done: Option<oneshot::Sender<StopOutcome>>,
    },
    /// A protected write the agent asked for (agent.sock): asked of the user in every client, written on approval.
    ProtectedWrite {
        request: crate::agent::WriteRequest,
        reply: oneshot::Sender<crate::agent::WriteOutcome>,
    },
    /// The agent stopped waiting for a protected write (or got its answer): settle it if it's still pending.
    SettleProtectedWrite,
    /// A bash call the agent asks the daemon to run (agent.sock): with a per-command grant, or after the user approves.
    RunCommand {
        request: crate::agent::RunRequest,
        reply: oneshot::Sender<crate::agent::RunDecision>,
    },
    /// A call the daemon ran in the session's own sandbox failed: ask the user when a block it recorded is liftable.
    RunFailed {
        request: crate::agent::RunRequest,
        /// The last look: the kernel's record had its time.
        last: bool,
        reply: oneshot::Sender<crate::agent::Failed>,
    },
    /// The agent stopped waiting for a run request: settle its question if it's still pending.
    SettleQuestion,
    /// A refusal the kernel recorded for this session's sandbox.
    Refusal(crate::daemon::Refusal),
    /// A connection the gate holds: decided by the session, asking the user when nothing else does.
    NetworkAsk {
        user: crate::launch::sandbox::network::User,
        to: crate::launch::sandbox::Destination,
        reply: oneshot::Sender<crate::launch::sandbox::network::Verdict>,
    },
    /// A connection the gate refused.
    GateRefused(crate::launch::sandbox::network::Refused),
    /// fnox's proxy URL, reported once by the helper that starts pi behind it: the gate forwards through it.
    GateUpstream {
        url: String,
    },
    /// A `[sandbox.network.connections]` entry the user saved from a network question (in any session) in `file`: a
    /// session whose configuration reads `file` lets every sandbox reach it from now on, without a restart.
    NetworkEntrySaved {
        target: crate::config::NetworkTarget,
        file: PathBuf,
    },
    /// Code in pi's process asks to widen its sandbox for one action (agent.sock `lift`).
    Lift {
        request: crate::agent::lift::LiftRequest,
        reply: oneshot::Sender<crate::agent::lift::LiftDecision>,
    },
    /// The caller stopped waiting for a lift: settle its question if it's still pending.
    SettleLift,
    /// pi's `lifts` stream.
    Lifts(LiftsEvent),
}

/// What happens on pi's `lifts` stream.
#[derive(Debug)]
pub enum LiftsEvent {
    /// pi opened its stream: grants go to `out`; the session answers with the stream's number. `pi` is the stream's
    /// process, which the session token was accepted from: pi's own pid.
    Opened {
        out: mpsc::UnboundedSender<LiftsOut>,
        id: oneshot::Sender<u64>,
        pi: i32,
    },
    /// pi consumed grant `id`.
    Consumed { id: u64 },
    /// pi couldn't consume grant `id`, or the daemon couldn't issue it.
    Failed { id: u64, message: String },
    /// Stream `stream` closed.
    Closed { stream: u64 },
}

/// What the session sends on pi's `lifts` stream; the socket issues each grant's token for pi's process.
#[derive(Debug)]
pub enum LiftsOut {
    Grant {
        id: u64,
        lift: crate::launch::sandbox::grant::LiveLift,
        /// A process pi started was blocked: its MCP servers restart to inherit the grant.
        for_child: bool,
    },
    Revoke {
        id: u64,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum StopReason {
    /// `piton stop`, or an empty pi without clients.
    Requested,
    DaemonShutdown,
}

#[derive(Debug, Clone, Copy)]
pub struct StopOutcome {
    pub forced: bool,
}

/// What other tasks may read about a session without asking it.
#[derive(Debug, Clone)]
pub struct Shared {
    pub key: SessionKey,
    pub workspace: PathBuf,
    pub checkout: crate::session::Checkout,
    pub mode: LaunchMode,
    pub id: Option<String>,
    pub file: Option<String>,
    pub status: SessionStatus,
    pub clients: usize,
    pub pid: Option<u32>,
    pub has_messages: bool,
    pub title: Option<String>,
}

impl Shared {
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            key: self.key,
            id: self.id.clone(),
            file: self.file.clone(),
            workspace: self.workspace.clone(),
            checkout: self.checkout.clone(),
            mode: self.mode,
            status: self.status,
            clients: self.clients,
            pid: self.pid,
            title: self.title.clone(),
        }
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    pub key: SessionKey,
    pub tx: mpsc::UnboundedSender<SessionCmd>,
    pub shared: Arc<Mutex<Shared>>,
    /// The sandbox's tag, for a sandboxed session: the kernel's refusals carry it.
    pub tag: Option<String>,
    /// The session's view, decided at launch: what `facts` answers.
    pub facts: Arc<crate::agent::facts::Facts>,
}

impl SessionHandle {
    pub fn snapshot(&self) -> Shared {
        lock(&self.shared).clone()
    }

    pub async fn attach(&self, client: ClientId, tx: ClientTx) -> bool {
        let (done, wait) = oneshot::channel();
        if self.tx.send(SessionCmd::Attach { client, tx, done }).is_err() {
            return false;
        }
        wait.await.is_ok()
    }

    pub async fn detach(&self, client: ClientId) -> Vec<Outstanding> {
        let (done, wait) = oneshot::channel();
        if self.tx.send(SessionCmd::Detach { client, done }).is_err() {
            return Vec::new();
        }
        wait.await.unwrap_or_default()
    }

    pub async fn stop(&self, reason: StopReason) -> Option<StopOutcome> {
        let (done, wait) = oneshot::channel();
        self.tx
            .send(SessionCmd::Stop {
                reason,
                done: Some(done),
            })
            .ok()?;
        wait.await.ok()
    }
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Called by a session task when it ends, and when its status changes.
pub struct Hooks {
    pub ended: Box<dyn Fn(SessionKey) + Send + Sync>,
    pub status_changed: Arc<Notify>,
    /// Wakes view connections to re-send the session list (they skip unchanged lists).
    pub list_changed: tokio::sync::broadcast::Sender<()>,
    /// A `[sandbox.network.connections]` entry was saved from a network question: the daemon tells every live session.
    pub network_entry_saved: Box<dyn Fn(crate::config::NetworkTarget, PathBuf) + Send + Sync>,
}

pub struct LaunchSpec {
    pub key: SessionKey,
    pub workspace: crate::project::Workspace,
    pub mode: LaunchMode,
    pub session: SessionArg,
    pub env: Env,
    /// Behind fnox: the one-time token the helper that becomes pi reports its start with, in the helper's own
    /// environment.
    pub launch_token: Option<String>,
    /// The sandbox, for sandboxed launches: `piton sandbox-exec` reads it from the environment and removes it before
    /// pi starts.
    pub policy: Option<launch::sandbox::Policy>,
    /// The session's view (runtime, kept grants, protected files, global tier, workspace): what `facts` answers and
    /// what a protected write is checked against.
    pub facts: crate::agent::facts::Facts,
    /// The credential proxy pi starts behind, when fnox is used for this workspace.
    pub proxy: Option<launch::fnox::Proxy>,
    /// What commands the user approves run with, for sandboxed launches.
    pub commands: Option<launch::sandbox::grant::CommandPolicies>,
    /// The source checkout and the workspace's checkout.
    pub source: PathBuf,
    pub checkout: PathBuf,
    /// The `[[sandbox.commands]]` the launch resolved.
    pub grants: Vec<crate::config::Listed<crate::config::CommandGrant>>,
    /// The configuration levels the launch read.
    pub levels: Vec<PathBuf>,
    /// What the session starts with a warning about.
    pub warnings: Vec<launch::LaunchWarning>,
    /// For a filtered network: pi's trusted hosts, and where the gate reaches hosts (`None`: behind fnox, reported once
    /// fnox runs).
    pub network: Option<NetworkSpec>,
    /// Whether pi loads the project's own resources (`--approve`), from the user's trust decision.
    pub project_trusted: bool,
}

/// What a session with a filtered network starts its gate with.
pub struct NetworkSpec {
    pub catalog: crate::pi::catalog::Catalog,
    pub route: Option<crate::launch::sandbox::network::Route>,
    /// The `ssh` wrapper's folder, when the daemon made it.
    pub ssh: Option<PathBuf>,
}

/// The gate of a session with a filtered network, deciding through the session's channel.
fn start_gate(spec: NetworkSpec, tx: &mpsc::UnboundedSender<SessionCmd>) -> Result<network::Network, LaunchError> {
    let asks = tx.clone();
    let decide: crate::launch::sandbox::network::Decide = Arc::new(move |user, to| {
        let asks = asks.clone();
        Box::pin(async move {
            let (reply, verdict) = oneshot::channel();
            let gone = crate::launch::sandbox::network::Verdict::Deny {
                reason: "Piton's sandbox refused it: the session ended".into(),
            };
            if asks.send(SessionCmd::NetworkAsk { user, to, reply }).is_err() {
                return gone;
            }
            verdict.await.unwrap_or(gone)
        })
    });
    let (refused_tx, mut refused_rx) = mpsc::unbounded_channel();
    let refusals = tx.clone();
    tokio::spawn(async move {
        while let Some(refused) = refused_rx.recv().await {
            if refusals.send(SessionCmd::GateRefused(refused)).is_err() {
                return;
            }
        }
    });
    let gate = crate::launch::sandbox::network::Gate::start(decide, spec.route, refused_tx).map_err(|source| {
        LaunchError::Spawn {
            program: "the network gate".into(),
            source,
        }
    })?;
    Ok(network::Network::new(gate, spec.catalog, spec.ssh))
}

pub fn spawn(config: Arc<DaemonConfig>, mut spec: LaunchSpec, hooks: Hooks) -> Result<SessionHandle, LaunchError> {
    let helper = launch::helper()?;
    let argv = launch::command_line(&config, spec.mode, &spec.session, spec.project_trusted, &helper);
    let encoding = |e: serde_json::Error| LaunchError::Spawn {
        program: "launch environment".into(),
        source: std::io::Error::other(e),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    // A filtered network's gate, with pi's own proxy user in its policy.
    let mut extra = Env::new();
    if let Some(token) = &spec.launch_token {
        extra.insert(crate::agent::LAUNCH_TOKEN_ENV.into(), token.clone());
    }
    let mut network = match spec.network.take() {
        Some(network) => Some(start_gate(network, &tx)?),
        None => None,
    };
    if let (Some(network), Some(policy)) = (&mut network, &mut spec.policy) {
        network.issue(network::PI_USER, policy);
    }
    if let Some(policy) = &spec.policy {
        extra.insert(
            launch::sandbox::POLICY_ENV.into(),
            serde_json::to_string(policy).map_err(encoding)?,
        );
    }
    // Behind fnox, fnox runs with the daemon's base environment and pi's travels past it in the handoff, which this
    // binary unpacks: `sandbox-exec` for a sandboxed launch, `launch-exec` for a host one.
    let (argv, env) = match (&spec.proxy, spec.mode) {
        (None, LaunchMode::Host) => (argv, spec.env.clone()),
        // The helper is unsandboxed until it applies the policy, and pi's environment includes the workspace's (which a
        // sandboxed agent could have set, `DYLD_*` included): it reaches only pi, through the handoff.
        (None, LaunchMode::Sandbox) => (argv, launch::handoff::for_helper(&spec.env, extra).map_err(encoding)?),
        (Some(proxy), LaunchMode::Sandbox) => (
            proxy.wrap(argv),
            proxy.environment(&spec.env, &extra).map_err(encoding)?,
        ),
        (Some(proxy), LaunchMode::Host) => {
            let mut through = vec![helper.to_string_lossy().into_owned(), "launch-exec".into(), "--".into()];
            through.extend(argv);
            (
                proxy.wrap(through),
                proxy.environment(&spec.env, &extra).map_err(encoding)?,
            )
        }
    };
    // Argument values can contain provider credentials; only structural metadata belongs in persistent logs.
    crate::debug!(
        "session {}: launching {:?} with {} arguments",
        spec.key,
        spec.mode,
        argv.len()
    );
    let (program, args) = argv.split_first().expect("command line is never empty");
    // Look the program up in pi's PATH: under launchd the daemon's own PATH doesn't have pi.
    let program = &launch::resolve_program(program, &spec.env);
    let mut child = Command::new(program)
        .args(args)
        .current_dir(&spec.workspace.path)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| LaunchError::Spawn {
            program: program.clone(),
            source,
        })?;

    let checkout = match &spec.workspace.relation {
        crate::project::Relation::Source => crate::session::Checkout::Source,
        crate::project::Relation::Worker { source, name } => crate::session::Checkout::Worker {
            source: source.clone(),
            name: name.clone(),
        },
    };
    let shared = Arc::new(Mutex::new(Shared {
        key: spec.key,
        checkout,
        workspace: spec.workspace.path.clone(),
        mode: spec.mode,
        id: None,
        file: match &spec.session {
            SessionArg::New => None,
            SessionArg::Resume(file) => Some(file.to_string_lossy().into_owned()),
        },
        status: SessionStatus::Idle,
        clients: 0,
        pid: child.id(),
        has_messages: matches!(spec.session, SessionArg::Resume(_)),
        title: None,
    }));
    let handle = SessionHandle {
        key: spec.key,
        tx,
        shared: shared.clone(),
        tag: spec.policy.as_ref().map(|p| p.tag.clone()).filter(|t| !t.is_empty()),
        facts: Arc::new(spec.facts.clone()),
    };

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (pi_tx, mut pi_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(line) = pi_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
        }
        // Dropping stdin closes it: pi's orderly-shutdown signal.
    });

    let session_tag = spec.policy.as_ref().map(|p| p.tag.clone()).unwrap_or_default();
    let commands = spec.commands.map(|policies| commands::Commands {
        policies,
        shell: crate::agent::PiShell::read(&spec.env, &config.paths.home, &spec.workspace.path),
        env: spec.env.clone(),
        source: spec.source.clone(),
        checkout: spec.checkout.clone(),
        paths: config.paths.clone(),
        grants: spec.grants.clone(),
        levels: spec.levels.clone(),
        session_grants: HashMap::new(),
    });
    let task = SessionTask {
        config,
        protected: spec.facts.protected,
        protected_write: None,
        commands,
        calls: commands::Calls::new(&session_tag),
        question: None,
        process: commands::ProcessBlocks::default(),
        shared,
        hooks,
        pi_tx: Some(pi_tx),
        clients: BTreeMap::new(),
        pending: HashMap::new(),
        next_internal: 0,
        replay: Replay::default(),
        startup: Startup {
            warnings: spec.warnings,
            fnox: spec.proxy.as_ref().map(|_| launch::fnox::StartupLog::default()),
            announced: false,
        },
        stderr_tail: Vec::new(),
        writer: None,
        stopping: None,
        stop_waiters: Vec::new(),
        empty_deadline: None,
        pgid: child.id().map(|pid| Pid::from_raw(pid as i32)),
        view: View::new(spec.key),
        watchers: BTreeMap::new(),
        sent: HashMap::new(),
        sent_order: VecDeque::new(),
        prompts: PromptQueue::default(),
        network,
        lifts: lifts::Lifts::default(),
        lifts_out: None,
        pi_pid: None,
        next_stream: 1,
        lift: None,
    };
    tokio::spawn(task.run(
        child,
        rx,
        BufReader::new(stdout).lines(),
        BufReader::new(stderr).lines(),
    ));

    let _ = handle.tx.send(SessionCmd::Internal(Internal::GetState));
    // Builds the view from the saved conversation (empty for a new session).
    let _ = handle.tx.send(SessionCmd::Internal(Internal::GetEntries));
    Ok(handle)
}

/// A command pi hasn't answered yet, keyed by the id piton sent it with.
enum Pending {
    Client {
        client: ClientId,
        outstanding: Outstanding,
    },
    Internal(Internal),
    View {
        command: String,
        reply: ViewReply,
    },
    /// A native client's message; its replies wait in [`SessionTask::sent`].
    Send {
        command: String,
        message: String,
    },
}

/// A native client's message by its id. A client that lost its connection before the reply sends the message again
/// with the same id; it must reach pi only once.
enum Sent {
    /// Forwarded to pi, no response yet: every send of it gets pi's response.
    Waiting(Vec<ViewReply>),
    /// pi accepted it: a repeat gets the same reply. (A refused message is forgotten, so a repeat tries again.)
    Accepted(Value),
}

/// A protected write the user is being asked about: the dialog's id, and where the outcome goes.
struct PendingWrite {
    id: String,
    /// The folder the write starts from (the checkout, a configuration level, pi's agent directory, a linked file's
    /// folder): it walks from here without following symlinks.
    root: PathBuf,
    path: PathBuf,
    content: String,
    reply: oneshot::Sender<crate::agent::WriteOutcome>,
}

/// How many message ids a session remembers for repeats.
const SENT_KEPT: usize = 256;

enum StopPhase {
    /// stdin closed; SIGTERM at the deadline.
    Closing,
    /// SIGTERM sent; SIGKILL at the deadline.
    Terminating,
    Killed,
}

struct SessionTask {
    config: Arc<DaemonConfig>,
    protected: crate::agent::Protected,
    /// The protected write waiting for the user: one per session.
    protected_write: Option<PendingWrite>,
    /// What approved commands run with: sandboxed sessions only.
    commands: Option<commands::Commands>,
    /// The bash calls pi is running, and whose sandbox each tag is.
    calls: commands::Calls,
    /// The block question waiting for the user: one per session, sharing the slot with protected writes.
    question: Option<commands::PendingQuestion>,
    /// A blocked process's question (an MCP server, an extension), and the refusals offered already.
    process: commands::ProcessBlocks,
    shared: Arc<Mutex<Shared>>,
    hooks: Hooks,
    pi_tx: Option<mpsc::UnboundedSender<String>>,
    clients: BTreeMap<ClientId, ClientTx>,
    pending: HashMap<String, Pending>,
    next_internal: u64,
    replay: Replay,
    /// How this pi started: the launch's warnings and fnox's.
    startup: Startup,
    stderr_tail: Vec<String>,
    writer: Option<ClientId>,
    stopping: Option<(StopPhase, Instant)>,
    stop_waiters: Vec<oneshot::Sender<StopOutcome>>,
    empty_deadline: Option<Instant>,
    pgid: Option<Pid>,
    view: View,
    watchers: BTreeMap<ClientId, Watcher>,
    /// Native clients' messages by id, oldest first in `sent_order`.
    sent: HashMap<String, Sent>,
    sent_order: VecDeque<String>,
    /// pi gets one `prompt` at a time; see [`PromptQueue`].
    prompts: PromptQueue,
    /// A filtered network's gate and its questions: sandboxed sessions whose network isn't open.
    network: Option<network::Network>,
    /// What pi's live sandbox is widened for, and its stream.
    lifts: lifts::Lifts,
    /// pi's open `lifts` stream: where grants go, and its number (a closed stream ends only its own grants).
    lifts_out: Option<(u64, mpsc::UnboundedSender<LiftsOut>)>,
    /// pi's pid, from its stream: a refusal of another pid is a process pi started (an MCP server).
    pi_pid: Option<i32>,
    next_stream: u64,
    /// An explicit lift waiting for the user: one at a time, sharing the slot with every other question.
    lift: Option<PendingLift>,
}

/// An explicit lift the user is being asked about.
struct PendingLift {
    id: String,
    targets: Vec<crate::launch::sandbox::grant::LiveLift>,
    reply: oneshot::Sender<crate::agent::lift::LiftDecision>,
}

struct Watcher {
    tx: WatchTx,
    limit: usize,
    /// Waiting for the rebuild to finish before getting a snapshot.
    awaiting_snapshot: bool,
}

const STDERR_TAIL: usize = 20;

/// How this pi started, for the user: the launch's warnings, the secrets fnox couldn't read (for a session behind the
/// proxy), and whether attached clients were told yet. fnox prints its warnings before it starts pi, so everything is
/// announced once pi's first record arrives.
#[derive(Default)]
struct Startup {
    warnings: Vec<launch::LaunchWarning>,
    fnox: Option<launch::fnox::StartupLog>,
    announced: bool,
}

impl Startup {
    /// The conversation's notice: the launch's warnings, then fnox's, one paragraph each.
    fn notice(&self) -> Option<String> {
        let lines: Vec<String> = self
            .warnings
            .iter()
            .map(ToString::to_string)
            .chain(self.fnox.as_ref().and_then(launch::fnox::StartupLog::notice))
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n\n"))
    }
}

/// The id of the records the daemon sends about how pi started in pi's `extension_ui_request` format.
const STARTUP_UI_ID: &str = "piton:startup";

/// The folders where host processes create sockets and files of their own: the temp roots and pi's `$TMPDIR`, as
/// written and real. No folder at or below them is offered from the block question.
fn shared_folders(env: &launch::Env) -> Vec<std::path::PathBuf> {
    let mut folders: Vec<std::path::PathBuf> = launch::sandbox::WRITE_ROOTS
        .iter()
        .map(std::path::PathBuf::from)
        .chain(["/tmp", "/var/folders"].map(std::path::PathBuf::from))
        .collect();
    if let Some(tmp) = env.get("TMPDIR").filter(|t| !t.is_empty()) {
        let tmp = std::path::PathBuf::from(tmp);
        folders.push(crate::agent::canonical_target(&tmp));
        folders.push(tmp);
    }
    folders
}

/// Where an entry saved from the block question goes, in the sandbox's terms.
/// A live lift's path with symlinks resolved, as Seatbelt matches: a file that doesn't exist yet keeps its name under
/// its folder's real path.
fn resolve_live(lift: &launch::sandbox::grant::LiveLift) -> launch::sandbox::grant::LiveLift {
    use launch::sandbox::grant::LiveLift;
    match lift {
        LiveLift::Read { path } => LiveLift::Read {
            path: crate::agent::canonical_target(path),
        },
        LiveLift::Write { path } => LiveLift::Write {
            path: crate::agent::canonical_target(path),
        },
        LiveLift::Service { .. } | LiveLift::Listen => lift.clone(),
    }
}

fn grant_scope(scope: crate::agent::block::PersistScope) -> launch::sandbox::grant::Scope {
    match scope {
        crate::agent::block::PersistScope::Project => launch::sandbox::grant::Scope::Project,
        crate::agent::block::PersistScope::Global => launch::sandbox::grant::Scope::Everywhere,
    }
}

/// The warning notification for an attached client (pi.nvim), when the startup notice has something.
fn startup_notify(startup: &Startup) -> Option<Arc<str>> {
    let message = startup.notice()?;
    let record = json!({
        "type": "extension_ui_request",
        "id": STARTUP_UI_ID,
        "method": "notify",
        "notifyType": "warning",
        "message": message,
    });
    Some(record.to_string().into())
}

impl SessionTask {
    async fn run(
        mut self,
        mut child: Child,
        mut rx: mpsc::UnboundedReceiver<SessionCmd>,
        mut stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        mut stderr: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    ) {
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut end_reason = EndReason::PiExited;
        loop {
            let stop_deadline = self.stopping.as_ref().map(|(_, at)| *at);
            let question_deadline = self.question.as_ref().and_then(|q| q.deadline);
            let network_deadline = self
                .network
                .as_ref()
                .and_then(|n| n.asking.as_ref())
                .and_then(|a| a.deadline);
            tokio::select! {
                line = stdout.next_line(), if stdout_open => match line {
                    Ok(Some(line)) => self.on_pi_line(line),
                    Ok(None) | Err(_) => stdout_open = false,
                },
                line = stderr.next_line(), if stderr_open => match line {
                    Ok(Some(line)) => {
                        if self.startup.fnox.as_mut().is_some_and(|log| log.read(&line)) {
                            self.on_unresolved_secrets();
                        }
                        self.stderr_tail.push(line);
                        if self.stderr_tail.len() > STDERR_TAIL {
                            self.stderr_tail.remove(0);
                        }
                    }
                    Ok(None) | Err(_) => stderr_open = false,
                },
                cmd = rx.recv() => match cmd {
                    Some(SessionCmd::Stop { reason, done }) => {
                        if let StopReason::DaemonShutdown = reason {
                            end_reason = EndReason::DaemonShutdown;
                        }
                        self.begin_stop(done);
                    }
                    Some(cmd) => self.on_cmd(cmd),
                    // Every handle is gone: nothing can reach this session any more.
                    None => self.begin_stop(None),
                },
                _ = child.wait(), if !stdout_open => break,
                _ = sleep_until(stop_deadline.unwrap_or_else(Instant::now)), if stop_deadline.is_some() => {
                    self.advance_stop();
                }
                _ = sleep_until(question_deadline.unwrap_or_else(Instant::now)), if question_deadline.is_some() => {
                    self.expire_question();
                }
                _ = sleep_until(network_deadline.unwrap_or_else(Instant::now)), if network_deadline.is_some() => {
                    self.expire_network_question();
                }
                _ = sleep_until(self.empty_deadline.unwrap_or_else(Instant::now)), if self.empty_deadline.is_some() => {
                    self.empty_deadline = None;
                    if self.is_empty_and_unused() {
                        crate::log!("session {}: stopping empty pi without clients", lock(&self.shared).key);
                        self.begin_stop(None);
                    }
                }
            }
        }
        // Drain the rest of stderr for the report.
        while let Ok(Some(line)) = stderr.next_line().await {
            self.stderr_tail.push(line);
        }
        let keep = self.stderr_tail.len().saturating_sub(STDERR_TAIL);
        self.stderr_tail.drain(..keep);

        let forced = matches!(self.stopping, Some((StopPhase::Terminating | StopPhase::Killed, _)));
        let stderr_lines = match end_reason {
            EndReason::PiExited => self.stderr_tail.clone(),
            EndReason::DaemonShutdown => Vec::new(),
        };
        let end = Frame::Control(Arc::new(AttachControl::End {
            reason: end_reason,
            stderr: stderr_lines,
        }));
        for tx in self.clients.values() {
            let _ = tx.send(end.clone());
        }
        for waiter in self.stop_waiters.drain(..) {
            let _ = waiter.send(StopOutcome { forced });
        }
        let key = lock(&self.shared).key;
        let exit = match child.try_wait() {
            Ok(Some(status)) => status.to_string(),
            Ok(None) | Err(_) => "unknown status".into(),
        };
        let requested = match self.stopping {
            Some(_) => "stopped",
            None => "pi exited on its own",
        };
        crate::log!(
            "session {key}: ended ({requested}, {exit}{}), {} client(s) told {end_reason:?}",
            if forced { ", force-stopped" } else { "" },
            self.clients.len()
        );
        if self.stopping.is_none() || forced {
            // pi's stderr is forwarded to attached clients above, not persisted: extensions/providers may print secrets.
            crate::log!(
                "session {key}: {} stderr lines withheld from persistent logging",
                self.stderr_tail.len()
            );
        }
        (self.hooks.ended)(key);
        self.hooks.status_changed.notify_one();
    }

    fn on_cmd(&mut self, cmd: SessionCmd) {
        match cmd {
            SessionCmd::Attach { client, tx, done } => {
                for line in self.replay.frames() {
                    let _ = tx.send(Frame::Record(line));
                }
                // A client that attaches after the announcement gets the notification too.
                if let Some(notify) = Some(&self.startup).filter(|s| s.announced).and_then(startup_notify) {
                    let _ = tx.send(Frame::Record(notify));
                }
                self.clients.insert(client, tx);
                self.empty_deadline = None;
                self.sync_shared();
                let _ = done.send(());
            }
            SessionCmd::Detach { client, done } => {
                self.clients.remove(&client);
                let mut outstanding = Vec::new();
                self.pending.retain(|_, p| match p {
                    Pending::Client {
                        client: c,
                        outstanding: o,
                    } if *c == client => {
                        outstanding.push(o.clone());
                        false
                    }
                    // A view command's reply belongs to a watcher, not an rpc client.
                    Pending::Client { .. } | Pending::Internal(_) | Pending::View { .. } | Pending::Send { .. } => true,
                });
                if self.writer == Some(client) {
                    self.writer = None;
                }
                self.sync_shared();
                self.arm_empty_timer();
                let _ = done.send(outstanding);
            }
            SessionCmd::FromClient { client, command } => self.handle_client_command(client, command),
            SessionCmd::Internal(command) => self.internal(command),
            SessionCmd::Watch { watcher, tx, from } => self.watch(watcher, tx, from),
            SessionCmd::Unwatch { watcher } => {
                self.watchers.remove(&watcher);
                self.sync_shared();
                self.arm_empty_timer();
            }
            SessionCmd::History { before, limit, reply } => {
                let _ = reply.send(Ok(self.view.history(before, limit)));
            }
            SessionCmd::ToolOutput { call_id, reply } => {
                let _ = reply.send(
                    self.view
                        .tool_output(&call_id)
                        .cloned()
                        .ok_or_else(|| format!("no output for tool call {call_id}")),
                );
            }
            SessionCmd::Image { image, reply } => {
                let _ = reply.send(
                    self.view
                        .image(image)
                        .map(|i| serde_json::json!(i))
                        .ok_or_else(|| format!("no image {image}")),
                );
            }
            SessionCmd::View {
                watcher,
                command,
                reply,
            } => self.view_command(watcher, command, reply),
            SessionCmd::ProtectedWrite { request, reply } => self.protected_write(request, reply),
            SessionCmd::SettleProtectedWrite => {
                self.settle_protected_write();
                self.ask_next_process();
                self.publish_view_state();
            }
            SessionCmd::RunCommand { request, reply } => self.run_command(request, reply),
            SessionCmd::RunFailed { request, last, reply } => self.run_failed(request, last, reply),
            SessionCmd::SettleQuestion => {
                self.settle_question();
                self.ask_next_process();
                self.publish_view_state();
            }
            SessionCmd::Refusal(refusal) => self.on_refusal(refusal),
            SessionCmd::NetworkAsk { user, to, reply } => self.network_ask(user, to, reply),
            SessionCmd::GateRefused(refused) => self.gate_refused(refused),
            SessionCmd::GateUpstream { url } => self.gate_upstream(&url),
            SessionCmd::NetworkEntrySaved { target, file } => self.network_entry_saved(target, file),
            SessionCmd::Lift { request, reply } => self.lift(request, reply),
            SessionCmd::SettleLift => {
                self.settle_lift();
                self.ask_next_process();
                self.publish_view_state();
            }
            SessionCmd::Lifts(event) => self.lifts_event(event),
            SessionCmd::Stop { .. } => unreachable!("handled in run"),
        }
    }

    /// A protected write the agent asked for: refused unless the path is one of this session's protected files and no
    /// other is pending; otherwise asked of the user with a dialog every client shows. Every protected write asks.
    fn protected_write(
        &mut self,
        request: crate::agent::WriteRequest,
        reply: oneshot::Sender<crate::agent::WriteOutcome>,
    ) {
        use crate::agent::{WriteOutcome, write_dialog};
        let refuse = |reason: String| WriteOutcome::Refused { reason };
        // The path as the file system will resolve it: the existing parent's canonical path and the name.
        let path = crate::agent::canonical_target(&request.path);
        let Some(file) = self.protected.check(&path) else {
            let _ = reply.send(refuse(format!(
                "{} isn't a protected file of this session; write it with the write or edit tool",
                request.path.display()
            )));
            return;
        };
        if self.asking() {
            let _ = reply.send(refuse(
                "another question is waiting for the user; try again after it's answered".into(),
            ));
            return;
        }
        let before = std::fs::read_to_string(&path).ok();
        let key = lock(&self.shared).key;
        self.next_internal += 1;
        let id = format!("piton:write:{}", self.next_internal);
        crate::log!("session {key}: asking about a protected write to {}", path.display());
        let record = write_dialog(&id, &path, file.controls, before.as_deref(), &request.content);
        self.protected_write = Some(PendingWrite {
            id,
            root: file.root,
            path,
            content: request.content,
            reply,
        });
        // To every client like a dialog of pi's, and kept for replay until it's answered.
        self.on_pi_line(record.to_string());
    }

    /// A client's answer to the pending protected write's dialog: write or not, and tell the agent.
    fn answer_protected_write(&mut self, answer: &Map<String, Value>) {
        use crate::agent::{WriteAnswer, WriteApproval, WriteOutcome, answer::Once, read_write_answer, write_approved};
        let Some(pending) = self.protected_write.take() else {
            return;
        };
        let key = lock(&self.shared).key;
        let outcome = match read_write_answer(answer) {
            WriteAnswer::Narrowest(Once(WriteApproval { content, notes })) => {
                let modified = content.as_ref().is_some_and(|c| *c != pending.content);
                let content = content.unwrap_or(pending.content);
                match write_approved(&pending.root, &pending.path, &content) {
                    Ok(()) => {
                        crate::log!("session {key}: protected write to {} approved", pending.path.display());
                        WriteOutcome::Approved {
                            modified,
                            content,
                            notes,
                        }
                    }
                    Err(e) => WriteOutcome::Refused {
                        reason: format!("approved, but can't write {}: {e}", pending.path.display()),
                    },
                }
            }
            WriteAnswer::Deny(notes) => {
                crate::log!("session {key}: protected write to {} rejected", pending.path.display());
                WriteOutcome::Rejected { notes }
            }
            WriteAnswer::Session(never) | WriteAnswer::Always(never) => match never {},
        };
        let _ = pending.reply.send(outcome);
        self.ask_next_process();
    }

    fn watch(&mut self, watcher: ClientId, tx: WatchTx, from: WatchFrom) {
        let limit = match from {
            WatchFrom::Snapshot { limit } | WatchFrom::Seq { limit, .. } => limit,
        };
        let resumed = match from {
            WatchFrom::Seq { seq, .. } if !self.view.reset_pending() => self.view.changes_since(seq),
            WatchFrom::Seq { .. } | WatchFrom::Snapshot { .. } => None,
        };
        let awaiting_snapshot = match resumed {
            Some(lines) => {
                for line in lines {
                    let _ = tx.send(line);
                }
                false
            }
            None if self.view.reset_pending() => {
                crate::log!(
                    "session {}: watcher {watcher} waits for the view rebuild to finish",
                    lock(&self.shared).key
                );
                true
            }
            None => {
                let _ = tx.send(self.view.snapshot(limit).to_string().into());
                false
            }
        };
        self.watchers.insert(
            watcher,
            Watcher {
                tx,
                limit,
                awaiting_snapshot,
            },
        );
        self.empty_deadline = None;
        self.sync_shared();
    }

    fn send_waiting_snapshots(&mut self) {
        for w in self.watchers.values_mut().filter(|w| w.awaiting_snapshot) {
            let _ = w.tx.send(self.view.snapshot(w.limit).to_string().into());
            w.awaiting_snapshot = false;
        }
    }

    /// Sends committed view changes to every watcher that has its snapshot.
    fn broadcast(&mut self, changes: Vec<Change>) {
        for change in changes {
            let committed = self.view.commit(change);
            for w in self.watchers.values() {
                if !w.awaiting_snapshot {
                    let _ = w.tx.send(committed.line.clone());
                }
            }
            if committed.reset {
                // After a reset every watcher needs a new snapshot.
                for w in self.watchers.values_mut() {
                    let _ = w.tx.send(self.view.snapshot(w.limit).to_string().into());
                    w.awaiting_snapshot = false;
                }
            }
        }
    }

    fn publish_view_state(&mut self) {
        // The block question goes to `view` clients typed; its `rpc` rendering is for `piton attach` only.
        let rpc_only = self.question.as_ref().map(|q| q.dialog_id.as_str());
        let dialogs: Vec<Value> = self
            .replay
            .pending_dialogs()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|d| d.get("id").and_then(Value::as_str) != rpc_only || rpc_only.is_none())
            .collect();
        let questions: Vec<Value> = self.question.iter().map(|q| q.question.view()).collect();
        let input = {
            let shared = lock(&self.shared);
            StateInput {
                status: shared.status,
                workspace: shared.workspace.clone(),
                mode: shared.mode,
                session_id: shared.id.clone(),
                session_file: shared.file.clone(),
                dialogs,
                questions,
            }
        };
        if let Some(change) = self.view.publish_state(input) {
            lock(&self.shared).title = self.view.title();
            // The id, file, or title may have changed.
            let _ = self.hooks.list_changed.send(());
            self.broadcast(vec![change]);
        }
    }

    /// A native client's command: steering commands make it the writer, like pi RPC from an rpc client.
    fn view_command(&mut self, watcher: ClientId, command: ViewCommand, reply: ViewReply) {
        let (rpc, pending) = match command {
            ViewCommand::Send { message, text, mode } => {
                match self.sent.get_mut(&message) {
                    Some(Sent::Waiting(replies)) => {
                        replies.push(reply);
                        return;
                    }
                    Some(Sent::Accepted(data)) => {
                        let _ = reply.send(Ok(data.clone()));
                        return;
                    }
                    None => {}
                }
                // pi decides whether a run is active: it starts a run when idle and applies `streamingBehavior` only
                // during one. The session's status can't decide it: it follows pi's events, which lag behind (a
                // message sent right after another would find the session still idle, and pi would refuse it).
                let behavior = match mode {
                    SendMode::Prompt | SendMode::FollowUp => "followUp",
                    SendMode::Steer => "steer",
                };
                let rpc = json!({"type": "prompt", "message": text, "streamingBehavior": behavior});
                self.remember_sent(message.clone(), Sent::Waiting(vec![reply]));
                let command = command_type(rpc.as_object().expect("an object"));
                (rpc, Pending::Send { command, message })
            }
            ViewCommand::Abort => {
                let rpc = json!({"type": "abort"});
                let command = command_type(rpc.as_object().expect("an object"));
                (rpc, Pending::View { command, reply })
            }
            ViewCommand::Decide { question, answer } => {
                let result = self.decide(&question, &answer);
                let _ = reply.send(result.map(|()| json!({})));
                self.sync_shared();
                self.publish_view_state();
                return;
            }
            ViewCommand::Answer { request_id, answer } => {
                let mut response = match answer {
                    Value::Object(map) => map,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
                        let _ = reply.send(Err("answer must be an object".into()));
                        return;
                    }
                };
                response.insert("type".into(), json!("extension_ui_response"));
                response.insert("id".into(), json!(request_id));
                let answered = self.replay.has_dialog(&request_id);
                self.handle_client_command(watcher, response);
                let _ = reply.send(match answered {
                    true => Ok(json!({})),
                    false => Err("that question was already answered or expired".into()),
                });
                self.publish_view_state();
                return;
            }
        };
        let Value::Object(mut command) = rpc else { return };
        self.next_internal += 1;
        let id = format!("piton:v{watcher}:{}", self.next_internal);
        self.writer = Some(watcher);
        command.insert("id".into(), Value::String(id.clone()));
        self.pending.insert(id, pending);
        self.send_to_pi(&Value::Object(command));
    }

    fn remember_sent(&mut self, message: String, sent: Sent) {
        if self.sent.insert(message.clone(), sent).is_none() {
            self.sent_order.push_back(message);
        }
        while self.sent_order.len() > SENT_KEPT {
            if let Some(oldest) = self.sent_order.pop_front() {
                self.sent.remove(&oldest);
            }
        }
    }

    /// pi answered a native client's message: reply to every send of it, and remember an accepted one for repeats.
    fn answer_sent(&mut self, message: &str, result: Result<Value, String>) {
        let waiting = match self.sent.remove(message) {
            Some(Sent::Waiting(replies)) => replies,
            Some(Sent::Accepted(_)) | None => Vec::new(),
        };
        for reply in waiting {
            let _ = reply.send(result.clone());
        }
        match result {
            Ok(data) => {
                self.sent.insert(message.to_string(), Sent::Accepted(data));
            }
            Err(_) => self.sent_order.retain(|m| m != message),
        }
    }

    fn handle_client_command(&mut self, client: ClientId, mut command: Map<String, Value>) {
        let kind = command_type(&command);
        match kind.as_str() {
            "extension_ui_response" => {
                let id = command.get("id").and_then(Value::as_str).map(str::to_string);
                let Some(id) = id else { return };
                if !self.replay.answer_dialog(&id) {
                    // Already answered by another client, expired, or unknown: pi would ignore it.
                    return;
                }
                // The daemon's own dialogs: answered here, never sent to pi (which didn't ask).
                if self.protected_write.as_ref().is_some_and(|w| w.id == id) {
                    self.answer_protected_write(&command);
                    // The status first, then the state with it: the dialog closes in every client.
                    self.sync_shared();
                    self.publish_view_state();
                    return;
                }
                if self.question.as_ref().is_some_and(|q| q.dialog_id == id) {
                    self.answer_question(&command);
                    self.sync_shared();
                    self.publish_view_state();
                    return;
                }
                if self.process.asking(&id) {
                    self.answer_process(&command);
                    self.sync_shared();
                    self.publish_view_state();
                    return;
                }
                if self.lift.as_ref().is_some_and(|l| l.id == id) {
                    self.answer_lift(&command);
                    self.sync_shared();
                    self.publish_view_state();
                    return;
                }
                if self
                    .network
                    .as_ref()
                    .and_then(|n| n.asking.as_ref())
                    .is_some_and(|a| a.dialog_id == id)
                {
                    self.answer_network(&command);
                    self.sync_shared();
                    self.publish_view_state();
                    return;
                }
                self.writer = Some(client);
                self.sync_shared();
                self.send_to_pi(&Value::Object(command));
                return;
            }
            "prompt" | "steer" | "follow_up" | "abort" | "bash" => self.writer = Some(client),
            _ => {}
        }
        self.next_internal += 1;
        let id = format!("piton:c{client}:{}", self.next_internal);
        let original_id = command.insert("id".into(), Value::String(id.clone()));
        self.pending.insert(
            id,
            Pending::Client {
                client,
                outstanding: Outstanding {
                    original_id,
                    command: kind,
                },
            },
        );
        self.send_to_pi(&Value::Object(command));
    }

    /// Writes a command to pi. A `prompt` waits until pi answered the one before it ([`PromptQueue`]).
    fn send_to_pi(&mut self, value: &Value) {
        let prompt_id = match (
            value.get("type").and_then(Value::as_str),
            value.get("id").and_then(Value::as_str),
        ) {
            (Some("prompt"), Some(id)) => Some(id.to_string()),
            (Some(_) | None, Some(_) | None) => None,
        };
        match prompt_id {
            Some(id) => {
                if let Some(prompt) = self.prompts.submit(id, value.clone()) {
                    self.write_to_pi(&prompt);
                }
            }
            None => self.write_to_pi(value),
        }
    }

    fn write_to_pi(&mut self, value: &Value) {
        if let Some(tx) = &self.pi_tx {
            let _ = tx.send(value.to_string());
        }
    }

    /// fnox named another secret it couldn't read: the conversation's notice and the status entry show the list.
    fn on_unresolved_secrets(&mut self) {
        let Some(status) = self.startup.fnox.as_ref().and_then(launch::fnox::StartupLog::status) else {
            return;
        };
        if let Some(notice) = self.startup.notice() {
            let changes = self.view.set_startup_notice(notice);
            self.broadcast(changes);
        }
        let record = json!({
            "type": "extension_ui_request",
            "id": STARTUP_UI_ID,
            "method": "setStatus",
            "statusKey": "piton-fnox",
            "statusText": status,
        });
        self.emit_record(record);
        // A warning read after pi's first record: tell the clients again, with the whole notice.
        if let Some(notify) = Some(&self.startup).filter(|s| s.announced).and_then(startup_notify) {
            for tx in self.clients.values() {
                let _ = tx.send(Frame::Record(notify.clone()));
            }
        }
    }

    /// pi is up, so fnox has printed its startup warnings: show the startup notice and tell the attached clients once.
    fn announce_startup(&mut self) {
        if self.startup.announced {
            return;
        }
        self.startup.announced = true;
        if !self.startup.warnings.is_empty()
            && let Some(notice) = self.startup.notice()
        {
            let changes = self.view.set_startup_notice(notice);
            self.broadcast(changes);
        }
        if let Some(notify) = startup_notify(&self.startup) {
            for tx in self.clients.values() {
                let _ = tx.send(Frame::Record(notify.clone()));
            }
        }
    }

    /// A record the daemon makes in pi's format: tracked for replay like pi's own, and sent to attached clients.
    fn emit_record(&mut self, record: Value) {
        let Value::Object(map) = record else { return };
        let kind = map.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
        let line: Arc<str> = Value::Object(map.clone()).to_string().into();
        self.replay.track(&kind, &map, &line);
        for tx in self.clients.values() {
            let _ = tx.send(Frame::Record(line.clone()));
        }
    }

    fn on_pi_line(&mut self, line: String) {
        self.announce_startup();
        let Ok(Value::Object(record)) = serde_json::from_str::<Value>(&line) else {
            return;
        };
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if kind == "response" || kind == "bash_execution_update" {
            self.route_response(kind == "response", record);
            return;
        }
        let line: Arc<str> = line.into();
        for ended in self.calls.track(&kind, &record) {
            if let Some(network) = &mut self.network {
                network.forget(&ended);
            }
        }
        self.track(&kind, &record, &line);
        for tx in self.clients.values() {
            let _ = tx.send(Frame::Record(line.clone()));
        }
        let changes = self.view.apply(&kind, &record);
        self.broadcast(changes);
        if kind == "compaction_end" && record.get("result").is_some() {
            // The conversation was rewritten: rebuild the view from entries.
            self.internal(Internal::GetEntries);
        }
        // A question whose dialog left with the run frees the slot; a waiting process question is shown then. (The
        // dialog it adds comes back through here, and finds the slot taken.)
        self.ask_next_process();
        self.publish_view_state();
    }

    /// A bash call the agent asks the daemon to run: refused unless it's a sandboxed session's running call; run with a
    /// per-command grant that matches it; otherwise, for a rerun, asked of the user.
    fn run_command(&mut self, request: crate::agent::RunRequest, reply: oneshot::Sender<crate::agent::RunDecision>) {
        use crate::agent::RunDecision;
        let key = lock(&self.shared).key;
        let Some(commands) = &self.commands else {
            let _ = reply.send(RunDecision::NotRun {
                reason: "only a sandboxed session runs commands through the daemon".into(),
            });
            return;
        };
        match commands.decide(&mut self.calls, &request) {
            commands::First::Decided(decision) => {
                match &decision {
                    // Every call runs here; only a widened or host one is worth a line in the daemon log.
                    RunDecision::Run(approved) => match &approved.runtime {
                        crate::agent::Runtime::Session(_) => {}
                        crate::agent::Runtime::Widened { .. } => {
                            crate::log!("session {key}: running a command with its per-command grant");
                        }
                        crate::agent::Runtime::Host { .. } => {
                            crate::log!("session {key}: running a command on the host by its per-command entry");
                        }
                    },
                    RunDecision::NotRun { reason } => crate::log!("session {key}: command request refused: {reason}"),
                }
                let decision = self.with_gate_user(decision);
                let _ = reply.send(decision);
            }
            commands::First::Ask { refusals, tag, note } => {
                let asker = crate::agent::question::Asker::Agent {
                    reason: request.reason.clone(),
                };
                let question = commands.question(
                    format!("piton:question:{}", self.next_internal + 1),
                    &request,
                    asker,
                    refusals,
                    note,
                );
                if self.asking() {
                    let _ = reply.send(RunDecision::NotRun {
                        reason: "another question is waiting for the user; try again after it's answered".into(),
                    });
                    return;
                }
                crate::log!("session {key}: asking to run a command outside the sandbox");
                self.ask_question(question, request, tag, reply);
            }
        }
    }

    /// Shows the block question: to `rpc` clients as pi's dialog, kept for replay until answered; `view` clients get the
    /// typed question in the state instead.
    fn ask_question(
        &mut self,
        question: crate::agent::question::Question,
        request: crate::agent::RunRequest,
        tag: String,
        reply: oneshot::Sender<crate::agent::RunDecision>,
    ) {
        self.next_internal += 1;
        let dialog_id = self.next_dialog_id();
        let record = crate::agent::question::rpc_dialog(&dialog_id, &question);
        self.question = Some(commands::PendingQuestion {
            question,
            dialog_id,
            stage: commands::Stage::Choice,
            request,
            tag,
            reply,
            deadline: self.config.approval_timeout.deadline(Instant::now()),
        });
        self.on_pi_line(record.to_string());
        self.publish_view_state();
    }

    /// A call that failed in the session's own sandbox: asked about when a block recorded for it is one an approved
    /// rerun lifts, otherwise reported to the agent with its blocks.
    fn run_failed(
        &mut self,
        request: crate::agent::RunRequest,
        last: bool,
        reply: oneshot::Sender<crate::agent::Failed>,
    ) {
        use crate::agent::Failed;
        let key = lock(&self.shared).key;
        let slot_taken = self.asking();
        let Some(commands) = &self.commands else {
            let _ = reply.send(Failed::Report {
                blocks: Vec::new(),
                note: None,
            });
            return;
        };
        match commands.after_failure(&mut self.calls, &request, last, slot_taken) {
            commands::AfterFailure::Wait => {
                let _ = reply.send(Failed::Wait);
            }
            commands::AfterFailure::Report { blocks, note } => {
                if !blocks.is_empty() {
                    crate::log!(
                        "session {key}: a command failed on blocks not asked about: {}",
                        blocks.join("; ")
                    );
                }
                let _ = reply.send(Failed::Report { blocks, note });
            }
            commands::AfterFailure::Ask {
                blocks,
                refusals,
                tag,
                note,
            } => {
                let question = commands.question(
                    format!("piton:question:{}", self.next_internal + 1),
                    &request,
                    crate::agent::question::Asker::Piton,
                    refusals,
                    note,
                );
                crate::log!(
                    "session {key}: a command failed on a block; asking to run it again: {}",
                    blocks.join("; ")
                );
                let (decided, decision) = oneshot::channel();
                self.ask_question(question, request, tag, decided);
                let _ = reply.send(Failed::Asked { blocks, decision });
            }
        }
    }

    /// A fresh id for the block question's `rpc` dialog.
    fn next_dialog_id(&mut self) -> String {
        self.next_internal += 1;
        format!("piton:block:{}", self.next_internal)
    }

    /// An `rpc` client's answer to the block question's dialog: show it again, switch the runtime, ask for the
    /// pattern, or decide.
    fn answer_question(&mut self, answer: &Map<String, Value>) {
        let Some(pending) = self.question.take() else { return };
        let next = commands::rpc_answer(pending.stage, &pending.question, answer);
        let (question, stage, record) = match next {
            commands::Next::Decided(answer) => return self.decide_question(pending, answer),
            commands::Next::AskAgain => {
                let id = self.next_dialog_id();
                let record = crate::agent::question::rpc_dialog(&id, &pending.question);
                (pending.question.clone(), commands::Stage::Choice, (id, record))
            }
            commands::Next::Switch => {
                let question = pending.question.switched();
                let id = self.next_dialog_id();
                let record = crate::agent::question::rpc_dialog(&id, &question);
                (question, commands::Stage::Choice, (id, record))
            }
            commands::Next::AskPattern(target, scope) => {
                let id = self.next_dialog_id();
                let record = crate::agent::question::rpc_pattern_dialog(&id, &pending.request.command, target, scope);
                (
                    pending.question.clone(),
                    commands::Stage::Pattern(target, scope),
                    (id, record),
                )
            }
        };
        let (dialog_id, record) = record;
        self.question = Some(commands::PendingQuestion {
            question,
            dialog_id,
            stage,
            ..pending
        });
        self.on_pi_line(record.to_string());
    }

    /// A `view` client's typed answer to the block question (`session.decide`).
    fn decide(&mut self, question: &str, answer: &Value) -> Result<(), String> {
        let Some(pending) = &self.question else {
            return Err("that question was already answered or expired".into());
        };
        if pending.question.id != question {
            return Err("that question was already answered or expired".into());
        }
        let answer = crate::agent::question::parse_decision(answer)?;
        let Some(pending) = self.question.take() else {
            return Ok(());
        };
        // The `rpc` rendering's dialog closes in every attached client.
        self.replay.answer_dialog(&pending.dialog_id);
        self.decide_question(pending, answer);
        Ok(())
    }

    /// The user's decision, from either rendering: save the entry when asked, then run or refuse.
    fn decide_question(&mut self, pending: commands::PendingQuestion, answer: crate::agent::question::Answer) {
        use crate::agent::{
            RunDecision,
            question::{Answer, Target},
        };
        let key = lock(&self.shared).key;
        let where_ = |target: Target| match target {
            Target::Sandbox => "in the sandbox",
            Target::Host => "on the host",
        };
        let decision = match answer {
            Answer::Deny(()) => {
                crate::log!("session {key}: the user denied running a command outside the sandbox");
                // The agent mustn't ask again in this run for what the user just declined.
                self.calls.decline(&pending.request.call_id);
                RunDecision::NotRun {
                    reason: "the user declined to run it outside the sandbox; don't ask again for this command".into(),
                }
            }
            Answer::Narrowest(crate::agent::answer::Once(target)) => match &self.commands {
                Some(commands) => {
                    crate::log!("session {key}: the user approved running a command {}", where_(target));
                    RunDecision::Run(Box::new(commands.approved_rerun(
                        &pending.request,
                        &pending.tag,
                        target,
                    )))
                }
                None => return,
            },
            Answer::Session(target) => match &mut self.commands {
                Some(commands) => {
                    crate::log!(
                        "session {key}: the user approved running a command {} for this session",
                        where_(target)
                    );
                    commands.allow_for_session(&pending.request.command, target);
                    RunDecision::Run(Box::new(commands.approved_rerun(
                        &pending.request,
                        &pending.tag,
                        target,
                    )))
                }
                None => return,
            },
            Answer::Always(always) => match &self.commands {
                Some(commands) => {
                    let target = always.target;
                    let shown = always.pattern.pattern().to_string();
                    let note = match commands.persist(always) {
                        Ok(path) => format!(" (saved `bash({shown})` in {}; new sessions use it)", path.display()),
                        Err(e) => format!(" (couldn't save the entry for `{shown}`: {e})"),
                    };
                    crate::log!(
                        "session {key}: the user approved running a command {}{note}",
                        where_(target)
                    );
                    RunDecision::Run(Box::new(commands.approved_rerun(
                        &pending.request,
                        &pending.tag,
                        target,
                    )))
                }
                None => return,
            },
        };
        let decision = self.with_gate_user(decision);
        let _ = pending.reply.send(decision);
        self.ask_next_process();
    }

    /// A command run in a sandbox with a filtered network gets the gate as its own proxy user, named by its tag, so the
    /// gate decides its connections under its policy and the question names its command. Forgotten when its call ends.
    fn with_gate_user(&mut self, decision: crate::agent::RunDecision) -> crate::agent::RunDecision {
        use crate::agent::{RunDecision, Runtime};
        let Some(network) = &mut self.network else {
            return decision;
        };
        match decision {
            RunDecision::Run(mut approved) => {
                match &mut approved.runtime {
                    Runtime::Session(policy) | Runtime::Widened { policy, .. } => {
                        let user = policy.tag.clone();
                        network.issue(&user, policy);
                    }
                    Runtime::Host { .. } => {}
                }
                RunDecision::Run(approved)
            }
            not_run @ RunDecision::NotRun { .. } => not_run,
        }
    }

    /// Ends the block question when it can't be answered any more: its dialog left (the run ended) or the agent
    /// stopped waiting.
    fn settle_question(&mut self) {
        let Some(pending) = &self.question else { return };
        let gone = !self.replay.has_dialog(&pending.dialog_id);
        if !gone && !pending.reply.is_closed() {
            return;
        }
        let Some(pending) = self.question.take() else { return };
        crate::log!(
            "session {}: a command question ended unanswered",
            lock(&self.shared).key
        );
        match gone {
            true => {
                let _ = pending.reply.send(crate::agent::RunDecision::NotRun {
                    reason: "the run ended before the user answered".into(),
                });
            }
            false => {
                self.replay.answer_dialog(&pending.dialog_id);
                self.sync_shared();
            }
        }
        self.publish_view_state();
    }

    /// A connection the gate holds: allowed or refused at once when the sandbox's entries, pi's hosts, or the session's
    /// answers decide it; otherwise it waits for the user, joining the question about the same
    /// destination when one is showing.
    fn network_ask(
        &mut self,
        user: crate::launch::sandbox::network::User,
        to: crate::launch::sandbox::Destination,
        reply: oneshot::Sender<crate::launch::sandbox::network::Verdict>,
    ) {
        let Some(network) = &mut self.network else {
            let _ = reply.send(network::refused(&to, "this session has no gate"));
            return;
        };
        let Some(waiting) = network.decide(user, to, reply) else {
            return;
        };
        match &mut network.asking {
            Some(asking) if asking.to == waiting.to => asking.waiting.push(waiting),
            Some(_) | None => {
                network.queued.push_back(waiting);
                self.ask_next_network();
            }
        }
    }

    /// Who a proxy user is, as the network question names it: pi, or the call's command.
    fn who(&self, user: &crate::launch::sandbox::network::User) -> String {
        match self.calls.owner(&user.0) {
            commands::TagOwner::Call(id) => self
                .calls
                .command_of(&id)
                .map_or_else(|| "A command".to_string(), |c| format!("`{c}`")),
            commands::TagOwner::Session
            | commands::TagOwner::GoneCall
            | commands::TagOwner::Typed
            | commands::TagOwner::Other => "pi (an MCP server, an extension, or pi itself)".to_string(),
        }
    }

    /// Shows the next network question when the session's question slot is free.
    fn ask_next_network(&mut self) {
        if self.asking() {
            return;
        }
        let Some(network) = &mut self.network else { return };
        if network.asking.is_some() {
            return;
        }
        let Some(first) = network.queued.pop_front() else {
            return;
        };
        // Every connection waiting on the same destination joins this question.
        let (same, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut network.queued)
            .into_iter()
            .partition(|w| w.to == first.to);
        network.queued = rest.into();
        let to = first.to.clone();
        let who = self.who(&first.user);
        let mut waiting = vec![first];
        waiting.extend(same);
        self.next_internal += 1;
        let dialog_id = format!("piton:network:{}", self.next_internal);
        let deadline = self.config.approval_timeout.deadline(Instant::now());
        let record = crate::agent::block::network_dialog(&dialog_id, &who, &to.to_string());
        if let Some(network) = &mut self.network {
            network.asking = Some(network::Asking {
                dialog_id,
                to,
                waiting,
                deadline,
            });
        }
        self.on_pi_line(record.to_string());
    }

    /// The user's answer to a network question.
    fn answer_network(&mut self, answer: &Map<String, Value>) {
        use crate::agent::block::NetworkAnswer;
        let key = lock(&self.shared).key;
        let Some(network) = &mut self.network else { return };
        let Some(to) = network.asking.as_ref().map(|a| a.to.clone()) else {
            return;
        };
        let verdict = match crate::agent::block::read_network_answer(answer) {
            NetworkAnswer::AskAgain => {
                // Shown again: the connections keep waiting under a new dialog.
                if let Some(asking) = network.asking.take() {
                    let mut waiting = asking.waiting.into_iter();
                    if let Some(first) = waiting.next() {
                        network.queued.push_front(first);
                    }
                    network.queued.extend(waiting);
                }
                self.ask_next_network();
                return;
            }
            NetworkAnswer::Session => {
                crate::log!("session {key}: the user allowed {to} for this session");
                network.allow_for_session(to.clone());
                crate::launch::sandbox::network::Verdict::Allow { literal: false }
            }
            NetworkAnswer::Persist(scope) => {
                let entry = crate::config::NetworkTarget::Host {
                    host: to.host.clone(),
                    port: Some(to.port),
                };
                let saved = self
                    .commands
                    .as_ref()
                    .map(|c| c.persist_lift(scope, &crate::launch::sandbox::grant::Lift::Network(entry.clone())));
                match saved {
                    Some(Ok(file)) => {
                        crate::log!(
                            "session {key}: saved `{}` in {}",
                            crate::launch::sandbox::grant::Lift::Network(entry.clone()).entry(),
                            file.display()
                        );
                        // Every live session that reads this file takes it (this one included), and new calls inherit it.
                        (self.hooks.network_entry_saved)(entry, file);
                        crate::launch::sandbox::network::Verdict::Allow { literal: false }
                    }
                    Some(Err(e)) => {
                        crate::log!("session {key}: couldn't save the entry: {e}");
                        self.show_notice(format!(
                            "Couldn't save the sandbox entry: {e}; allowed for this session"
                        ));
                        if let Some(network) = &mut self.network {
                            network.allow_for_session(to.clone());
                        }
                        crate::launch::sandbox::network::Verdict::Allow { literal: false }
                    }
                    None => network::refused(&to, "no configuration to save to"),
                }
            }
            NetworkAnswer::NotNow => network::refused(&to, "the user didn't allow it"),
        };
        if let Some(network) = &mut self.network {
            network.settle(&verdict);
        }
        self.ask_next_process();
    }

    /// No answer in time: the connections are refused, and the dialog closes everywhere.
    fn expire_network_question(&mut self) {
        let Some(network) = &mut self.network else { return };
        let Some(asking) = &network.asking else { return };
        let (id, to) = (asking.dialog_id.clone(), asking.to.clone());
        network.settle(&network::refused(&to, "the user didn't answer in time"));
        self.replay.answer_dialog(&id);
        crate::log!("session {}: a network question timed out", lock(&self.shared).key);
        self.ask_next_process();
        self.sync_shared();
        self.publish_view_state();
    }

    /// A connection the gate refused: recorded for the call that made it (its block list shows it), else logged.
    fn gate_refused(&mut self, refused: crate::launch::sandbox::network::Refused) {
        let key = lock(&self.shared).key;
        crate::log!("session {key}: the gate refused {} ({})", refused.to, refused.user.0);
        if let commands::TagOwner::Call(id) = self.calls.owner(&refused.user.0) {
            self.calls.note(
                &id,
                commands::CallRefusal {
                    process: commands::GATE_PROCESS.into(),
                    operation: "network-outbound".into(),
                    target: refused.to.to_string(),
                },
            );
        }
    }

    /// A connection entry saved from a network question: when this session's configuration reads `file`, every sandbox
    /// it has and every call it starts from now on reaches the destination, as after a restart.
    fn network_entry_saved(&mut self, target: crate::config::NetworkTarget, file: PathBuf) {
        let Some(network) = &mut self.network else { return };
        let Some(commands) = &mut self.commands else { return };
        if !commands.reads(&file) {
            return;
        }
        let source = crate::launch::sandbox::Source::Config {
            key: format!("sandbox.network.connections.\"{target}\""),
            from: crate::launch::sandbox::Provenance::File { path: file },
        };
        network.add_entry(target.clone(), source.clone());
        commands.add_network_entry(target, source);
        crate::log!("session {}: took a saved connection entry", lock(&self.shared).key);
    }

    /// fnox's proxy URL: the gate forwards every allowed connection through it from now on.
    fn gate_upstream(&mut self, url: &str) {
        let key = lock(&self.shared).key;
        match (&self.network, crate::launch::sandbox::network::parse_upstream(url)) {
            (Some(network), Some(route)) => {
                network.gate.set_route(route);
                crate::log!("session {key}: the gate forwards through fnox's proxy");
            }
            (Some(_), None) => crate::log!("session {key}: fnox's proxy URL isn't one the gate can use"),
            (None, _) => {}
        }
    }

    /// A refusal the kernel recorded for this session: kept for the bash call it belongs to, or, for a process that
    /// isn't one, asked about: lifted in pi's running sandbox, or saved as the entry that lifts it, or reported when
    /// neither can.
    fn on_refusal(&mut self, refusal: crate::daemon::Refusal) {
        use crate::{
            agent::block::{Live, NoLive},
            launch::sandbox::grant::{self, Refused, Scope},
        };
        let summary = refusal.summary();
        // A call's sandbox: kept for that call, shown when it's asked to run again. A typed command: the user saw it fail
        // and it's never rerun. pi's own sandbox: lifted live. A call's background job outliving it: it can't take pi's
        // token, so only an entry.
        let own = match self.calls.owner(&refusal.tag) {
            commands::TagOwner::Call(id) => {
                self.calls.note(&id, commands::CallRefusal::from(&refusal));
                return;
            }
            commands::TagOwner::Typed | commands::TagOwner::Other => return,
            commands::TagOwner::Session => true,
            commands::TagOwner::GoneCall => false,
        };
        let Some(commands) = &self.commands else { return };
        let key = lock(&self.shared).key;
        if !self
            .process
            .first(&refusal.process, &refusal.operation, &refusal.target)
        {
            return;
        }
        // A refusal programs hit and recover from: logged, never a notice or a question.
        if grant::incidental(&refusal.operation, &refusal.target) {
            crate::log!("session {key}: {} was blocked: {summary} (incidental)", refusal.process);
            return;
        }
        if let Some(silenced) = crate::launch::sandbox::silenced(
            &commands.policies.silence,
            &refusal.process,
            &refusal.operation,
            &refusal.target,
        ) {
            crate::log!(
                "session {key}: {} was blocked: {summary} (silenced by {}, {})",
                refusal.process,
                silenced.describe(),
                silenced.source
            );
            return;
        }
        let projects = [commands.source.as_path(), commands.checkout.as_path()];
        let shared = shared_folders(&commands.env);
        let widened = |lift: &grant::Lift, scope: Scope| {
            let file = commands
                .scope_file(match scope {
                    Scope::Project => crate::agent::block::PersistScope::Project,
                    Scope::Everywhere => crate::agent::block::PersistScope::Global,
                })
                .ok_or_else(|| "no file for that scope".to_string())?;
            commands.policies.resolver.with(&lift.entries(), &file)
        };
        let refused = Refused {
            operation: &refusal.operation,
            target: &refusal.target,
            pid: refusal.pid,
        };
        let bounds = grant::Bounds {
            projects: &projects,
            home: &commands.paths.home,
            shared: &shared,
            credentials: commands.policies.credentials,
        };
        let decided = grant::lift_for(&commands.policies.launch, refused, &bounds, widened);
        // What a token opens in pi's own sandbox, checked against every covering deny.
        let live = match (own, self.lifts.stream()) {
            (false, _) => Live::No(NoLive::OtherProcess),
            (true, lifts::Stream::Closed) => Live::No(NoLive::NoStream),
            (true, lifts::Stream::Open) => match grant::live_lift(&commands.policies.launch, refused, &bounds) {
                Ok(lift) => Live::Lift(lift),
                Err(why) => Live::No(NoLive::Refused(why.to_string())),
            },
        };
        let process = &refusal.process;
        let question = |offer: Option<grant::Offer>, live: Live| commands::ProcessQuestion {
            id: String::new(),
            process: process.clone(),
            operation: refusal.operation.clone(),
            target: refusal.target.clone(),
            pid: refusal.pid,
            refusal: summary.clone(),
            live,
            offer,
        };
        let notice = match (decided, live) {
            // An entry the user wrote decides it: only logged.
            (Err(grant::NoLift::Decided(grant::DeniedBy::UserDeny)), _) => {
                crate::log!("session {key}: {process} was blocked: {summary} (your own [sandbox] entry)");
                return;
            }
            // A credential read is a probe programs recover from: a notice, as for bash calls, never a question.
            (
                Err(why @ grant::NoLift::Decided(grant::DeniedBy::Credential(crate::launch::sandbox::Access::Read))),
                _,
            ) => notice::notice(
                &notice::Blocked {
                    process,
                    operation: &refusal.operation,
                    target: &refusal.target,
                    summary: &summary,
                },
                &why,
                &notice::Context {
                    home: &commands.paths.home,
                    claude: notice::ClaudeSignIn::of(commands.policies.claude_login),
                },
            ),
            (Ok(offer), live) => {
                crate::log!("session {key}: {process} was blocked: {summary}");
                self.process.queue(question(Some(offer), live));
                self.ask_next_process();
                return;
            }
            // No saved form (Git's control files, the keychain with fnox off), but pi's sandbox can be widened now.
            (Err(_), live @ Live::Lift(_)) => {
                crate::log!("session {key}: {process} was blocked: {summary} (liftable while it runs)");
                self.process.queue(question(None, live));
                self.ask_next_process();
                return;
            }
            (Err(why), Live::No(_)) => notice::notice(
                &notice::Blocked {
                    process,
                    operation: &refusal.operation,
                    target: &refusal.target,
                    summary: &summary,
                },
                &why,
                &notice::Context {
                    home: &commands.paths.home,
                    claude: notice::ClaudeSignIn::of(commands.policies.claude_login),
                },
            ),
        };
        crate::log!("session {key}: {notice}");
        self.show_notice(notice);
    }

    /// A warning in the session view (the apps) and as a notification to attached clients (pi.nvim).
    fn show_notice(&mut self, text: String) {
        let changes = self.view.add_notice(text.clone());
        self.broadcast(changes);
        self.next_internal += 1;
        let record: Arc<str> = json!({
            "type": "extension_ui_request",
            "id": format!("piton:notice:{}", self.next_internal),
            "method": "notify",
            "notifyType": "warning",
            "message": text,
        })
        .to_string()
        .into();
        for tx in self.clients.values() {
            let _ = tx.send(Frame::Record(record.clone()));
        }
    }

    /// Whether the session's one question slot is taken: a protected write, a block question, or a process question.
    /// Settles the ones that can't be answered any more first.
    fn asking(&mut self) -> bool {
        self.settle_protected_write();
        self.settle_question();
        self.settle_process();
        self.settle_network();
        self.settle_lift();
        self.protected_write.is_some()
            || self.question.is_some()
            || self.process.asking.is_some()
            || self.network.as_ref().is_some_and(|n| n.asking.is_some())
            || self.lift.is_some()
    }

    /// A network question whose dialog left unanswered (the run it showed in ended) is asked again under a new dialog;
    /// its connections keep waiting.
    fn settle_network(&mut self) {
        let Some(network) = &mut self.network else { return };
        let Some(asking) = &network.asking else { return };
        if self.replay.has_dialog(&asking.dialog_id) {
            return;
        }
        if let Some(asking) = network.asking.take() {
            for waiting in asking.waiting.into_iter().rev() {
                network.queued.push_front(waiting);
            }
        }
    }

    /// A process question whose dialog left unanswered (the run it showed in ended) is asked again.
    fn settle_process(&mut self) {
        let Some(question) = &self.process.asking else { return };
        if self.replay.has_dialog(&question.id) {
            return;
        }
        self.process.requeue_unanswered();
    }

    /// Shows the next process question when nothing else is being asked. Called whenever the slot may have freed.
    fn ask_next_process(&mut self) {
        // A held connection is waiting for an answer: its question goes first.
        self.ask_next_network();
        if self.asking() {
            return;
        }
        let Some(next) = self.process.next().cloned() else {
            return;
        };
        // A fresh id, so a re-asked question is a new dialog every client shows.
        self.next_internal += 1;
        let id = format!("piton:process:{}", self.next_internal);
        if let Some(asking) = &mut self.process.asking {
            asking.id = id.clone();
        }
        // The stream may have opened or closed since it was queued: decide liftability now.
        let next = self.refresh_live(next);
        if let Some(asking) = &mut self.process.asking {
            asking.live = next.live.clone();
        }
        let record =
            crate::agent::block::process_dialog(&id, &next.process, &next.refusal, &next.live, next.offer.as_ref());
        self.on_pi_line(record.to_string());
    }

    /// A queued question's live answer, as of now: a question queued without a stream gets one once pi's opened, and
    /// loses it when it closed.
    fn refresh_live(&self, mut question: commands::ProcessQuestion) -> commands::ProcessQuestion {
        use crate::agent::block::{Live, NoLive};
        question.live = match (&question.live, self.lifts.stream()) {
            (Live::No(NoLive::OtherProcess | NoLive::Refused(_)), _) => question.live,
            (Live::Lift(_), lifts::Stream::Closed) => Live::No(NoLive::NoStream),
            (Live::Lift(_), lifts::Stream::Open) | (Live::No(NoLive::NoStream), lifts::Stream::Closed) => question.live,
            (Live::No(NoLive::NoStream), lifts::Stream::Open) => {
                let Some(commands) = &self.commands else {
                    return question;
                };
                let projects = [commands.source.as_path(), commands.checkout.as_path()];
                let shared = shared_folders(&commands.env);
                let bounds = crate::launch::sandbox::grant::Bounds {
                    projects: &projects,
                    home: &commands.paths.home,
                    shared: &shared,
                    credentials: commands.policies.credentials,
                };
                let refused = crate::launch::sandbox::grant::Refused {
                    operation: &question.operation,
                    target: &question.target,
                    pid: question.pid,
                };
                match crate::launch::sandbox::grant::live_lift(&commands.policies.launch, refused, &bounds) {
                    Ok(lift) => Live::Lift(lift),
                    Err(why) => Live::No(NoLive::Refused(why.to_string())),
                }
            }
        };
        question
    }

    /// The user's answer to a process question: lift it in pi's running sandbox until the run settles or for the
    /// session, save the entry (and lift it now), or leave it.
    fn answer_process(&mut self, answer: &Map<String, Value>) {
        use crate::agent::{
            answer::{Answer, UntilSettled},
            block::Live,
        };
        let Some(question) = self.process.asking.take() else {
            return;
        };
        let key = lock(&self.shared).key;
        // pi's own process takes the grant at once; another (an MCP server pi started) once it's started again.
        let who = match self.pi_pid {
            Some(pi) if pi == question.pid => lifts::Who::Pi,
            Some(_) | None => lifts::Who::Child,
        };
        let block = || lifts::Block {
            process: question.process.clone(),
            operation: question.operation.clone(),
            target: question.target.clone(),
            who,
        };
        let answer = crate::agent::block::read_process_answer(answer, &question.live, question.offer.is_some());
        let live = match &question.live {
            Live::Lift(lift) => Some(lift.clone()),
            Live::No(_) => None,
        };
        match answer {
            None => {
                self.process.asking = Some(question);
                self.process.requeue_unanswered();
                self.ask_next_process();
            }
            Some(Answer::Deny(())) => {
                crate::log!("session {key}: the user left {} blocked for now", question.process);
                self.ask_next_process();
            }
            Some(Answer::Narrowest(UntilSettled(()))) => {
                if let Some(lift) = live {
                    crate::log!("session {key}: {} lifted until the run settles", lift.describe());
                    self.grant_live(lift, lifts::Scope::UntilSettled, Some(block()));
                }
                self.ask_next_process();
            }
            Some(Answer::Session(())) => {
                if let Some(lift) = live {
                    crate::log!("session {key}: {} lifted for the session", lift.describe());
                    self.grant_live(lift, lifts::Scope::Session, Some(block()));
                }
                self.ask_next_process();
            }
            Some(Answer::Always(scope)) => {
                let Some(offer) = &question.offer else { return };
                let Some(commands) = &mut self.commands else { return };
                let entry = offer.at(grant_scope(scope)).clone();
                match commands.persist_lift(scope, &entry) {
                    Ok(path) => {
                        crate::log!("session {key}: saved `{}` in {}", entry.entry(), path.display());
                        // Later bash calls get it now, as after a restart.
                        if let Err(e) = commands.add_saved_entry(&entry, &path) {
                            crate::log!("session {key}: the saved entry doesn't resolve for commands: {e}");
                        }
                        // pi's own sandbox gets it now, for the session; new sessions read the entry. The block isn't
                        // asked about again: the entry covers it from now on.
                        if let Some(lift) = live {
                            let effects = self.lifts.grant(lift, lifts::Scope::Session, Some(block()));
                            self.apply_lifts(effects);
                        }
                    }
                    Err(e) => {
                        crate::log!("session {key}: couldn't save the entry: {e}");
                        self.show_notice(format!("Couldn't save the sandbox entry: {e}"));
                    }
                }
                self.ask_next_process();
            }
        }
    }

    /// Grants `lift` on pi's stream and applies what that did.
    fn grant_live(
        &mut self,
        lift: crate::launch::sandbox::grant::LiveLift,
        scope: lifts::Scope,
        block: Option<lifts::Block>,
    ) {
        let effects = self.lifts.grant(lift, scope, block);
        self.apply_lifts(effects);
    }

    /// Sends what the lifts state machine decided, asks about ended grants' blocks again, and reports failures.
    fn apply_lifts(&mut self, effects: lifts::Effects) {
        let lifts::Effects { send, forget, failed } = effects;
        if let Some((_, out)) = &self.lifts_out {
            for line in send {
                let _ = out.send(match line {
                    lifts::Out::Grant { id, lift, for_child } => LiftsOut::Grant { id, lift, for_child },
                    lifts::Out::Revoke { id } => LiftsOut::Revoke { id },
                });
            }
        }
        for block in forget {
            self.process.forget(&block.process, &block.operation, &block.target);
        }
        for (lift, why) in failed {
            let key = lock(&self.shared).key;
            crate::log!(
                "session {key}: couldn't widen pi's sandbox for {}: {why}",
                lift.describe()
            );
            self.show_notice(format!("Couldn't widen pi's sandbox for {}: {why}", lift.describe()));
        }
    }

    /// pi's `lifts` stream.
    fn lifts_event(&mut self, event: LiftsEvent) {
        let key = lock(&self.shared).key;
        match event {
            LiftsEvent::Opened { out, id, pi } => {
                self.pi_pid = Some(pi);
                let stream = self.next_stream;
                self.next_stream += 1;
                let effects = self.lifts.opened();
                self.apply_lifts(effects);
                self.lifts_out = Some((stream, out));
                let _ = id.send(stream);
                crate::log!("session {key}: pi's sandbox can be widened while it runs");
            }
            LiftsEvent::Consumed { id } => {
                let effects = self.lifts.reply(id, lifts::Reply::Consumed);
                self.apply_lifts(effects);
            }
            LiftsEvent::Failed { id, message } => {
                let effects = self.lifts.reply(id, lifts::Reply::Failed(message));
                self.apply_lifts(effects);
            }
            LiftsEvent::Closed { stream } => {
                // Only the open stream's closing ends the grants: a replaced one's already did.
                if self.lifts_out.as_ref().is_some_and(|(open, _)| *open == stream) {
                    self.lifts_out = None;
                    let effects = self.lifts.closed();
                    self.apply_lifts(effects);
                    crate::log!("session {key}: pi's lift stream closed");
                }
            }
        }
    }

    /// pi's run settled: every "until the run settles" grant ends.
    fn lifts_settled(&mut self) {
        let effects = self.lifts.settled();
        self.apply_lifts(effects);
    }

    /// An explicit lift from code in pi's process: answered from a session approval when one covers every target,
    /// refused when a target is floor or another question holds the slot, otherwise asked.
    fn lift(
        &mut self,
        request: crate::agent::lift::LiftRequest,
        reply: oneshot::Sender<crate::agent::lift::LiftDecision>,
    ) {
        use crate::agent::lift::LiftDecision;
        let key = lock(&self.shared).key;
        let Some(commands) = &self.commands else {
            let _ = reply.send(LiftDecision::Refused {
                reason: "only a sandboxed session has a sandbox to widen".into(),
            });
            return;
        };
        if request.targets.is_empty() {
            let _ = reply.send(LiftDecision::Refused {
                reason: "nothing to lift".into(),
            });
            return;
        }
        let mut targets = Vec::with_capacity(request.targets.len());
        for target in &request.targets {
            let resolved = resolve_live(target);
            if let Err(why) = crate::launch::sandbox::grant::check_live(
                &commands.policies.launch,
                &resolved,
                commands.policies.credentials,
            ) {
                crate::log!("session {key}: lift of {} refused: {why}", resolved.describe());
                let _ = reply.send(LiftDecision::Refused {
                    reason: format!("{}: {why}", resolved.describe()),
                });
                return;
            }
            targets.push(resolved);
        }
        if targets.iter().all(|t| self.lifts.covers(t)) {
            let _ = reply.send(LiftDecision::Granted(targets));
            return;
        }
        if self.asking() {
            let _ = reply.send(LiftDecision::Refused {
                reason: "another question is waiting for the user; try again after it's answered".into(),
            });
            return;
        }
        self.next_internal += 1;
        let id = format!("piton:lift:{}", self.next_internal);
        crate::log!("session {key}: asking about a lift: {}", request.reason);
        let record = crate::agent::lift::dialog(
            &id,
            &crate::agent::lift::LiftRequest {
                targets: targets.clone(),
                reason: request.reason,
            },
        );
        self.lift = Some(PendingLift { id, targets, reply });
        self.on_pi_line(record.to_string());
    }

    /// The user's answer to an explicit lift.
    fn answer_lift(&mut self, answer: &Map<String, Value>) {
        use crate::agent::{
            answer::{Answer, Once},
            lift::LiftDecision,
        };
        let Some(pending) = self.lift.take() else { return };
        let key = lock(&self.shared).key;
        match crate::agent::lift::read_answer(answer) {
            None => {
                // Asks again, under a new dialog.
                self.next_internal += 1;
                let id = format!("piton:lift:{}", self.next_internal);
                let record = crate::agent::lift::dialog(
                    &id,
                    &crate::agent::lift::LiftRequest {
                        targets: pending.targets.clone(),
                        reason: "(asked again)".into(),
                    },
                );
                self.lift = Some(PendingLift { id, ..pending });
                self.on_pi_line(record.to_string());
            }
            Some(Answer::Narrowest(Once(()))) => {
                crate::log!("session {key}: lift approved once");
                let _ = pending.reply.send(LiftDecision::Granted(pending.targets));
            }
            Some(Answer::Session(())) => {
                crate::log!("session {key}: lift approved for the session");
                // Later requests and blocks for these are answered without asking; pi's sandbox is widened for them.
                for target in &pending.targets {
                    self.grant_live(target.clone(), lifts::Scope::Session, None);
                }
                let _ = pending.reply.send(LiftDecision::Granted(pending.targets));
            }
            Some(Answer::Always(never)) => match never {},
            Some(Answer::Deny(())) => {
                crate::log!("session {key}: lift denied");
                let _ = pending.reply.send(LiftDecision::Denied);
            }
        }
    }

    /// Ends a pending lift that can no longer be answered: its dialog left with the run, or the caller stopped waiting.
    fn settle_lift(&mut self) {
        let Some(pending) = &self.lift else { return };
        let gone = !self.replay.has_dialog(&pending.id);
        if !gone && !pending.reply.is_closed() {
            return;
        }
        let Some(pending) = self.lift.take() else { return };
        match gone {
            true => {
                let _ = pending.reply.send(crate::agent::lift::LiftDecision::Refused {
                    reason: "the run ended before the user answered; ask again if it's still needed".into(),
                });
            }
            false => {
                self.replay.answer_dialog(&pending.id);
                self.sync_shared();
            }
        }
    }

    /// No answer in time: the command doesn't run, and the dialog closes everywhere.
    fn expire_question(&mut self) {
        let Some(pending) = self.question.take() else { return };
        crate::log!("session {}: a command question timed out", lock(&self.shared).key);
        self.replay.answer_dialog(&pending.dialog_id);
        let _ = pending.reply.send(crate::agent::RunDecision::NotRun {
            reason: "the user didn't answer in time; it didn't run".into(),
        });
        self.ask_next_process();
        self.sync_shared();
        self.publish_view_state();
    }

    /// Ends a pending protected write that can no longer be answered: its dialog left (the run it belonged to ended,
    /// so pi.nvim and the apps no longer show it) or the agent stopped waiting. The agent, if still there, hears that it
    /// wasn't written; the next request isn't refused as "another is waiting".
    fn settle_protected_write(&mut self) {
        let Some(pending) = &self.protected_write else { return };
        let gone = !self.replay.has_dialog(&pending.id);
        if !gone && !pending.reply.is_closed() {
            return;
        }
        let Some(pending) = self.protected_write.take() else {
            return;
        };
        let key = lock(&self.shared).key;
        crate::log!(
            "session {key}: protected write to {} ended unanswered",
            pending.path.display()
        );
        if gone {
            let _ = pending.reply.send(crate::agent::WriteOutcome::Refused {
                reason: "the run ended before the user answered; ask again if it's still needed".into(),
            });
        } else {
            // The agent left (its tool call was aborted): close the dialog in every client.
            self.replay.answer_dialog(&pending.id);
            self.sync_shared();
        }
    }

    fn internal(&mut self, command: Internal) {
        match command {
            Internal::GetEntries => self.view.begin_reset(),
            Internal::GetState | Internal::Prompt { .. } => {}
        }
        self.next_internal += 1;
        let id = format!("piton:i:{}", self.next_internal);
        let rpc = command.rpc(id.clone());
        self.pending.insert(id, Pending::Internal(command));
        self.send_to_pi(&rpc);
    }

    /// pi's temporary state now, as records for the view's rebuild: the in-progress message and running tools.
    fn live_records(&self) -> Vec<Map<String, Value>> {
        self.replay
            .live_lines()
            .into_iter()
            .filter_map(|l| match serde_json::from_str::<Value>(&l) {
                Ok(Value::Object(map)) => Some(map),
                Ok(_) | Err(_) => None,
            })
            .collect()
    }

    fn route_response(&mut self, is_response: bool, mut record: Map<String, Value>) {
        let Some(id) = record.get("id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        if is_response && let Some(next) = self.prompts.answered(&id) {
            self.write_to_pi(&next);
        }
        let pending = if is_response { self.pending.remove(&id) } else { None };
        let route = match pending {
            Some(p) => Some(p),
            // `bash_execution_update` repeats the command id while the command is still pending.
            None if !is_response => self.pending.get(&id).and_then(|p| match p {
                Pending::Client { client, outstanding } => Some(Pending::Client {
                    client: *client,
                    outstanding: outstanding.clone(),
                }),
                Pending::Internal(command) => Some(Pending::Internal(command.clone())),
                // Native clients don't send `bash`; nothing streams to a view reply.
                Pending::View { .. } | Pending::Send { .. } => None,
            }),
            None => None,
        };
        match route {
            Some(Pending::Client { client, outstanding }) => {
                if is_response {
                    self.after_response(&outstanding.command, &record);
                }
                match outstanding.original_id {
                    Some(original) => record.insert("id".into(), original),
                    None => record.remove("id"),
                };
                if let Some(tx) = self.clients.get(&client) {
                    let _ = tx.send(Frame::Record(Value::Object(record).to_string().into()));
                }
            }
            Some(Pending::Internal(command)) if is_response => self.after_response(command.kind(), &record),
            Some(Pending::View { command, reply }) if is_response => {
                self.after_response(&command, &record);
                let _ = reply.send(view_result(&record));
            }
            Some(Pending::Send { command, message }) if is_response => {
                self.after_response(&command, &record);
                self.answer_sent(&message, view_result(&record));
            }
            Some(Pending::Internal(_) | Pending::View { .. } | Pending::Send { .. }) | None => {}
        }
    }

    /// Keeps the session's id and file current after commands that change the conversation.
    fn after_response(&mut self, command: &str, record: &Map<String, Value>) {
        let success = record.get("success").and_then(Value::as_bool).unwrap_or(false);
        if !success {
            if command == "get_entries" {
                let key = lock(&self.shared).key;
                crate::log!("session {key}: get_entries failed; the view keeps its previous items");
                let changes = self.view.cancel_reset();
                self.broadcast(changes);
                // Watchers that waited for the rebuild get the view as it is, instead of waiting forever.
                self.send_waiting_snapshots();
            }
            return;
        }
        match command {
            "get_state" => {
                if let Some(Value::Object(data)) = record.get("data") {
                    let mut shared = lock(&self.shared);
                    shared.id = data.get("sessionId").and_then(Value::as_str).map(str::to_string);
                    shared.file = data.get("sessionFile").and_then(Value::as_str).map(str::to_string);
                    let count = data.get("messageCount").and_then(Value::as_u64).unwrap_or(0);
                    shared.has_messages = count > 0;
                    drop(shared);
                    self.view.on_state_data(data);
                }
                self.arm_empty_timer();
                self.publish_view_state();
            }
            "get_entries" => {
                if let Some(Value::Object(data)) = record.get("data") {
                    let live = self.live_records();
                    let reset = self.view.finish_reset(data, &live);
                    self.broadcast(vec![reset]);
                    self.publish_view_state();
                }
            }
            "switch_session" | "new_session" | "fork" | "clone" => {
                // The conversation changed: forget the old one's temporary state, ask for the new identity, and
                // rebuild the view.
                self.replay.forget_conversation();
                self.internal(Internal::GetState);
                self.internal(Internal::GetEntries);
            }
            _ => {}
        }
    }

    fn track(&mut self, kind: &str, record: &Map<String, Value>, line: &Arc<str>) {
        self.replay.track(kind, record, line);
        // Not `agent_end`: retries, compaction, steering, and follow-ups continue after it.
        if kind == "agent_settled" {
            self.lifts_settled();
        }
        if kind == "message_end" {
            let role = record
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str);
            if matches!(role, Some("user" | "assistant" | "toolResult")) {
                lock(&self.shared).has_messages = true;
            }
        }
        self.sync_shared();
    }

    fn status(&self) -> SessionStatus {
        if self.stopping.is_some() {
            return SessionStatus::Stopping;
        }
        if self.replay.needs_input() {
            return SessionStatus::NeedsInput;
        }
        if self.replay.working() {
            return SessionStatus::Working;
        }
        SessionStatus::Idle
    }

    fn sync_shared(&mut self) {
        let status = self.status();
        let clients = self.clients.len() + self.watchers.len();
        let (status_changed, list_changed) = {
            let mut shared = lock(&self.shared);
            let status_changed = shared.status != status;
            let list_changed = status_changed || shared.clients != clients;
            shared.status = status;
            shared.clients = clients;
            (status_changed, list_changed)
        };
        if status_changed {
            self.hooks.status_changed.notify_one();
        }
        if list_changed {
            let _ = self.hooks.list_changed.send(());
        }
    }

    fn is_empty_and_unused(&self) -> bool {
        let has_messages = lock(&self.shared).has_messages;
        self.clients.is_empty()
            && self.watchers.is_empty()
            && !has_messages
            && matches!(self.status(), SessionStatus::Idle)
    }

    fn arm_empty_timer(&mut self) {
        if self.stopping.is_none() && self.is_empty_and_unused() && self.empty_deadline.is_none() {
            self.empty_deadline = Some(Instant::now() + self.config.stop_empty_session_after.get());
        }
    }

    fn begin_stop(&mut self, done: Option<oneshot::Sender<StopOutcome>>) {
        self.stop_waiters.extend(done);
        if self.stopping.is_some() {
            return;
        }
        self.pi_tx = None; // closes pi's stdin once the writer drains
        self.stopping = Some((
            StopPhase::Closing,
            Instant::now() + self.config.stop.terminate_after.get(),
        ));
        self.sync_shared();
    }

    fn advance_stop(&mut self) {
        let Some((phase, _)) = &self.stopping else {
            return;
        };
        let key = lock(&self.shared).key;
        match phase {
            StopPhase::Closing => {
                crate::log!("session {key}: pi didn't exit after stdin closed; sending SIGTERM");
                self.signal(Signal::SIGTERM);
                self.stopping = Some((
                    StopPhase::Terminating,
                    Instant::now() + self.config.stop.kill_after.get(),
                ));
            }
            StopPhase::Terminating => {
                crate::log!("session {key}: pi didn't exit after SIGTERM; sending SIGKILL");
                self.signal(Signal::SIGKILL);
                self.stopping = Some((StopPhase::Killed, Instant::now() + Duration::from_secs(3600)));
            }
            StopPhase::Killed => {}
        }
    }

    fn signal(&self, signal: Signal) {
        if let Some(pgid) = self.pgid {
            let _ = killpg(pgid, signal);
        }
    }
}

/// pi's response to a native client's command as the client's reply.
fn view_result(record: &Map<String, Value>) -> Result<Value, String> {
    match record.get("success").and_then(Value::as_bool) {
        Some(true) => Ok(record.get("data").cloned().unwrap_or(json!({}))),
        Some(false) | None => Err(record
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("pi refused the command")
            .to_string()),
    }
}

fn command_type(command: &Map<String, Value>) -> String {
    command
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp is shared with host processes, as written and real; pi's own `$TMPDIR` too.
    #[test]
    fn shared_folders_hold_temp() {
        let env: launch::Env = [("TMPDIR".to_string(), "/var/folders/x/T/".to_string())].into();
        let shared = shared_folders(&env);
        for folder in ["/private/tmp", "/tmp", "/private/var/folders", "/var/folders/x/T/"] {
            assert!(shared.contains(&std::path::PathBuf::from(folder)), "{folder}");
        }
    }

    /// The startup notice holds the launch's warnings, then the secrets fnox couldn't read; nothing when both are empty.
    #[test]
    fn the_startup_notice_holds_launch_warnings_and_fnox_secrets() {
        assert_eq!(Startup::default().notice(), None);
        let mut startup = Startup {
            warnings: vec![launch::LaunchWarning::NoClaudeToken],
            fnox: Some(launch::fnox::StartupLog::default()),
            announced: false,
        };
        let notice = startup.notice().unwrap();
        assert!(notice.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{notice}");
        let log = startup.fnox.as_mut().unwrap();
        log.read("fnox proxy: 'GH' is not set; requests that need it are refused until it is");
        let notice = startup.notice().unwrap();
        let (warning, fnox) = notice.split_once("\n\n").unwrap();
        assert!(
            warning.contains("CLAUDE_CODE_OAUTH_TOKEN") && fnox.contains("GH"),
            "{notice}"
        );
        assert!(startup_notify(&startup).unwrap().contains("piton:startup"));
    }
}
