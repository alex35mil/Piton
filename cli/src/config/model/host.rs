//! `config.host.toml` and `config.host.local.toml` in the global tier: how the daemon runs on this host, and this
//! host's preferences the apps change. `config.host.local.toml` wins per key. The pi package reads `[debugging]` on
//! whichever machine pi runs.

use serde::{Deserialize, Serialize};

use super::{
    duration::{ConfigDuration, Timeout},
    tables::nearer,
};

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
    /// Launch mode for `piton attach` when none is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_attach_mode: Option<crate::launch::LaunchMode>,
    /// Stop a pi with no messages, no run, and no pending dialog this long after its last client leaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_empty_session_after: Option<ConfigDuration>,
    /// How long an approval (a blocked command, a held connection) waits for an answer before the action doesn't happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout: Option<Timeout>,
    /// Hold the Mac awake while sessions are live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_awake: Option<bool>,
    #[serde(default, skip_serializing_if = "PiTable::is_empty")]
    pub pi: PiTable,
    #[serde(default, skip_serializing_if = "EnvTable::is_empty")]
    pub env: EnvTable,
    #[serde(default, skip_serializing_if = "StopTable::is_empty")]
    pub stop: StopTable,
    /// The external programs piton runs, each used only when enabled and available (`crate::integrations`).
    #[serde(default, skip_serializing_if = "IntegrationsTable::is_empty")]
    pub integrations: IntegrationsTable,
    #[serde(default, skip_serializing_if = "DebuggingTable::is_empty")]
    pub debugging: DebuggingTable,
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

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_shell: Option<bool>,
}

/// How a session's pi is stopped.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StopTable {
    /// After closing pi's stdin, how long before SIGTERM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate_after: Option<ConfigDuration>,
    /// After SIGTERM, how long before SIGKILL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_after: Option<ConfigDuration>,
}

/// `[integrations.<name>]` for each external program.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationsTable {
    #[serde(default, skip_serializing_if = "IntegrationTable::is_empty")]
    pub mise: IntegrationTable,
    #[serde(default, skip_serializing_if = "IntegrationTable::is_empty")]
    pub fnox: IntegrationTable,
    #[serde(default, skip_serializing_if = "TailscaleTable::is_empty")]
    pub tailscale: TailscaleTable,
}

/// One integration: whether to use it (unset: when it's available) and how to run it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrationTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The executable and leading arguments, found through the login shell's PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

impl IntegrationTable {
    fn merge(&mut self, n: Self) {
        nearer(&mut self.enabled, n.enabled);
        nearer(&mut self.command, n.command);
    }
}

/// `[integrations.tailscale]`: the integration, and how the apps are served on the tailnet while it's used.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TailscaleTable {
    /// Whether Tailscale is used, and so whether the apps are served on the tailnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// How often to ping each app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<ConfigDuration>,
    /// While Tailscale isn't up, how often to try again to listen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<ConfigDuration>,
}

impl TailscaleTable {
    /// The keys every integration has.
    pub fn integration(&self) -> IntegrationTable {
        IntegrationTable {
            enabled: self.enabled,
            command: self.command.clone(),
        }
    }

    fn merge(&mut self, n: Self) {
        nearer(&mut self.enabled, n.enabled);
        nearer(&mut self.command, n.command);
        nearer(&mut self.port, n.port);
        nearer(&mut self.heartbeat, n.heartbeat);
        nearer(&mut self.retry, n.retry);
    }
}

/// Logging, and switches for a development install (the pi package running from a git checkout of piton), the
/// latter documented in docs/development.md.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DebuggingTable {
    /// Host logging's default level; `piton logging` and the apps switch it until it expires or the daemon restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<crate::logging::Mode>,
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
    EnvTable,
    StopTable,
    IntegrationsTable,
    IntegrationTable,
    TailscaleTable,
    DebuggingTable
);

impl HostFile {
    /// Values the schema accepts but piton can't use.
    pub fn check(&self) -> Result<(), String> {
        if self.pi.command.as_ref().is_some_and(Vec::is_empty) {
            return Err("pi.command must not be empty".into());
        }
        for (name, integration) in [
            ("mise", &self.integrations.mise),
            ("fnox", &self.integrations.fnox),
            ("tailscale", &self.integrations.tailscale.integration()),
        ] {
            if integration.command.as_ref().is_some_and(Vec::is_empty) {
                return Err(format!("integrations.{name}.command must not be empty"));
            }
        }
        Ok(())
    }

    /// Folds the local file in: per key, its value wins.
    pub fn merge(&mut self, n: Self) {
        nearer(&mut self.socket, n.socket);
        nearer(&mut self.log, n.log);
        nearer(&mut self.roots, n.roots);
        nearer(&mut self.favorites, n.favorites);
        nearer(&mut self.default_attach_mode, n.default_attach_mode);
        nearer(&mut self.stop_empty_session_after, n.stop_empty_session_after);
        nearer(&mut self.approval_timeout, n.approval_timeout);
        nearer(&mut self.keep_awake, n.keep_awake);
        nearer(&mut self.pi.command, n.pi.command);
        nearer(&mut self.pi.args, n.pi.args);
        nearer(&mut self.env.login_shell, n.env.login_shell);
        nearer(&mut self.stop.terminate_after, n.stop.terminate_after);
        nearer(&mut self.stop.kill_after, n.stop.kill_after);
        self.integrations.mise.merge(n.integrations.mise);
        self.integrations.fnox.merge(n.integrations.fnox);
        self.integrations.tailscale.merge(n.integrations.tailscale);
        nearer(&mut self.debugging.log_level, n.debugging.log_level);
        nearer(&mut self.debugging.session_analysis, n.debugging.session_analysis);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_file_wins_per_key() {
        let mut base: HostFile =
            toml::from_str("roots = [\"~/a\"]\n[stop]\nterminate_after = \"1s\"\nkill_after = \"2s\"\n").unwrap();
        base.merge(toml::from_str("keep_awake = false\n[stop]\nterminate_after = \"9s\"\n").unwrap());
        assert_eq!(
            (base.stop.terminate_after, base.stop.kill_after),
            (
                Some(ConfigDuration::from_millis(9_000)),
                Some(ConfigDuration::from_millis(2_000))
            )
        );
        assert_eq!(base.roots.as_deref(), Some(&["~/a".to_string()][..]));
        assert_eq!(base.keep_awake, Some(false));
    }

    #[test]
    fn empty_commands_are_refused() {
        let file: HostFile = toml::from_str("[pi]\ncommand = []\n").unwrap();
        assert!(file.check().is_err());
        let file: HostFile = toml::from_str("[integrations.mise]\ncommand = []\n").unwrap();
        assert!(file.check().unwrap_err().contains("integrations.mise.command"));
        let file: HostFile = toml::from_str("[integrations.tailscale]\ncommand = []\n").unwrap();
        assert!(file.check().unwrap_err().contains("integrations.tailscale.command"));
    }

    #[test]
    fn integrations_merge_per_key() {
        let mut base: HostFile = toml::from_str(
            "[integrations.mise]\nenabled = true\ncommand = [\"/a/mise\"]\n[integrations.tailscale]\nport = 1\nretry = \"1s\"\n",
        )
        .unwrap();
        base.merge(
            toml::from_str("[integrations.mise]\nenabled = false\n[integrations.tailscale]\nport = 2\n").unwrap(),
        );
        assert_eq!(base.integrations.mise.enabled, Some(false));
        assert_eq!(base.integrations.mise.command, Some(vec!["/a/mise".to_string()]));
        assert_eq!(base.integrations.tailscale.port, Some(2));
        assert_eq!(
            base.integrations.tailscale.retry,
            Some(ConfigDuration::from_millis(1_000))
        );
    }

    /// The keys this file had before are plain unknown keys.
    #[test]
    fn earlier_keys_are_unknown() {
        for text in [
            "[env]\nmise = false\n",
            "[tailnet]\nport = 1\n",
            "[tools.mise]\nenabled = true\n",
            "[development]\nsession_analysis = false\n",
            "logging = \"debug\"\n",
            "empty_grace_ms = 1\n",
            "confirmation_timeout_ms = 1\n",
            "[stop]\ngrace_ms = 1\n",
            "[integrations.tailscale]\nheartbeat_ms = 1\n",
            "[integrations.mise]\nport = 1\n",
        ] {
            assert!(toml::from_str::<HostFile>(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn durations_are_strings_and_only_the_question_may_never_time_out() {
        let file: HostFile =
            toml::from_str("approval_timeout = \"never\"\nstop_empty_session_after = \"30s\"\n").unwrap();
        assert_eq!(file.approval_timeout, Some(Timeout::Never));
        assert!(toml::from_str::<HostFile>("stop_empty_session_after = \"never\"\n").is_err());
        assert!(toml::from_str::<HostFile>("approval_timeout = 600000\n").is_err());
        assert!(toml::from_str::<HostFile>("approval_timeout = \"0s\"\n").is_err());
        assert!(toml::from_str::<HostFile>("[integrations.tailscale]\nretry = \"never\"\n").is_err());
    }
}
