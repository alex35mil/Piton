//! Where a pi runs: inside the sandbox, or directly on the host.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    /// pi inside nono.
    Sandbox,
    /// pi directly on the host.
    Host,
}
