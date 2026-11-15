//! A live session as the daemon reports it: to the CLI (`piton ls`), to `piton attach`, and to the apps.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch::LaunchMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// piton's key for the pi process; stable while it runs.
    pub key: u64,
    /// pi's current session id and file (they change on `new_session` / `switch_session`).
    pub id: Option<String>,
    pub file: Option<String>,
    pub workspace: PathBuf,
    /// What `workspace` is: a project's own checkout, or a worker of one. The apps group sessions by project.
    pub checkout: Checkout,
    pub mode: LaunchMode,
    pub status: SessionStatus,
    pub clients: usize,
    pub pid: Option<u32>,
    /// The session name, or the first line of the first user message.
    pub title: Option<String>,
}

/// A session's workspace relative to its project (see crate::project): decided from the worker's git config and folder
/// layout, so a repository whose own name contains `--` isn't mistaken for a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Checkout {
    /// The project's own checkout: the workspace is the project.
    Source,
    /// A worker `<repo>--<name>` of the project at `source`.
    Worker { source: PathBuf, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Working,
    NeedsInput,
    Stopping,
}
