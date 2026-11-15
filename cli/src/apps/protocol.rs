//! The native apps' `view` protocol: the requests a connection takes and the daemon-wide state it pushes. Carried over
//! the Unix socket (after a `view` hello) or the tailnet WebSocket.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch::LaunchMode;

/// A request on a `view` connection. Replies echo `id`: `{"type":"reply","id":..,"ok":true,"data":..}` or
/// `{"type":"reply","id":..,"ok":false,"error":..}`. Pushed lines have no id: `sessions` (the session list, after
/// `sessions.subscribe` and on every change), `snapshot`, `change`, and `history` (see crate::session's view).
#[derive(Debug, Deserialize)]
pub struct ViewEnvelope {
    pub id: serde_json::Value,
    #[serde(flatten)]
    pub request: ViewRequest,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ViewRequest {
    #[serde(rename = "sessions.list")]
    SessionsList,
    /// Daemon-wide state (keep-awake, settings). Also pushed as `daemon` lines after `sessions.subscribe`, on change.
    #[serde(rename = "daemon.state")]
    DaemonState,
    /// Change one setting: `key` (as written in its file) in the global file of `scope`. `value: null` returns it to
    /// the default. `local` moves the key to the scope's `.local` file (`true`) or the shared one (`false`); omitted, it
    /// stays in the file that sets it. A scope without a local file (`client`) refuses `local`.
    #[serde(rename = "settings.set")]
    SettingsSet {
        scope: crate::config::Scope,
        key: String,
        value: serde_json::Value,
        #[serde(default)]
        local: Option<bool>,
    },
    /// Switch host logging until the policy expires or the daemon restarts (`logging` in `config.host.toml` is the
    /// default).
    #[serde(rename = "daemon.logging")]
    DaemonLogging { policy: crate::logging::Policy },
    /// Push the session list now and whenever it changes.
    #[serde(rename = "sessions.subscribe")]
    SessionsSubscribe,
    /// The direct children of `path` under the configured roots (the roots when omitted): groups and checkouts.
    #[serde(rename = "workspaces.browse")]
    WorkspacesBrowse {
        #[serde(default)]
        path: Option<PathBuf>,
    },
    /// Start a session in a checkout under the configured roots.
    #[serde(rename = "session.start")]
    SessionStart {
        workspace: PathBuf,
        #[serde(default)]
        mode: Option<LaunchMode>,
        #[serde(default)]
        prompt: Option<String>,
    },
    #[serde(rename = "session.stop")]
    SessionStop { session: u64 },
    /// Watch one session: a snapshot (or the changes since `from`), then every change. One watch per session per
    /// connection; watching again replaces it.
    #[serde(rename = "session.watch")]
    SessionWatch {
        session: u64,
        #[serde(default)]
        from: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "session.unwatch")]
    SessionUnwatch { session: u64 },
    #[serde(rename = "session.history")]
    SessionHistory {
        session: u64,
        before: u64,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "session.tool_output")]
    SessionToolOutput { session: u64, call_id: String },
    /// One image's data, by the id in its reference block (items and tool outputs never carry image data).
    #[serde(rename = "session.image")]
    SessionImage { session: u64, image: u64 },
    /// Send a message. `message_id` is the client's id for it: a send repeated with the same id (a retry after the
    /// reply was lost) reaches pi once and gets the first one's reply.
    #[serde(rename = "session.send")]
    SessionSend {
        session: u64,
        message_id: String,
        text: String,
        #[serde(default)]
        mode: SendModeWire,
    },
    #[serde(rename = "session.abort")]
    SessionAbort { session: u64 },
    /// Answer a pending dialog with pi's `extension_ui_response` fields: `value`, `confirmed`, or `cancelled`.
    #[serde(rename = "session.answer")]
    SessionAnswer {
        session: u64,
        request_id: String,
        answer: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendModeWire {
    /// A prompt when idle; a follow-up when the agent is working.
    #[default]
    Prompt,
    Steer,
    FollowUp,
}

/// Daemon-wide state for the apps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonState {
    /// `<package version> (<git commit>[-dirty])` of the running daemon.
    pub version: String,
    /// This host's identity, stable across restarts and kept by `piton state export|import`.
    pub host_id: String,
    /// Keep-awake is enabled (`keep_awake` in the host file): the Mac is held awake while sessions are live.
    pub keep_awake: bool,
    /// The assertion is held right now (enabled and at least one live session).
    pub awake: bool,
    pub sessions: usize,
    /// Host logging (this Mac's app, the daemon, and the phone-log collector).
    pub logging: crate::logging::State,
    /// Every setting the apps show: its value, where it lives, and whether it waits for a restart.
    pub settings: Vec<crate::apps::SettingInfo>,
    /// Why the global files' latest version can't be used (the settings are the last good values).
    pub settings_error: Option<String>,
}
