//! Passing pi's environment past fnox. fnox runs on the host and reads its environment to find its config and the
//! programs its providers run (`HOME`, `XDG_CONFIG_HOME`, `FNOX_*`, `PATH` for `op`), so it gets only the daemon's base
//! environment, never the workspace's. pi's environment travels in [`HANDOFF_ENV`], which fnox passes to its child
//! unread; this binary (`piton sandbox-exec`, or `piton launch-exec` for a host launch) unpacks it and starts pi with
//! it, keeping every change fnox made to the base (its proxy and certificate variables, placeholders, removed
//! credentials).

use serde::{Deserialize, Serialize};

use super::Env;

/// The handoff's variable: pi's environment and the base environment fnox was started with, as JSON.
pub const HANDOFF_ENV: &str = "PITON_LAUNCH_ENV";

#[derive(Debug, Serialize, Deserialize)]
struct Handoff {
    /// The environment pi is meant to get.
    env: Env,
    /// The environment fnox was started with: what fnox's changes are measured against.
    base: Env,
}

#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    #[error("no launch environment: {HANDOFF_ENV} isn't set")]
    Missing,
    #[error("invalid launch environment: {0}")]
    Invalid(String),
    #[error("no command to run")]
    NoCommand,
    #[error("can't run {program}: {source}")]
    Exec { program: String, source: std::io::Error },
}

/// The handoff's value for pi's environment `env`, behind a fnox started with `base`.
pub fn encode(env: &Env, base: &Env) -> Result<String, serde_json::Error> {
    serde_json::to_string(&Handoff {
        env: env.clone(),
        base: base.clone(),
    })
}

/// pi's environment from this process's: `None` without a handoff (no fnox). With one, pi's intended environment, then
/// every variable fnox added or changed relative to the base, and none that fnox removed.
pub fn from_process() -> Result<Option<Env>, HandoffError> {
    let current: Env = std::env::vars().collect();
    let Some(text) = current.get(HANDOFF_ENV) else {
        return Ok(None);
    };
    let handoff: Handoff = serde_json::from_str(text).map_err(|e| HandoffError::Invalid(e.to_string()))?;
    Ok(Some(merge(handoff.env, &handoff.base, &current)))
}

fn merge(mut env: Env, base: &Env, current: &Env) -> Env {
    for (name, value) in current {
        if name != HANDOFF_ENV && base.get(name) != Some(value) {
            env.insert(name.clone(), value.clone());
        }
    }
    for name in base.keys() {
        if !current.contains_key(name) {
            env.remove(name);
        }
    }
    env
}

/// `piton launch-exec -- <command...>`: a host launch behind fnox. Replaces this process with the command, in pi's
/// environment from the handoff. Returns only on failure.
pub fn exec(command: &[std::ffi::OsString]) -> HandoffError {
    let run = || -> Result<std::convert::Infallible, HandoffError> {
        let env = from_process()?.ok_or(HandoffError::Missing)?;
        let (program, args) = command.split_first().ok_or(HandoffError::NoCommand)?;
        use std::os::unix::process::CommandExt;
        // With the environment replaced, the program is found through pi's PATH.
        let source = std::process::Command::new(program)
            .args(args)
            .env_clear()
            .envs(&env)
            .exec();
        Err(HandoffError::Exec {
            program: program.to_string_lossy().into_owned(),
            source,
        })
    };
    match run() {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// pi gets the workspace's values, and fnox's changes to the base win over them.
    #[test]
    fn fnox_changes_win_over_pis_environment() {
        let base = env(&[
            ("HOME", "/Users/me"),
            ("PATH", "/usr/bin"),
            ("GITHUB_TOKEN", "real"),
            ("AWS_SECRET_ACCESS_KEY", "real-aws"),
        ]);
        // The workspace's mise environment changed PATH and HOME and set a variable of its own.
        let pi = env(&[
            ("HOME", "/repo/.h"),
            ("PATH", "/repo/bin:/usr/bin"),
            ("GITHUB_TOKEN", "real"),
            ("AWS_SECRET_ACCESS_KEY", "real-aws"),
            ("FROM_MISE", "1"),
        ]);
        // fnox started its child with the base, a placeholder, its proxy variable, and a credential removed.
        let mut current = base.clone();
        current.insert("GITHUB_TOKEN".into(), "fnox_proxy_1".into());
        current.insert("HTTPS_PROXY".into(), "http://127.0.0.1:1".into());
        current.remove("AWS_SECRET_ACCESS_KEY");
        current.insert(HANDOFF_ENV.into(), "{}".into());
        assert_eq!(
            merge(pi, &base, &current),
            env(&[
                ("HOME", "/repo/.h"),
                ("PATH", "/repo/bin:/usr/bin"),
                ("GITHUB_TOKEN", "fnox_proxy_1"),
                ("HTTPS_PROXY", "http://127.0.0.1:1"),
                ("FROM_MISE", "1"),
            ])
        );
    }

    #[test]
    fn round_trips() {
        let pi = env(&[("A", "1")]);
        let base = env(&[("B", "2")]);
        let handoff: Handoff = serde_json::from_str(&encode(&pi, &base).unwrap()).unwrap();
        assert_eq!((handoff.env, handoff.base), (pi, base));
    }
}
