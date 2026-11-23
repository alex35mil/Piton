//! A [`Policy`] as a `nono::CapabilitySet`, and `piton sandbox-exec`: apply it to this process, then become pi.
//!
//! Everything is readable through one typed grant of `/`; the network is open (the library's default). Every other
//! rule is a raw Seatbelt rule, which the library emits after its own allows in the order given, so Seatbelt's
//! last-match semantics are the policy's.

use std::path::Path;

use nono::{AccessMode, CapabilitySet, IpcMode};

use super::rules::Policy;

/// The environment variable the daemon passes a sandboxed launch's policy in (JSON). Removed before pi starts.
pub const POLICY_ENV: &str = "PITON_SANDBOX_POLICY";

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("no sandbox policy: {POLICY_ENV} isn't set")]
    Missing,
    #[error("invalid sandbox policy: {0}")]
    Invalid(String),
    #[error("can't apply the sandbox: {0}")]
    Sandbox(String),
    #[error("no command to run")]
    NoCommand,
    #[error("can't run {program}: {source}")]
    Exec { program: String, source: std::io::Error },
}

/// How a command the daemon runs for a sandboxed launch is contained. macOS doesn't reliably start a sandbox inside
/// another one, so it's decided by whether this process already runs in one, asked of the kernel, never of an
/// environment variable a workspace could set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// This process isn't sandboxed: the command runs through `sandbox-exec` with the launch's policy.
    Apply,
    /// This process is already sandboxed (`piton sandbox show` run by an agent): the command runs directly and stays
    /// inside the current sandbox, which nothing inside can widen.
    Inherited,
}

/// How to start a command the daemon runs contained by a launch's policy: through `helper sandbox-exec` with the policy
/// in [`POLICY_ENV`], or directly when this process is already sandboxed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contained {
    pub program: String,
    pub args: Vec<std::ffi::OsString>,
    pub env: crate::launch::Env,
}

/// `program args` with `env`, contained by `policy`.
pub fn contained(
    helper: &Path,
    policy: &Policy,
    containment: Containment,
    program: String,
    args: Vec<std::ffi::OsString>,
    env: &crate::launch::Env,
) -> Result<Contained, String> {
    Ok(match containment {
        Containment::Apply => {
            let policy = serde_json::to_string(policy).map_err(|e| format!("can't encode the sandbox: {e}"))?;
            // The helper is unsandboxed until it applies the policy: `env` reaches only the command.
            let env = crate::launch::handoff::for_helper(env, [(POLICY_ENV.to_string(), policy)].into_iter().collect())
                .map_err(|e| format!("can't encode the environment: {e}"))?;
            let mut sandboxed: Vec<std::ffi::OsString> = vec!["sandbox-exec".into(), "--".into(), program.into()];
            sandboxed.extend(args);
            Contained {
                program: helper.to_string_lossy().into_owned(),
                args: sandboxed,
                env,
            }
        }
        Containment::Inherited => Contained {
            program,
            args,
            env: env.clone(),
        },
    })
}

/// This process's [`Containment`].
pub fn containment() -> Containment {
    match current_process_sandboxed() {
        true => Containment::Inherited,
        false => Containment::Apply,
    }
}

#[allow(unsafe_code)]
fn current_process_sandboxed() -> bool {
    // libsystem_sandbox, part of libSystem: with no operation, nonzero when the process is sandboxed (-1 on error,
    // which then tries to apply the sandbox and fails visibly if it can't).
    unsafe extern "C" {
        fn sandbox_check(pid: libc::pid_t, operation: *const libc::c_char, filter: libc::c_int, ...) -> libc::c_int;
    }
    // SAFETY: a null operation with the "no filter" type (0) takes no further arguments; the call only reads the pid.
    unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) > 0 }
}

/// The capability set for `policy`.
pub fn capabilities(policy: &Policy) -> Result<CapabilitySet, ApplyError> {
    let sandbox = |e: nono::NonoError| ApplyError::Sandbox(e.to_string());
    let mut caps = CapabilitySet::new()
        .allow_path(Path::new("/"), AccessMode::Read)
        .map_err(sandbox)?;
    // Multiprocessing runtimes (Python, joblib) need POSIX semaphores.
    caps.set_ipc_mode_mut(IpcMode::Full);
    for rule in policy.seatbelt().map_err(ApplyError::Invalid)? {
        caps.add_platform_rule(rule).map_err(sandbox)?;
    }
    Ok(caps)
}

/// Sends fnox's proxy URL (its `HTTPS_PROXY`, set for this process) to the daemon over `agent.sock`, once. Failure only
/// leaves the gate refusing connections, which says why.
fn report_upstream(token: &str, env: Option<&crate::launch::Env>) {
    let url = std::env::var("HTTPS_PROXY").ok();
    let socket = env
        .and_then(|e| e.get(crate::agent::SOCKET_ENV).cloned())
        .or_else(|| std::env::var(crate::agent::SOCKET_ENV).ok());
    let (Some(url), Some(socket)) = (url, socket) else {
        eprintln!("piton sandbox-exec: no fnox proxy to report to the network gate");
        return;
    };
    let request = serde_json::json!({ "type": "gate_upstream", "token": token, "url": url });
    let sent = std::os::unix::net::UnixStream::connect(&socket).and_then(|mut s| {
        use std::io::{BufRead, Write};
        writeln!(s, "{request}")?;
        let mut line = String::new();
        std::io::BufReader::new(s).read_line(&mut line)?;
        Ok(line)
    });
    match sent {
        Ok(line) if line.contains("\"reported\"") => {}
        Ok(line) => eprintln!(
            "piton sandbox-exec: the daemon didn't take fnox's proxy: {}",
            line.trim()
        ),
        Err(e) => eprintln!("piton sandbox-exec: can't report fnox's proxy to the daemon: {e}"),
    }
}

/// `piton sandbox-exec -- <command...>`: reads the policy from the environment, sandboxes this process, and replaces
/// it with the command. Returns only on failure.
pub fn exec(command: &[std::ffi::OsString]) -> ApplyError {
    let run = || -> Result<std::convert::Infallible, ApplyError> {
        let text = std::env::var(POLICY_ENV).map_err(|_| ApplyError::Missing)?;
        let policy: Policy = serde_json::from_str(&text).map_err(|e| ApplyError::Invalid(e.to_string()))?;
        let caps = capabilities(&policy)?;
        let (program, args) = command.split_first().ok_or(ApplyError::NoCommand)?;
        // Behind fnox, pi's environment comes from the handoff (fnox itself ran with the daemon's base environment).
        let handoff = crate::launch::handoff::from_process().map_err(|e| ApplyError::Invalid(e.to_string()))?;
        // Behind fnox with a filtered network: tell the session's gate where fnox's proxy is, before the sandbox closes
        // the daemon's socket to anything but `agent.sock` (which stays open, but the token is spent once).
        if let Ok(token) = std::env::var(crate::agent::GATE_TOKEN_ENV) {
            report_upstream(&token, handoff.as_ref());
        }
        nono::Sandbox::apply_auto(&caps).map_err(|e| ApplyError::Sandbox(e.to_string()))?;
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        // The command's environment: the handoff's, else this process's own.
        let mut env: crate::launch::Env = match handoff {
            Some(env) => env,
            None => std::env::vars().collect(),
        };
        env.remove(POLICY_ENV);
        env.remove(crate::agent::GATE_TOKEN_ENV);
        // A filtered network's processes reach hosts only through their gate, whatever the environment said.
        if let Some((url, ssh)) = policy.network.proxy() {
            crate::launch::sandbox::network::set_proxy_env(&mut env, &url, ssh);
        }
        // With the environment replaced, the program is found through its PATH.
        cmd.env_clear().envs(env);
        let source = cmd.exec();
        Err(ApplyError::Exec {
            program: program.to_string_lossy().into_owned(),
            source,
        })
    };
    match run() {
        Ok(never) => match never {},
        Err(e) => e,
    }
}
