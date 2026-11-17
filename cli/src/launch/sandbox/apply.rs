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

/// `piton sandbox-exec -- <command...>`: reads the policy from the environment, sandboxes this process, and replaces
/// it with the command. Returns only on failure.
pub fn exec(command: &[std::ffi::OsString]) -> ApplyError {
    let run = || -> Result<std::convert::Infallible, ApplyError> {
        let text = std::env::var(POLICY_ENV).map_err(|_| ApplyError::Missing)?;
        let policy: Policy = serde_json::from_str(&text).map_err(|e| ApplyError::Invalid(e.to_string()))?;
        let caps = capabilities(&policy)?;
        let (program, args) = command.split_first().ok_or(ApplyError::NoCommand)?;
        nono::Sandbox::apply_auto(&caps).map_err(|e| ApplyError::Sandbox(e.to_string()))?;
        use std::os::unix::process::CommandExt;
        let source = std::process::Command::new(program)
            .args(args)
            .env_remove(POLICY_ENV)
            .exec();
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
