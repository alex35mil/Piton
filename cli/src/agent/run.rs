//! A bash call of a sandboxed session, run by the daemon: a sandboxed process can't apply a new sandbox, so the daemon,
//! outside any, starts each call through `piton sandbox-exec` in its own sandbox: the session's policy, or one a grant
//! or the user's approval widened, tagged with the call so the kernel's record of a refusal names it. It runs in its own
//! process group; its output streams back to the agent's tool call. As in pi's own bash, the call ends when the shell
//! exits (its background jobs keep running); the agent's stop or the timeout kills the whole group.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::{io::AsyncReadExt, process::Command, sync::mpsc};

use crate::launch::{Env, sandbox::Policy};

/// How long output may still arrive after the shell exits, as pi waits for its pipes (`EXIT_STDIO_GRACE_MS`).
const EXIT_GRACE: Duration = Duration::from_millis(100);

/// A request to run a bash call through the daemon (`agent.sock`): every bash call and `!` command of a sandboxed
/// session. `rerun: true` asks to run it again outside the sandbox after it failed there (the user is asked).
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
    pub rerun: bool,
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

/// A command the daemon runs, everything decided by the daemon.
#[derive(Debug, Clone)]
pub struct Approved {
    /// Tagged with the call.
    pub policy: Policy,
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
    /// `<piton> sandbox-exec`.
    pub helper: &'a Path,
    pub policy: &'a Policy,
    /// The shell (`/bin/bash`, or pi's `shellPath`) and the command it runs with `-c`.
    pub shell: &'a str,
    pub command: &'a str,
    pub cwd: &'a Path,
    pub env: &'a Env,
    pub timeout: Option<Duration>,
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
    let policy = match serde_json::to_string(spec.policy) {
        Ok(policy) => policy,
        Err(e) => {
            return RunEnd::Failed {
                reason: format!("can't encode the sandbox: {e}"),
            };
        }
    };
    // The helper is unsandboxed until it applies the policy: the command's environment (which a request may have
    // chosen) reaches only the command, after the sandbox is on.
    let own = [(crate::launch::sandbox::POLICY_ENV.to_string(), policy)]
        .into_iter()
        .collect();
    let env = match crate::launch::handoff::for_helper(spec.env, own) {
        Ok(env) => env,
        Err(e) => {
            return RunEnd::Failed {
                reason: format!("can't encode the environment: {e}"),
            };
        }
    };
    let child = Command::new(spec.helper)
        .args(["sandbox-exec", "--", spec.shell, "-c", spec.command])
        .current_dir(spec.cwd)
        .env_clear()
        .envs(&env)
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
                reason: format!("can't start {}: {e}", spec.helper.display()),
            };
        }
    };
    let group = child.id().map(|pid| nix::unistd::Pid::from_raw(pid as i32));
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
            network: Vec::new(),
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
        let policy = policy();
        let (out, end) = collect(RunSpec {
            helper: &helper,
            policy: &policy,
            shell: "/bin/sh",
            command: "echo out; echo err >&2; pwd; echo cmd-x=$X; exit 3",
            cwd: dir.path(),
            env: &env,
            timeout: None,
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
        let policy = policy();
        let marker = |name: &str| dir.path().join(name);
        let spec = |command: &'static str, timeout| RunSpec {
            helper: &helper,
            policy: &policy,
            shell: "/bin/sh",
            command,
            cwd: dir.path(),
            env: &env,
            timeout,
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
