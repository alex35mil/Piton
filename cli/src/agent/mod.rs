//! The bridge between a sandboxed agent and the daemon: what the agent may not change on its own, the one channel it
//! has to ask anyway, and how that request is answered.
//!
//! - [`command`]: which per-command entry a bash call gets, matched as the permissions extension matches.
//! - [`question`]: the block question for a command, typed, rendered per client protocol.
//! - [`run`]: where an approved command runs (a sandbox or the host), and running it.
//! - [`protected`]: which files are protected, and the Seatbelt rules every sandboxed launch carries for them.
//! - [`git`]: Git's hooks and configuration, which a sandboxed agent may not write.
//! - [`channel`]: the channel's contract: callers, their tokens and processes, requests, and who may send which.
//! - [`socket`]: `agent.sock`, the transport: identifies a request's caller and dispatches it.
//! - [`client`]: the Rust client of the channel, for `piton` running inside a sandbox.
//! - [`answer`]: the answers every approval-gated request shares.
//! - [`write`]: a protected write an agent asks for: the user's dialog, and the daemon's write once approved.

pub mod answer;
pub mod block;
mod channel;
pub mod client;
mod command;
mod git;
pub mod open;
mod protected;
pub mod question;
mod run;
mod socket;
mod write;

#[cfg(test)]
pub use self::run::{Ran, RanBy, RanRuntime};
pub use self::{
    channel::{LAUNCH_TOKEN_ENV, SOCKET_ENV, TOKEN_ENV, Tokens},
    command::{Matched, analyze, grant_for},
    git::{GitControl, GitControlEntry},
    protected::{
        Controls, PROTECTED_ENV, Protected, ProtectedFacts, ProtectedRoots, canonical_target, fixed_entries, lexical,
        link_entries,
    },
    run::{Approved, Failed, PiShell, Rerun, RunDecision, RunRequest, Runtime, Why, host_env},
    socket::{FILE, bind, random_hex, serve},
    write::{
        FileMode, WriteAnswer, WriteApproval, WriteOutcome, WriteRequest, read_answer as read_write_answer,
        remove_file, write_approved, write_dialog, write_file,
    },
};
