//! How the daemon runs: `socket`, `log`, `[pi]`, `[env]`, `[stop]`, `[tailnet]` at the top of the global file. Global
//! tier only.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, nearer, present};

/// The top-level keys of this part.
pub(super) const KEYS: &[&str] = &["socket", "log", "pi", "env", "stop", "tailnet"];

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(default, skip_serializing_if = "PiTable::is_empty")]
    pub pi: PiTable,
    #[serde(default, skip_serializing_if = "EnvTable::is_empty")]
    pub env: EnvTable,
    #[serde(default, skip_serializing_if = "StopTable::is_empty")]
    pub stop: StopTable,
    #[serde(default, skip_serializing_if = "TailnetTable::is_empty")]
    pub tailnet: TailnetTable,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PiTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
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

macro_rules! is_empty {
    ($($ty:ty),*) => {$(
        impl $ty {
            pub fn is_empty(&self) -> bool {
                *self == Self::default()
            }
        }
    )*};
}
is_empty!(PiTable, EnvTable, StopTable, TailnetTable);

impl TableValue for RunTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[
            ("socket", self.socket.is_some()),
            ("log", self.log.is_some()),
            ("pi", !self.pi.is_empty()),
            ("env", !self.env.is_empty()),
            ("stop", !self.stop.is_empty()),
            ("tailnet", !self.tailnet.is_empty()),
        ])
    }

    fn check(&self) -> Result<(), String> {
        match &self.pi.command {
            Some(command) if command.is_empty() => Err("pi.command must not be empty".into()),
            Some(_) | None => Ok(()),
        }
    }
}

impl Merge for RunTable {
    /// Per key: the nearer value.
    fn merge(&mut self, n: Self) {
        nearer(&mut self.socket, n.socket);
        nearer(&mut self.log, n.log);
        nearer(&mut self.pi.command, n.pi.command);
        nearer(&mut self.pi.args, n.pi.args);
        nearer(&mut self.env.login_shell, n.env.login_shell);
        nearer(&mut self.env.mise, n.env.mise);
        nearer(&mut self.stop.grace_ms, n.stop.grace_ms);
        nearer(&mut self.stop.term_grace_ms, n.stop.term_grace_ms);
        nearer(&mut self.tailnet.enabled, n.tailnet.enabled);
        nearer(&mut self.tailnet.port, n.tailnet.port);
        nearer(&mut self.tailnet.cli, n.tailnet.cli);
        nearer(&mut self.tailnet.heartbeat_ms, n.tailnet.heartbeat_ms);
        nearer(&mut self.tailnet.retry_ms, n.tailnet.retry_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_take_the_nearer_value() {
        let mut base: RunTable = toml::from_str("[stop]\ngrace_ms = 1\nterm_grace_ms = 2\n").unwrap();
        base.merge(toml::from_str("[stop]\ngrace_ms = 9\n").unwrap());
        assert_eq!((base.stop.grace_ms, base.stop.term_grace_ms), (Some(9), Some(2)));
    }

    #[test]
    fn an_empty_pi_command_is_refused() {
        let run = RunTable {
            pi: PiTable {
                command: Some(Vec::new()),
                args: None,
            },
            ..RunTable::default()
        };
        assert!(run.check().is_err());
    }
}
