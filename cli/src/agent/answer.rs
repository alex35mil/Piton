//! The answers every approval-gated request shares, and what each request kind declares about them. A kind fills in
//! what each answer carries, so its type says which it offers:
//!
//! - **Narrowest:** [`Once`] when the daemon can bound the operation (one rerun of a bash call, one write, one lift for
//!   one action), or [`UntilSettled`] when it can't (a block in pi's own process), until pi's `agent_settled` (not
//!   `agent_end`: retries, compaction, steering, and follow-ups continue after it). A kind's narrowest is one of the
//!   two, never both.
//! - **For this session:** until the session ends; nothing saved.
//! - **Always:** saved where the kind says, offered only by kinds that have a saved form.
//! - **Deny**, with what the user said about it, when the kind keeps that.
//!
//! An answer a kind doesn't offer is [`Never`]: it can't be built, and its arm is matched by `match never {}`. What a
//! question offers ([`Answer::OFFERED`]) is read from the type, so a declaration can't drift from it.

/// An answer a request kind doesn't offer.
pub type Never = std::convert::Infallible;

/// Approved once: the daemon bounds the operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Once<T>(pub T);

/// Approved until the current run settles: the daemon can't bound the operation (a block in pi's own process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntilSettled<T>(pub T);

/// The user's answer to a request of a kind whose narrowest approval is `N` ([`Once`] or [`UntilSettled`]), whose
/// session-long approval carries `S`, whose saved one carries `A`, and whose denial carries `D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer<N, S, A, D> {
    Narrowest(N),
    /// Until the session ends; nothing saved.
    Session(S),
    /// Saved where the kind says.
    Always(A),
    Deny(D),
}

/// Which narrowest approval a kind offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Narrowest {
    Once,
    UntilSettled,
}

/// The answers a question offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offered {
    pub narrowest: Narrowest,
    pub session: bool,
    pub always: bool,
}

/// A narrowest approval's kind.
pub trait NarrowestKind {
    const KIND: Narrowest;
}

impl<T> NarrowestKind for Once<T> {
    const KIND: Narrowest = Narrowest::Once;
}

impl<T> NarrowestKind for UntilSettled<T> {
    const KIND: Narrowest = Narrowest::UntilSettled;
}

/// Whether an answer's payload type can be built: [`Never`] can't.
pub trait Offer {
    const OFFERED: bool;
}

impl Offer for Never {
    const OFFERED: bool = false;
}

/// An answer that carries nothing beyond being given.
impl Offer for () {
    const OFFERED: bool = true;
}

impl<N: NarrowestKind, S: Offer, A: Offer, D> Answer<N, S, A, D> {
    /// What this answer type lets a question offer.
    pub const OFFERED: Offered = Offered {
        narrowest: N::KIND,
        session: S::OFFERED,
        always: A::OFFERED,
    };
}

/// Whether a request asks the user before it's done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asks {
    /// Always, with the answers it offers.
    Always(Offered),
    /// Only when it needs more than the caller has (a bash call's rerun), with the answers it offers then.
    When(Offered),
    /// Never: it's answered from what the daemon knows.
    Never,
}
