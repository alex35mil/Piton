//! The bridge between a sandboxed agent and the daemon: what the agent may not change on its own, the one channel it
//! has to ask anyway, and how that request is answered.
//!
//! - [`command`]: which per-command entry a bash call gets, matched as the permissions extension matches.
//! - [`question`]: the block question for a command, typed, rendered per client protocol.
//! - [`run`]: where an approved command runs (a sandbox or the host), and running it.
//! - [`protected`]: which files are protected, and the Seatbelt rules every sandboxed launch carries for them.
//! - [`git`]: Git's hooks and configuration, which a sandboxed agent may not write.
//! - [`socket`]: `agent.sock`, the socket sandboxed pis reach the daemon on, and their session tokens.
//! - [`write`]: a protected write an agent asks for: the user's dialog, and the daemon's write once approved.

pub mod block;
mod command;
mod git;
pub mod open;
mod protected;
pub mod question;
mod run;
mod socket;
mod write;

pub use self::{
    command::{Matched, analyze, grant_for},
    git::{GitControl, GitControlEntry},
    protected::{
        Controls, PROTECTED_ENV, Protected, ProtectedFacts, ProtectedRoots, canonical_target, fixed_entries, lexical,
        link_entries,
    },
    run::{
        Approved, Failed, PiShell, Ran, Rerun, RunDecision, RunEnd, RunEvent, RunRequest, RunSpec, Runtime, Why,
        host_env, run,
    },
    socket::{FILE, SOCKET_ENV, TOKEN_ENV, Tokens, bind, random_hex, serve},
    write::{
        FileMode, WriteDecision, WriteOutcome, WriteRequest, remove_file, write_approved, write_dialog, write_file,
    },
};
