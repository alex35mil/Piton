//! A bash call of a sandboxed session, run by the daemon: a sandboxed process can't apply a new sandbox, so the daemon,
//! outside any, starts each call where its [`Runtime`] says: through `piton sandbox-exec` in its own sandbox (the
//! session's policy, or one a grant or the user's approval widened, tagged with the call so the kernel's record of a
//! refusal names it), or, when the user chose it, on the host with no sandbox. It runs in its own process group; its
//! output streams back to the agent's tool call. As in pi's own bash, the call ends when the shell exits (its
//! background jobs keep running); the agent's stop or the timeout kills the whole group.

use std::{path::Path, process::Stdio, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{io::AsyncReadExt, process::Command, sync::mpsc};

use super::socket::{WakeOnDrop, Writer, send};
use crate::{
    config::CommandPattern,
    daemon::Daemon,
    launch::{Env, sandbox::Policy},
    session::{SessionCmd, SessionKey},
};

/// How long output may still arrive after the shell exits, as pi waits for its pipes (`EXIT_STDIO_GRACE_MS`).
const EXIT_GRACE: Duration = Duration::from_millis(100);

/// A request to run a bash call through the daemon (`agent.sock`): every bash call and `!` command of a sandboxed
/// session. `rerun` asks to run it again after the sandbox blocked it, hinting where (the user is asked).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RunRequest {
    /// pi's id of the bash tool call (the daemon accepts only a call pi reported running with this command), or
    /// `user:<n>` for a command the user typed (`!`).
    pub call_id: String,
    pub command: String,
    pub cwd: std::path::PathBuf,
    /// Seconds, as the bash tool takes it.
    #[serde(default)]
    pub timeout: Option<f64>,
    /// pi's shell environment for the call (its `PI_*` variables, its bin folder on `PATH`): used when the call runs
    /// at the session's own policy. Like every command's environment, it reaches only the command, after the sandbox is
    /// applied, never the unsandboxed helper.
    #[serde(default)]
    pub env: Env,
    /// Why the agent asks to rerun, shown to the user.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub rerun: Option<Rerun>,
}

/// The agent's rerun request: the runtime it hints, which the question opens on. The user may choose the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rerun {
    pub runtime: super::question::Target,
}

impl RunRequest {
    /// A command the user typed (`!`), not a tool call.
    pub fn typed_by_user(&self) -> bool {
        self.call_id.starts_with("user:")
    }
}

/// What the session decided for a request.
#[derive(Debug, Clone)]
pub enum RunDecision {
    /// The daemon runs it.
    Run(Box<Approved>),
    /// Not run: a refusal (the request doesn't match a running call, a question is pending), or the user's answer.
    NotRun { reason: String },
}

/// What the session says about a call that failed in its own sandbox.
#[derive(Debug)]
pub enum Failed {
    /// Nothing recorded yet: look again shortly.
    Wait,
    /// No question: the blocks the agent is shown (each `operation target`, with why it isn't asked about) and a note.
    Report { blocks: Vec<String>, note: Option<String> },
    /// The user is asked: the blocks, and the decision that follows.
    Asked {
        blocks: Vec<String>,
        decision: tokio::sync::oneshot::Receiver<RunDecision>,
    },
}

/// Where a command runs, decided by the daemon. Only [`Runtime::Host`] runs without a sandbox, and only a saved
/// `runtime = "host"` entry or the user's answer makes one.
#[derive(Debug, Clone, PartialEq)]
pub enum Runtime {
    /// The session's own sandbox: a plain call, or a command the user typed. Tagged with the call.
    Session(Policy),
    /// The session's sandbox widened (never past the floor), tagged with the call.
    Widened { policy: Policy, by: Why },
    /// The host, with no sandbox.
    Host { by: Why },
}

/// What made a command run widened or on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// A saved `[[sandbox.commands]]` entry matched it.
    Grant(CommandPattern),
    /// The user approved it in the block question.
    Approval,
    /// The user approved this exact command for the session in an earlier block question.
    Session,
}

impl Runtime {
    /// What the agent is told about where it ran: `None` for the session's own sandbox (nothing changed).
    pub fn ran(&self) -> Option<Ran> {
        let by = |why: &Why| match why {
            Why::Grant(_) => RanBy::Grant,
            Why::Approval => RanBy::Approval,
            Why::Session => RanBy::Session,
        };
        match self {
            Runtime::Session(_) => None,
            Runtime::Widened { by: why, .. } => Some(Ran {
                runtime: RanRuntime::Sandbox,
                by: by(why),
            }),
            Runtime::Host { by: why } => Some(Ran {
                runtime: RanRuntime::Host,
                by: by(why),
            }),
        }
    }
}

/// Where a widened or host run ran, as sent to the agent before its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Ran {
    pub runtime: RanRuntime,
    pub by: RanBy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RanRuntime {
    /// A widened sandbox.
    Sandbox,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RanBy {
    Grant,
    Approval,
    /// Approved for this session.
    Session,
}

/// The variables a launch sets that reach `agent.sock`: never given to a host run, which has no sandbox and nothing to
/// ask the channel.
const SESSION_ONLY: [&str; 2] = [super::TOKEN_ENV, super::SOCKET_ENV];

/// What `run_command` answers, line by line: where it runs, its output, then how it ended.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunLine {
    /// Where a widened or host run runs, before its output: the agent's result says so.
    Ran(Ran),
    /// A chunk of the running command's output, base64.
    Output {
        data: String,
    },
    /// The daemon's run ended: the exit code, or 128 + the signal.
    Exit {
        code: i32,
    },
    /// The blocks the daemon recorded for a failed run (`operation target`, with why one isn't asked about), and a note.
    Blocks {
        blocks: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// The run failed on a block and the user is asked to run it again: what follows is the rerun's, or `not_rerun`.
    Blocked,
    /// The user didn't approve running it again: the first run's exit follows.
    NotRerun {
        reason: String,
    },
    TimedOut,
    /// Not run, with why: the user denied it, didn't answer, or the request doesn't match a running call.
    NotRun {
        reason: String,
    },
    /// The run couldn't happen: the session ended, or the command couldn't be started.
    Error {
        message: String,
    },
}

/// How long a failed call waits for the kernel's record of its blocks: it usually comes within ~5 ms
/// (docs/pi-findings.md), and the wait ends at the first look that finds one.
const BLOCK_WAIT: Duration = Duration::from_millis(50);
/// How often a failed call looks for its blocks while it waits.
const BLOCK_POLL: Duration = Duration::from_millis(10);

/// Decides a bash call with its session (a grant, the user's answer), runs it when approved, and streams it back. A
/// call that fails in the session's own sandbox on a block an approved rerun lifts is asked about, and the rerun's
/// result is the call's.
pub async fn serve(daemon: &Daemon, session: SessionKey, request: RunRequest, write: &mut Writer) {
    let ended = || RunLine::Error {
        message: "unknown or ended session".into(),
    };
    let Some(handle) = daemon.session(session) else {
        send(write, &ended()).await;
        return;
    };
    let (reply, decision) = tokio::sync::oneshot::channel();
    let asked_for = request.clone();
    if handle.tx.send(SessionCmd::RunCommand { request, reply }).is_err() {
        send(write, &ended()).await;
        return;
    }
    // Wakes the session when the agent stops waiting, so it closes the question at once.
    let _wake = WakeOnDrop(handle.tx.clone(), || SessionCmd::SettleQuestion);
    let unanswered = || RunLine::Error {
        message: "the session ended before the user answered".into(),
    };
    let approved = match decision.await {
        Ok(RunDecision::Run(approved)) => approved,
        Ok(RunDecision::NotRun { reason }) => {
            send(write, &RunLine::NotRun { reason }).await;
            return;
        }
        Err(_) => {
            send(write, &unanswered()).await;
            return;
        }
    };
    let Some(end) = execute(daemon, &approved, session, write).await else {
        return;
    };
    let code = match (&approved.runtime, end) {
        (Runtime::Session(_), RunLine::Exit { code }) if code != 0 => code,
        (_, last) => {
            send(write, &last).await;
            return;
        }
    };
    // Failed in the session's own sandbox: the session decides whether to ask, after the kernel's record had its time.
    let started = tokio::time::Instant::now();
    let nothing = || Failed::Report {
        blocks: Vec::new(),
        note: None,
    };
    let failed = loop {
        let last = started.elapsed() >= BLOCK_WAIT;
        let (reply, answer) = tokio::sync::oneshot::channel();
        let asked = SessionCmd::RunFailed {
            request: asked_for.clone(),
            last,
            reply,
        };
        if handle.tx.send(asked).is_err() {
            break nothing();
        }
        match answer.await {
            Ok(Failed::Wait) => tokio::time::sleep(BLOCK_POLL).await,
            Ok(failed @ (Failed::Report { .. } | Failed::Asked { .. })) => break failed,
            Err(_) => break nothing(),
        }
    };
    let first_exit = RunLine::Exit { code };
    match failed {
        // The loop above waits it out.
        Failed::Wait => {}
        Failed::Report { blocks, note } => {
            if (!blocks.is_empty() || note.is_some()) && !send(write, &RunLine::Blocks { blocks, note }).await {
                return;
            }
            send(write, &first_exit).await;
        }
        Failed::Asked { blocks, decision } => {
            if !send(write, &RunLine::Blocks { blocks, note: None }).await || !send(write, &RunLine::Blocked).await {
                return;
            }
            let reason = match decision.await {
                Ok(RunDecision::Run(rerun)) => {
                    if let Some(last) = execute(daemon, &rerun, session, write).await {
                        send(write, &last).await;
                    }
                    return;
                }
                Ok(RunDecision::NotRun { reason }) => reason,
                Err(_) => "the session ended before the user answered".to_string(),
            };
            if send(write, &RunLine::NotRerun { reason }).await {
                send(write, &first_exit).await;
            }
        }
    }
}

/// Runs `approved` and streams its output: the `ran` line first when it isn't the session's own sandbox. A sandboxed
/// run gets a call token of its own in place of pi's, valid for its process group until it ends. Returns the line that
/// ends it (not sent yet); `None` when the agent stopped listening.
async fn execute(daemon: &Daemon, approved: &Approved, session: SessionKey, write: &mut Writer) -> Option<RunLine> {
    let helper = match crate::launch::helper() {
        Ok(helper) => helper,
        Err(e) => return Some(RunLine::Error { message: e.to_string() }),
    };
    if let Some(ran) = approved.runtime.ran()
        && !send(write, &RunLine::Ran(ran)).await
    {
        return None;
    }
    let call = match &approved.runtime {
        Runtime::Session(_) | Runtime::Widened { .. } => match daemon.agent_tokens().issue_call(session) {
            Ok(call) => Some(call),
            Err(e) => {
                return Some(RunLine::Error {
                    message: format!("can't make the call's token: {e}"),
                });
            }
        },
        Runtime::Host { .. } => None,
    };
    let env = call_env(&approved.env, call.as_ref());
    let (events, mut output) = mpsc::unbounded_channel();
    let spec = RunSpec {
        helper: &helper,
        runtime: &approved.runtime,
        shell: &approved.shell,
        command: &approved.command,
        cwd: &approved.cwd,
        env: &env,
        timeout: approved.timeout,
        call: call.as_ref(),
    };
    let run = run(spec, events);
    tokio::pin!(run);
    let output_line = |chunk: &[u8]| RunLine::Output {
        data: STANDARD.encode(chunk),
    };
    let end = loop {
        tokio::select! {
            end = &mut run => break end,
            Some(RunEvent::Output(chunk)) = output.recv() => {
                if !send(write, &output_line(&chunk)).await {
                    return None;
                }
            }
        }
    };
    // What the command printed before it ended.
    while let Ok(RunEvent::Output(chunk)) = output.try_recv() {
        if !send(write, &output_line(&chunk)).await {
            return None;
        }
    }
    let last = match end {
        RunEnd::Exited { code } => {
            crate::log!("session {session}: command run by the daemon exited with {code}");
            RunLine::Exit { code }
        }
        RunEnd::TimedOut => {
            crate::log!("session {session}: command run by the daemon timed out");
            RunLine::TimedOut
        }
        RunEnd::Failed { reason } => {
            crate::log!("session {session}: command run by the daemon failed: {reason}");
            RunLine::Error { message: reason }
        }
    };
    Some(last)
}

/// A run's environment: `env` with the call's own token in place of whatever token it held (pi's, from the launch
/// environment or the extension's), or with no token at all when the run has none (a host run).
fn call_env(env: &Env, call: Option<&super::channel::CallToken>) -> Env {
    let mut env = env.clone();
    match call {
        Some(call) => env.insert(super::TOKEN_ENV.into(), call.token.clone()),
        None => env.remove(super::TOKEN_ENV),
    };
    env
}

/// A host run's environment: the session's launch environment (the workspace's environment tools included, as the
/// user's own shell would have them) without [`SESSION_ONLY`].
pub fn host_env(launch: &Env) -> Env {
    let mut env = launch.clone();
    for name in SESSION_ONLY {
        env.remove(name);
    }
    env
}

/// A command the daemon runs, everything decided by the daemon.
#[derive(Debug, Clone)]
pub struct Approved {
    pub runtime: Runtime,
    pub shell: String,
    /// The command with pi's `shellCommandPrefix`.
    pub command: String,
    pub cwd: std::path::PathBuf,
    pub env: Env,
    pub timeout: Option<Duration>,
}

/// pi's shell settings, read the way pi reads them: `shellPath` and `shellCommandPrefix` from the agent directory's
/// `settings.json`, then the workspace's `.pi/settings.json` over them. Neither file is writable from the sandbox (the
/// agent directory's settings and protected files), so a request can't choose the shell a granted command runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiShell {
    pub shell: String,
    pub prefix: Option<String>,
}

impl PiShell {
    /// From the launch environment (`PI_CODING_AGENT_DIR`, else `~/.pi/agent`) and the workspace.
    pub fn read(env: &Env, home: &Path, workspace: &Path) -> Self {
        let agent_dir = env
            .get("PI_CODING_AGENT_DIR")
            .filter(|v| !v.is_empty())
            .map(|v| crate::config::expand_home(v, home))
            .map(|p| if p.is_absolute() { p } else { workspace.join(p) })
            .unwrap_or_else(|| home.join(".pi/agent"));
        let mut shell = None;
        let mut prefix = None;
        for file in [agent_dir.join("settings.json"), workspace.join(".pi/settings.json")] {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(serde_json::Value::Object(settings)) = serde_json::from_str(&text) else {
                continue;
            };
            if let Some(path) = settings.get("shellPath").and_then(serde_json::Value::as_str) {
                shell = Some(crate::config::expand_home(path, home).to_string_lossy().into_owned());
            }
            if let Some(p) = settings.get("shellCommandPrefix").and_then(serde_json::Value::as_str) {
                prefix = Some(p.to_string());
            }
        }
        Self {
            shell: shell.unwrap_or_else(|| "/bin/bash".into()),
            prefix: prefix.filter(|p| !p.is_empty()),
        }
    }

    /// `command` as pi runs it: after the prefix, on its own line.
    pub fn command(&self, command: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        }
    }
}

/// One command to run.
#[derive(Debug, Clone)]
pub struct RunSpec<'a> {
    /// `<piton> sandbox-exec`, for a sandboxed runtime.
    pub helper: &'a Path,
    pub runtime: &'a Runtime,
    /// The shell (`/bin/bash`, or pi's `shellPath`) and the command it runs with `-c`.
    pub shell: &'a str,
    pub command: &'a str,
    pub cwd: &'a Path,
    pub env: &'a Env,
    pub timeout: Option<Duration>,
    /// The call's token, bound to the run's process group once it starts.
    pub call: Option<&'a super::channel::CallToken>,
}

/// What a run sends while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    Output(Vec<u8>),
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// The shell exited: its code, or 128 + the signal that ended it.
    Exited {
        code: i32,
    },
    TimedOut,
    /// The command couldn't be started.
    Failed {
        reason: String,
    },
}

/// Runs `spec`, sending its output to `events` as it comes, until the shell exits. Dropping the returned future (the
/// agent stopped waiting) or the timeout kills the process group.
pub async fn run(spec: RunSpec<'_>, events: mpsc::UnboundedSender<RunEvent>) -> RunEnd {
    let mut command = match spec.runtime {
        Runtime::Session(policy) | Runtime::Widened { policy, .. } => match sandboxed(&spec, policy) {
            Ok(command) => command,
            Err(reason) => return RunEnd::Failed { reason },
        },
        // No helper and no policy: the shell itself, with the environment the daemon chose for a host run.
        Runtime::Host { .. } => {
            let mut command = Command::new(spec.shell);
            command.args(["-c", spec.command]).env_clear().envs(spec.env);
            command
        }
    };
    let program = command.as_std().get_program().to_string_lossy().into_owned();
    let child = command
        .current_dir(spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            return RunEnd::Failed {
                reason: format!("can't start {program}: {e}"),
            };
        }
    };
    let group = child.id().map(|pid| nix::unistd::Pid::from_raw(pid as i32));
    // The group is the child's pid (`process_group(0)`): the call's token is its processes' from now on.
    if let (Some(call), Some(group)) = (spec.call, group) {
        call.started(group.as_raw());
    }
    // Kills the whole group when the run is stopped or times out: background processes the command started too. A
    // finished command disarms it, leaving its background jobs running.
    let mut guard = KillGroup(group);
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return RunEnd::Failed {
            reason: "no output pipes".into(),
        };
    };
    let forward = |events: mpsc::UnboundedSender<RunEvent>| {
        move |chunk: &[u8]| {
            let _ = events.send(RunEvent::Output(chunk.to_vec()));
        }
    };
    let pipes = async {
        tokio::join!(
            pump(&mut stdout, forward(events.clone())),
            pump(&mut stderr, forward(events))
        );
    };
    let waited = async {
        tokio::pin!(pipes);
        let mut drained = false;
        let status = tokio::select! {
            status = child.wait() => status,
            // Both pipes closed first: the shell's exit follows.
            () = &mut pipes => {
                drained = true;
                child.wait().await
            }
        };
        // A background job holding the pipes doesn't hold the call: what arrives soon after the exit is kept.
        if !drained {
            let _ = tokio::time::timeout(EXIT_GRACE, &mut pipes).await;
        }
        status
    };
    let status = match spec.timeout {
        Some(limit) => match tokio::time::timeout(limit, waited).await {
            Ok(status) => status,
            Err(_) => return RunEnd::TimedOut,
        },
        None => waited.await,
    };
    guard.0 = None;
    match status {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            let code = status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(1);
            RunEnd::Exited { code }
        }
        Err(e) => RunEnd::Failed {
            reason: format!("waiting for the command: {e}"),
        },
    }
}

/// `<piton> sandbox-exec -- <shell> -c <command>` with `policy`. The helper is unsandboxed until it applies the policy:
/// the command's environment (which a request may have chosen) reaches only the command, after the sandbox is on.
fn sandboxed(spec: &RunSpec<'_>, policy: &Policy) -> Result<Command, String> {
    let policy = serde_json::to_string(policy).map_err(|e| format!("can't encode the sandbox: {e}"))?;
    let own = [(crate::launch::sandbox::POLICY_ENV.to_string(), policy)]
        .into_iter()
        .collect();
    let env =
        crate::launch::handoff::for_helper(spec.env, own).map_err(|e| format!("can't encode the environment: {e}"))?;
    let mut command = Command::new(spec.helper);
    command
        .args(["sandbox-exec", "--", spec.shell, "-c", spec.command])
        .env_clear()
        .envs(&env);
    Ok(command)
}

async fn pump(reader: &mut (impl AsyncReadExt + Unpin), mut send: impl FnMut(&[u8])) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => send(&buf[..n]),
        }
    }
}

struct KillGroup(Option<nix::unistd::Pid>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for `piton sandbox-exec`: says what it got itself (a policy, the command's `X`), then runs the rest of
    /// its arguments after `--` in the environment the handoff carries, so the tests need no sandbox (sandboxes don't
    /// nest).
    fn fake_helper(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("helper");
        std::fs::write(
            &path,
            "#!/bin/sh\n[ \"$1\" = sandbox-exec ] && [ \"$2\" = -- ] || exit 99\nshift 2\necho \"policy=${PITON_SANDBOX_POLICY:+set} \
             own-x=${X:-none}\"\nexec /usr/bin/env -i $(printf '%s' \"$PITON_LAUNCH_ENV\" | /usr/bin/python3 -I -c 'import \
             json,sys,shlex; print(\" \".join(shlex.quote(k+\"=\"+v) for k,v in json.load(sys.stdin)[\"env\"].items()))') \"$@\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn policy() -> Policy {
        Policy {
            rules: Vec::new(),
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
            tag: String::new(),
        }
    }

    async fn collect(spec: RunSpec<'_>) -> (String, RunEnd) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let end = run(spec, tx).await;
        let mut out = Vec::new();
        while let Ok(RunEvent::Output(chunk)) = rx.try_recv() {
            out.extend(chunk);
        }
        (String::from_utf8_lossy(&out).into_owned(), end)
    }

    /// The shell and prefix come from pi's settings files, the workspace's over the agent directory's.
    #[test]
    fn reads_pi_shell_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let workspace = home.join("w");
        let none = PiShell::read(&Env::new(), home, &workspace);
        assert_eq!(
            none,
            PiShell {
                shell: "/bin/bash".into(),
                prefix: None
            }
        );
        assert_eq!(none.command("ls"), "ls");
        std::fs::create_dir_all(home.join("agent")).unwrap();
        std::fs::write(
            home.join("agent/settings.json"),
            r#"{"shellPath": "~/bin/zsh", "shellCommandPrefix": "shopt -s expand_aliases"}"#,
        )
        .unwrap();
        let env: Env = [("PI_CODING_AGENT_DIR".to_string(), "~/agent".to_string())]
            .into_iter()
            .collect();
        let read = PiShell::read(&env, home, &workspace);
        assert_eq!(read.shell, home.join("bin/zsh").to_string_lossy());
        assert_eq!(read.command("ls"), "shopt -s expand_aliases\nls");
        std::fs::create_dir_all(workspace.join(".pi")).unwrap();
        std::fs::write(
            workspace.join(".pi/settings.json"),
            r#"{"shellCommandPrefix": "set -e"}"#,
        )
        .unwrap();
        assert_eq!(PiShell::read(&env, home, &workspace).command("ls"), "set -e\nls");
    }

    #[tokio::test]
    async fn output_streams_and_the_exit_code_returns() {
        let dir = tempfile::tempdir().unwrap();
        let helper = fake_helper(dir.path());
        let env: Env = [
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("X".to_string(), "1".to_string()),
        ]
        .into_iter()
        .collect();
        let sandboxed = Runtime::Session(policy());
        let (out, end) = collect(RunSpec {
            helper: &helper,
            runtime: &sandboxed,
            shell: "/bin/sh",
            command: "echo out; echo err >&2; pwd; echo cmd-x=$X; exit 3",
            cwd: dir.path(),
            env: &env,
            timeout: None,
            call: None,
        })
        .await;
        assert!(
            out.contains("policy=set own-x=none"),
            "the helper doesn't get the command's environment: {out}"
        );
        assert!(out.contains("out\n") && out.contains("err\n"), "{out}");
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        assert!(out.contains(&cwd.display().to_string()), "{out}");
        assert!(out.contains("cmd-x=1"), "the command does: {out}");
        assert_eq!(end, RunEnd::Exited { code: 3 });

        // On the host: the shell itself, no helper and no policy, with the given environment.
        let host = Runtime::Host { by: Why::Approval };
        let (out, end) = collect(RunSpec {
            helper: &helper,
            runtime: &host,
            shell: "/bin/sh",
            command: "echo policy=${PITON_SANDBOX_POLICY:-none} cmd-x=$X; pwd; exit 4",
            cwd: dir.path(),
            env: &env,
            timeout: None,
            call: None,
        })
        .await;
        assert!(!out.contains("own-x"), "the helper didn't run: {out}");
        assert!(out.contains("policy=none cmd-x=1"), "{out}");
        assert!(out.contains(&cwd.display().to_string()), "{out}");
        assert_eq!(end, RunEnd::Exited { code: 4 });
    }

    /// A call's token is bound to the run's own process group once it starts; its processes may use it, others may not.
    #[tokio::test]
    async fn a_run_binds_its_call_token_to_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let helper = fake_helper(dir.path());
        let tokens = std::sync::Arc::new(super::super::Tokens::default());
        let call = tokens.issue_call(1).unwrap();
        let env: Env = [("PATH".to_string(), "/usr/bin:/bin".to_string())]
            .into_iter()
            .collect();
        let host = Runtime::Host { by: Why::Approval };
        let (out, end) = collect(RunSpec {
            helper: &helper,
            runtime: &host,
            shell: "/bin/sh",
            // On the host the shell is the run's first process: its pid is the group's.
            command: "echo $$",
            cwd: dir.path(),
            env: &env,
            timeout: None,
            call: Some(&call),
        })
        .await;
        assert_eq!(end, RunEnd::Exited { code: 0 });
        let group: i32 = out.trim().parse().unwrap();
        let member = super::super::channel::Peer {
            pid: group,
            group: Some(group),
            #[cfg(target_os = "macos")]
            audit: super::super::channel::AuditToken([0; 8]),
        };
        assert!(matches!(
            tokens.accept(&call.token, &member),
            Ok(super::super::channel::Caller::Call { session: 1, .. })
        ));
        let outsider = super::super::channel::Peer {
            group: Some(std::process::id() as i32),
            ..member
        };
        assert!(tokens.accept(&call.token, &outsider).is_err());
    }

    /// A run's environment carries its own call token in place of pi's, and a host run none.
    #[test]
    fn a_run_never_carries_pi_s_token() {
        let tokens = std::sync::Arc::new(super::super::Tokens::default());
        let call = tokens.issue_call(1).unwrap();
        let env: Env = [(super::super::TOKEN_ENV.to_string(), "pi's".to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            call_env(&env, Some(&call)).get(super::super::TOKEN_ENV),
            Some(&call.token)
        );
        assert_eq!(call_env(&env, None).get(super::super::TOKEN_ENV), None);
    }

    /// A host run gets the session's launch environment without what reaches `agent.sock`.
    #[test]
    fn a_host_run_drops_the_session_s_own_variables() {
        let launch: Env = [
            ("PATH", "/w/bin:/usr/bin"),
            ("RUST_LOG", "debug"),
            (super::super::TOKEN_ENV, "t"),
            (super::super::SOCKET_ENV, "/s/agent.sock"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let env = host_env(&launch);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/w/bin:/usr/bin"));
        assert_eq!(env.get("RUST_LOG").map(String::as_str), Some("debug"));
        for gone in SESSION_ONLY {
            assert!(!env.contains_key(gone), "{gone}");
        }
    }

    #[test]
    fn only_a_widened_or_host_run_is_reported() {
        assert_eq!(Runtime::Session(policy()).ran(), None);
        let grant = Why::Grant(CommandPattern::new("log show *").unwrap());
        assert_eq!(
            Runtime::Widened {
                policy: policy(),
                by: Why::Approval
            }
            .ran(),
            Some(Ran {
                runtime: RanRuntime::Sandbox,
                by: RanBy::Approval
            })
        );
        assert_eq!(
            Runtime::Host { by: grant }.ran(),
            Some(Ran {
                runtime: RanRuntime::Host,
                by: RanBy::Grant
            })
        );
    }

    /// A timeout, and dropping the run (the agent's stop), kill the whole process group: a background child too. A
    /// command that finishes leaves its background job running, as pi's bash does, even while it holds the pipes.
    #[tokio::test]
    async fn stopping_kills_the_group_and_finishing_doesnt() {
        let dir = tempfile::tempdir().unwrap();
        let helper = fake_helper(dir.path());
        let env: Env = [("PATH".to_string(), "/usr/bin:/bin".to_string())]
            .into_iter()
            .collect();
        for runtime in [Runtime::Session(policy()), Runtime::Host { by: Why::Approval }] {
            let sub = dir.path().join(format!("{}", runtime.ran().is_some()));
            std::fs::create_dir_all(&sub).unwrap();
            stops_and_finishes(&helper, &runtime, &sub, &env).await;
        }
    }

    async fn stops_and_finishes(helper: &Path, runtime: &Runtime, dir: &Path, env: &Env) {
        let marker = |name: &str| dir.join(name);
        let spec = |command: &'static str, timeout| RunSpec {
            helper,
            runtime,
            shell: "/bin/sh",
            command,
            cwd: dir,
            env,
            timeout,
            call: None,
        };

        let (_, end) = collect(spec(
            "(sleep 1; touch timed) & sleep 5",
            Some(Duration::from_millis(200)),
        ))
        .await;
        assert_eq!(end, RunEnd::TimedOut);

        let (tx, _rx) = mpsc::unbounded_channel();
        let dropped = tokio::time::timeout(
            Duration::from_millis(200),
            run(spec("(sleep 1; touch stopped) & sleep 5", None), tx),
        )
        .await;
        assert!(dropped.is_err(), "still running when dropped");

        let started = std::time::Instant::now();
        let (out, end) = collect(spec("(sleep 1; touch kept) & echo started", None)).await;
        assert_eq!(end, RunEnd::Exited { code: 0 });
        assert!(out.contains("started"), "{out}");
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "the background job doesn't hold the call"
        );

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker("timed").exists(), "killed at the timeout");
        assert!(!marker("stopped").exists(), "killed when stopped");
        assert!(
            marker("kept").exists(),
            "a finished command's background job keeps running"
        );
    }
}
