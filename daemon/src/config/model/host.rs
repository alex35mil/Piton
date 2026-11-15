//! `config.host.toml` and `config.host.local.toml` in the global tier: how the daemon runs on this host, and this
//! host's preferences the apps change. `config.host.local.toml` wins per key. The pi package reads `[development]` on
//! whichever machine pi runs.

use serde::{Deserialize, Serialize};

use super::tables::nearer;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostFile {
    /// The daemon's socket: a test and development override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// Where the apps may browse and start sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorites: Option<Vec<String>>,
    /// Host logging default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<crate::diagnostics::Mode>,
    /// Launch mode for `piton attach` when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_attach_mode: Option<crate::wire::LaunchMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_grace_ms: Option<u64>,
    /// Hold the Mac awake while sessions are live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_awake: Option<bool>,
    #[serde(default, skip_serializing_if = "PiTable::is_empty")]
    pub pi: PiTable,
    #[serde(default, skip_serializing_if = "SandboxProgram::is_empty")]
    pub sandbox: SandboxProgram,
    #[serde(default, skip_serializing_if = "EnvTable::is_empty")]
    pub env: EnvTable,
    #[serde(default, skip_serializing_if = "StopTable::is_empty")]
    pub stop: StopTable,
    #[serde(default, skip_serializing_if = "TailnetTable::is_empty")]
    pub tailnet: TailnetTable,
    #[serde(default, skip_serializing_if = "DevelopmentTable::is_empty")]
    pub development: DevelopmentTable,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PiTable {
    /// pi executable and leading arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

/// `[sandbox]` here is only the sandbox program: an option of how the daemon runs. Sandbox policy (grants, the base
/// profile) is in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProgram {
    /// The sandbox executable and leading arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_shell: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mise: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StopTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_grace_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TailnetTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_ms: Option<u64>,
}

/// Switches that apply only to a development install (the pi package running from a git checkout of piton).
/// Documented in docs/development.md.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DevelopmentTable {
    /// A shipped Deliverable ends with an analysis of the session for workflow problems. Absent: on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_analysis: Option<bool>,
}

macro_rules! is_empty {
    ($($ty:ty),*) => {$(
        impl $ty {
            pub fn is_empty(&self) -> bool {
                *self == Self::default()
            }
        }
    )*};
}
is_empty!(
    PiTable,
    SandboxProgram,
    EnvTable,
    StopTable,
    TailnetTable,
    DevelopmentTable
);

impl HostFile {
    /// Values the schema accepts but piton can't use.
    pub fn check(&self) -> Result<(), String> {
        if self.pi.command.as_ref().is_some_and(Vec::is_empty) {
            return Err("pi.command must not be empty".into());
        }
        if self.sandbox.command.as_ref().is_some_and(Vec::is_empty) {
            return Err("sandbox.command must not be empty".into());
        }
        Ok(())
    }

    /// Folds the local file in: per key, its value wins.
    pub fn merge(&mut self, n: Self) {
        nearer(&mut self.socket, n.socket);
        nearer(&mut self.log, n.log);
        nearer(&mut self.roots, n.roots);
        nearer(&mut self.favorites, n.favorites);
        nearer(&mut self.logging, n.logging);
        nearer(&mut self.default_attach_mode, n.default_attach_mode);
        nearer(&mut self.empty_grace_ms, n.empty_grace_ms);
        nearer(&mut self.keep_awake, n.keep_awake);
        nearer(&mut self.pi.command, n.pi.command);
        nearer(&mut self.pi.args, n.pi.args);
        nearer(&mut self.sandbox.command, n.sandbox.command);
        nearer(&mut self.env.login_shell, n.env.login_shell);
        nearer(&mut self.env.mise, n.env.mise);
        nearer(&mut self.stop.grace_ms, n.stop.grace_ms);
        nearer(&mut self.stop.term_grace_ms, n.stop.term_grace_ms);
        nearer(&mut self.tailnet.enabled, n.tailnet.enabled);
        nearer(&mut self.tailnet.port, n.tailnet.port);
        nearer(&mut self.tailnet.cli, n.tailnet.cli);
        nearer(&mut self.tailnet.heartbeat_ms, n.tailnet.heartbeat_ms);
        nearer(&mut self.tailnet.retry_ms, n.tailnet.retry_ms);
        nearer(&mut self.development.session_analysis, n.development.session_analysis);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_file_wins_per_key() {
        let mut base: HostFile =
            toml::from_str("roots = [\"~/a\"]\n[stop]\ngrace_ms = 1\nterm_grace_ms = 2\n").unwrap();
        base.merge(toml::from_str("keep_awake = false\n[stop]\ngrace_ms = 9\n").unwrap());
        assert_eq!((base.stop.grace_ms, base.stop.term_grace_ms), (Some(9), Some(2)));
        assert_eq!(base.roots.as_deref(), Some(&["~/a".to_string()][..]));
        assert_eq!(base.keep_awake, Some(false));
    }

    #[test]
    fn empty_commands_are_refused() {
        let file: HostFile = toml::from_str("[pi]\ncommand = []\n").unwrap();
        assert!(file.check().is_err());
        let file: HostFile = toml::from_str("[sandbox]\ncommand = []\n").unwrap();
        assert!(file.check().is_err());
        // Sandbox policy isn't here.
        assert!(toml::from_str::<HostFile>("[sandbox]\nbase = \"x\"\n").is_err());
    }
}
