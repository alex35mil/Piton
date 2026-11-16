//! The bridge between a sandboxed agent and the daemon: what the agent may not change on its own, the one channel it
//! has to ask anyway, and how that request is answered.
//!
//! - [`protected`]: which files are protected, and the Seatbelt rules every sandboxed launch carries for them.
//! - [`socket`]: `agent.sock`, the socket sandboxed pis reach the daemon on, and their session tokens.
//! - [`write`]: a protected write an agent asks for: the user's dialog, and the daemon's write once approved.

mod protected;
mod socket;
mod write;

pub use self::{
    protected::{ProtectedRoots, canonical_target},
    socket::{FILE, SOCKET_ENV, TOKEN_ENV, Tokens, bind, serve},
    write::{WriteDecision, WriteOutcome, WriteRequest, write_approved, write_dialog},
};
