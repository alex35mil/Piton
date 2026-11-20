//! Launching pi behind fnox's credential proxy (experimental): `fnox --non-interactive proxy run --no-sandbox -- <the
//! usual command>`. pi's environment holds placeholders; fnox, outside the sandbox, puts the real secrets into the
//! requests its rules allow.
//!
//! - fnox finds its own config files (the global one, the workspace and its parents). A workspace uses the proxy when
//!   one of them has a `[proxy]` table; otherwise its launches run without it, as if fnox were off.
//! - `fnox proxy rules` validates the config without resolving secrets: a broken config refuses the launch.
//! - A sandboxed launch is refused when the sandbox could change a file fnox loads, or read the keychain (where the
//!   credentials of fnox's secret sources live): an agent could redirect secrets or run `fnox get` itself.
//! - A secret fnox can't resolve doesn't stop the launch (fnox starts pi anyway and refuses the requests that need it):
//!   [`StartupLog`] reads fnox's warnings into the session's notice.
//!
//! fnox's messages read here aren't a documented interface; the tests pin the forms fnox 1.48 prints.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::{
    Env,
    sandbox::{Access, Policy},
};
use crate::{agent::canonical_target, tools::Program};

/// What fnox prints when no config with a `[proxy]` table applies to the folder.
const NO_PROXY_TABLE: &str = "No [proxy] configuration found";

/// A launch behind the proxy: fnox's command, the config files it loads for this workspace, and the environment it
/// runs with (the daemon's base environment: what those files were found with).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub program: Program,
    pub files: Vec<PathBuf>,
    pub env: Env,
}

impl Proxy {
    /// The command line that starts `argv` behind the proxy. `--no-sandbox`: Piton's sandbox applies (sandboxes don't
    /// nest), whatever `[proxy] sandbox` says. `argv` starts with this binary (`sandbox-exec` or `launch-exec`), which
    /// takes pi's environment from the handoff.
    pub fn wrap(&self, argv: Vec<String>) -> Vec<String> {
        let mut out = vec![self.program.program.clone()];
        out.extend(self.program.args.iter().cloned());
        out.extend(["--non-interactive", "proxy", "run", "--no-sandbox", "--"].map(String::from));
        out.extend(argv);
        out
    }

    /// fnox's environment for starting pi with `pi_env`: the base environment fnox's checks ran with, plus `extra`
    /// (the sandbox policy) and the handoff carrying `pi_env`. The workspace's environment never reaches fnox itself.
    pub fn environment(&self, pi_env: &Env, extra: &Env) -> Result<Env, serde_json::Error> {
        let mut env = self.env.clone();
        env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        // Measured against everything fnox starts with, so only fnox's own changes carry over to pi.
        let handoff = super::handoff::encode(pi_env, &env)?;
        env.insert(super::handoff::HANDOFF_ENV.into(), handoff);
        Ok(env)
    }

    /// Refuses a sandboxed launch whose sandbox could change what fnox loads or read the keychain.
    pub fn check_sandbox(&self, policy: &Policy, home: &Path) -> Result<(), FnoxError> {
        if let Some(file) = self
            .files
            .iter()
            .find(|f| policy.decide(&canonical_target(f), Access::Write).allowed)
        {
            return Err(FnoxError::WritableConfig { file: file.clone() });
        }
        let keychains = canonical_target(&home.join("Library/Keychains"));
        if policy.keychain || policy.decide(&keychains, Access::Read).allowed {
            return Err(FnoxError::Keychain);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FnoxError {
    #[error("can't run fnox: {0}")]
    Spawn(std::io::Error),
    #[error("fnox's configuration for {workspace} is invalid: {message}")]
    Config { workspace: PathBuf, message: String },
    #[error(
        "fnox loads {file}, which this sandbox lets the agent change (and so which secrets go where); keep it out of the \
         workspace and the sandbox's [sandbox] write paths"
    )]
    WritableConfig { file: PathBuf },
    #[error(
        "the sandbox can read your keychain, where fnox's secret-source credentials live: remove ~/Library/Keychains \
         (and any folder containing it) from [sandbox] read in your configuration (`piton config check` lists the \
         files)"
    )]
    Keychain,
}

/// Whether `workspace`'s launches go behind the proxy: `None` when no `[proxy]` applies there. `env` is the daemon's
/// base environment, so a workspace's environment can't change which config fnox reads.
pub async fn prepare(program: &Program, env: &Env, workspace: &Path) -> Result<Option<Proxy>, FnoxError> {
    let run = |args: &'static [&'static str]| {
        Command::new(&program.program)
            .args(&program.args)
            .args(args)
            .current_dir(workspace)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .output()
    };
    let rules = run(&["proxy", "rules"]).await.map_err(FnoxError::Spawn)?;
    if !rules.status.success() {
        let message = strip_ansi(&String::from_utf8_lossy(&rules.stderr)).trim().to_string();
        if message.contains(NO_PROXY_TABLE) {
            return Ok(None);
        }
        return Err(FnoxError::Config {
            workspace: workspace.to_path_buf(),
            message,
        });
    }
    let listed = run(&["config-files"]).await.map_err(FnoxError::Spawn)?;
    if !listed.status.success() {
        return Err(FnoxError::Config {
            workspace: workspace.to_path_buf(),
            message: strip_ansi(&String::from_utf8_lossy(&listed.stderr)).trim().to_string(),
        });
    }
    let files = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    Ok(Some(Proxy {
        program: program.clone(),
        files,
        env: env.clone(),
    }))
}

/// pi's environment behind the proxy: `FNOX_*` variables only as the daemon's base environment has them, so a
/// repository's `mise.toml` can't pick fnox's profile or config.
pub fn strip_workspace_variables(env: &mut Env, base: &Env) {
    env.retain(|key, _| !key.starts_with("FNOX_"));
    env.extend(
        base.iter()
            .filter(|(key, _)| key.starts_with("FNOX_"))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
}

/// A secret a proxy rule names that fnox couldn't resolve when it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub secret: String,
    /// fnox's reason, when it printed one.
    pub reason: Option<String>,
}

/// fnox's startup warnings, read from the session's stderr line by line. Reasons come before the line naming the
/// secret as unset:
///
/// ```text
/// WARN fnox_core::secret_resolver: Error resolving secret 'BAD': Provider 'nope' not configured in profile 'default'
/// Warning: Secret 'MISSING' not found and no default provided
/// WARN fnox::commands::proxy: fnox proxy: 'MISSING' is not set; requests that need it are refused until it is (...)
/// ```
#[derive(Debug, Default)]
pub struct StartupLog {
    reasons: Vec<(String, String)>,
    pub unresolved: Vec<Unresolved>,
}

impl StartupLog {
    /// Reads one stderr line; true when it named another unresolved secret.
    pub fn read(&mut self, line: &str) -> bool {
        let line = strip_ansi(line);
        if let Some((secret, reason)) = after(&line, "Error resolving secret '").and_then(|r| r.split_once("': ")) {
            self.reasons.push((secret.to_string(), reason.trim().to_string()));
            return false;
        }
        if let Some((secret, reason)) = after(&line, "Warning: Secret '").and_then(|r| r.split_once("' ")) {
            self.reasons.push((secret.to_string(), reason.trim().to_string()));
            return false;
        }
        let Some(secret) = after(&line, "fnox proxy: '")
            .and_then(|r| r.split_once("' is not set;"))
            .map(|(secret, _)| secret.to_string())
        else {
            return false;
        };
        if self.unresolved.iter().any(|u| u.secret == secret) {
            return false;
        }
        let reason = self
            .reasons
            .iter()
            .rev()
            .find(|(name, _)| *name == secret)
            .map(|(_, reason)| shorten(reason));
        self.unresolved.push(Unresolved { secret, reason });
        true
    }

    /// The conversation's notice: which secrets are missing and what that means.
    pub fn notice(&self) -> Option<String> {
        if self.unresolved.is_empty() {
            return None;
        }
        let list = self
            .unresolved
            .iter()
            .map(|u| match &u.reason {
                Some(reason) => format!("{} ({reason})", u.secret),
                None => u.secret.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "fnox couldn't read these secrets when this session started: {list}. Requests that need them are refused \
             until fnox can read them."
        ))
    }

    /// The status line entry: short, for an editor's status area.
    pub fn status(&self) -> Option<String> {
        match self.unresolved.len() {
            0 => None,
            1 => Some("fnox: 1 secret unavailable".into()),
            n => Some(format!("fnox: {n} secrets unavailable")),
        }
    }
}

fn after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|at| &line[at + marker.len()..])
}

fn shorten(reason: &str) -> String {
    const MAX: usize = 200;
    match reason.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &reason[..at]),
        None => reason.to_string(),
    }
}

/// Text without terminal color codes: fnox colors its log lines even when stderr isn't a terminal.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::sandbox::{Rule, Source, Target};

    /// fnox 1.48's startup output with a missing secret and a provider error, colors included.
    #[test]
    fn reads_fnox_startup_warnings() {
        let mut log = StartupLog::default();
        let lines = [
            "\u{1b}[2m2026-11-18T18:45:38.441779Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m \u{1b}[2mfnox_core::secret_resolver\u{1b}[0m\u{1b}[2m:\u{1b}[0m Error resolving secret 'BAD': Provider 'nope' not configured in profile 'default'",
            "  (provider not defined in any config file)",
            "Warning: Secret 'MISSING_ONE' not found and no default provided",
            "\u{1b}[2m2026-11-18T18:45:38.442312Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m \u{1b}[2mfnox::commands::proxy\u{1b}[0m\u{1b}[2m:\u{1b}[0m fnox proxy: 'MISSING_ONE' is not set; requests that need it are refused until it is (for example, run `fnox set MISSING_ONE`)",
            "2026-11-18T18:45:38.442321Z  WARN fnox::commands::proxy: fnox proxy: 'BAD' is not set; requests that need it are refused until it is (for example, run `fnox set BAD`)",
            "2026-11-18T18:45:38.550131Z  WARN fnox::commands::proxy: fnox proxy is not sandboxing the command; same-user code may bypass the proxy (see --sandbox)",
            "proxied request method=POST domain=\"api.example.com\" path=/ injected=[]",
        ];
        let named: Vec<bool> = lines.iter().map(|l| log.read(l)).collect();
        assert_eq!(named, [false, false, false, true, true, false, false]);
        assert_eq!(
            log.unresolved,
            [
                Unresolved {
                    secret: "MISSING_ONE".into(),
                    reason: Some("not found and no default provided".into()),
                },
                Unresolved {
                    secret: "BAD".into(),
                    reason: Some("Provider 'nope' not configured in profile 'default'".into()),
                },
            ]
        );
        assert!(!log.read(lines[4]), "a repeated warning isn't new");
        assert_eq!(log.status().as_deref(), Some("fnox: 2 secrets unavailable"));
        let notice = log.notice().unwrap();
        assert!(
            notice.contains("MISSING_ONE (not found and no default provided); BAD (Provider"),
            "{notice}"
        );
        assert_eq!(StartupLog::default().notice(), None);
    }

    #[test]
    fn wraps_the_command() {
        let proxy = Proxy {
            program: Program {
                program: "/bin/fnox".into(),
                args: vec!["-P".into(), "agent".into()],
            },
            files: vec![],
            env: Env::new(),
        };
        assert_eq!(
            proxy.wrap(vec!["pi".into(), "--mode".into(), "rpc".into()]),
            [
                "/bin/fnox",
                "-P",
                "agent",
                "--non-interactive",
                "proxy",
                "run",
                "--no-sandbox",
                "--",
                "pi",
                "--mode",
                "rpc"
            ]
        );
    }

    #[test]
    fn workspace_fnox_variables_are_replaced_by_the_base_ones() {
        let base: Env = [("FNOX_PROFILE".to_string(), "agent".to_string())].into();
        let mut env: Env = [
            ("FNOX_PROFILE".to_string(), "repo".to_string()),
            ("FNOX_CONFIG".to_string(), "/repo/fnox.toml".to_string()),
            ("PATH".to_string(), "/bin".to_string()),
        ]
        .into();
        strip_workspace_variables(&mut env, &base);
        assert_eq!(
            env,
            [
                ("FNOX_PROFILE".to_string(), "agent".to_string()),
                ("PATH".to_string(), "/bin".to_string())
            ]
            .into()
        );
    }

    fn policy(rules: Vec<Rule>, keychain: bool) -> Policy {
        Policy {
            rules,
            keychain,
            network: Vec::new(),
        }
    }

    fn allow(access: Access, path: &str) -> Rule {
        Rule {
            effect: crate::launch::sandbox::Effect::Allow,
            access: vec![access],
            target: Target::Subpath { path: path.into() },
            source: Source::Workspace,
        }
    }

    #[test]
    fn refuses_a_sandbox_that_can_change_fnox_config_or_read_the_keychain() {
        let proxy = Proxy {
            program: Program {
                program: "fnox".into(),
                args: vec![],
            },
            files: vec!["/w/shared/secrets.toml".into(), "/h/.config/fnox/config.toml".into()],
            env: Env::new(),
        };
        let home = Path::new("/h");
        // Reads are open by default; deny the keychain as the built-in policy does.
        let closed_keychain = Rule {
            effect: crate::launch::sandbox::Effect::Deny,
            access: vec![Access::Read],
            target: Target::Subpath {
                path: "/h/Library/Keychains".into(),
            },
            source: Source::Workspace,
        };
        let ok = policy(vec![allow(Access::Write, "/w/src"), closed_keychain.clone()], false);
        assert!(proxy.check_sandbox(&ok, home).is_ok());
        let writable = policy(vec![allow(Access::Write, "/w"), closed_keychain.clone()], false);
        assert!(matches!(
            proxy.check_sandbox(&writable, home),
            Err(FnoxError::WritableConfig { file }) if file == Path::new("/w/shared/secrets.toml")
        ));
        let exposed = policy(
            vec![closed_keychain.clone(), allow(Access::Read, "/h/Library/Keychains")],
            false,
        );
        assert!(matches!(proxy.check_sandbox(&exposed, home), Err(FnoxError::Keychain)));
        let services = policy(vec![closed_keychain], true);
        assert!(matches!(proxy.check_sandbox(&services, home), Err(FnoxError::Keychain)));
    }
}
