//! Messages between the daemon and its local clients (`piton attach`, the CLI) over the Unix socket.
//!
//! Every connection starts with one [`Hello`] line. After an `rpc` hello the client sends raw pi RPC lines and the
//! daemon sends tagged frames: `r<pi record>` or `c<AttachControl>`, one per line, so control messages never mix with
//! pi records on `piton attach`'s stdout. After a `control` hello the client sends one [`ControlRequest`] line and gets
//! one [`ControlResponse`] line.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{launch::LaunchMode, session::SessionInfo};

pub const PROTOCOL_VERSION: u32 = 1;

pub const FRAME_RECORD: u8 = b'r';
pub const FRAME_CONTROL: u8 = b'c';

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum Hello {
    /// Raw pi RPC for one session; a new pi is launched in `workspace`.
    Rpc {
        version: u32,
        workspace: PathBuf,
        #[serde(default)]
        client: Option<String>,
    },
    /// One control request and its response.
    Control { version: u32 },
    /// The native clients' protocol: requests with ids, replies, and pushed session-list and session-view updates.
    /// See [`ViewRequest`].
    View {
        version: u32,
        #[serde(default)]
        client: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachControl {
    /// The connection is attached to a freshly launched pi.
    Welcome { session: u64 },
    /// The connection couldn't be set up; the daemon closes it.
    Error { message: String },
    /// The connection ends; the daemon closes it.
    End {
        reason: EndReason,
        #[serde(default)]
        stderr: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The session's pi ended (terminated or crashed). `piton attach` exits 3.
    PiExited,
    /// The daemon is shutting down. `piton attach` exits 1, so clients reconnect later.
    DaemonShutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    List,
    Start {
        workspace: PathBuf,
        mode: LaunchMode,
        #[serde(default)]
        prompt: Option<String>,
    },
    Stop {
        /// pi session id, session file, or piton session key.
        session: String,
    },
    WorkerCreate(crate::project::CreateRequest),
    WorkerList {
        /// The source checkout or any of its workers.
        from: PathBuf,
    },
    WorkerDelete {
        path: PathBuf,
        /// Delete despite uncommitted changes or unpushed commits.
        #[serde(default)]
        force: bool,
    },
    /// The host logging policy in effect (`piton logging status`, the collector).
    Logging,
    /// Switch host logging until the policy expires or the daemon restarts.
    SetLogging {
        policy: crate::logging::Policy,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Started {
        session: SessionInfo,
    },
    Stopped {
        forced: bool,
    },
    Worker {
        worker: crate::project::WorkerInfo,
    },
    Workers {
        workers: Vec<WorkerEntry>,
    },
    WorkerDeleted {
        /// Sessions that were running in the worker and got stopped.
        stopped: usize,
        /// Set when the source couldn't be fast-forwarded afterwards.
        warning: Option<String>,
    },
    /// A worker operation was refused for a reason the caller can act on.
    WorkerRefused {
        refusal: crate::project::WorkerRefusal,
    },
    Logging {
        logging: crate::logging::State,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEntry {
    #[serde(flatten)]
    pub worker: crate::project::WorkerInfo,
    /// Live sessions in the worker.
    pub sessions: Vec<SessionInfo>,
}
