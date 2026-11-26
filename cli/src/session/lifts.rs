//! A session's live lifts (#408): what pi's sandbox is widened for right now, as one state machine.
//!
//! The daemon issues each grant's token to pi's process over the `lifts` stream; the permissions extension consumes it
//! with the addon and reports back. A grant ends when its scope ends: an "until the run settles" one at pi's
//! `agent_settled`, a session one when the session does. Requests for a lift the session already holds (an explicit
//! `lift`, or the same block again) are answered from it without asking.
//!
//! ```text
//!   offered ──consumed──▶ active ──settled (UntilSettled) / stream closed──▶ (gone, revoke sent)
//!      └────failed / stream closed──▶ (gone)
//! ```

use std::collections::BTreeMap;

use crate::launch::sandbox::grant::LiveLift;

/// How long a grant lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Until pi's current run settles (`agent_settled`).
    UntilSettled,
    /// Until the session ends.
    Session,
}

/// One grant's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Sent to pi; not consumed yet.
    Offered,
    /// pi consumed it: its sandbox is widened.
    Active,
}

/// One grant: what it opens, for how long, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub lift: LiveLift,
    pub scope: Scope,
    pub state: State,
    /// The block it answered, when a block caused it: asked about again once the grant ends.
    pub block: Option<Block>,
}

/// A block a grant answered: (process, operation, target), as `ProcessBlocks` offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub process: String,
    pub operation: String,
    pub target: String,
    /// Whose block it was: an MCP server pi started keeps the sandbox it started with, so it's restarted to inherit.
    pub who: Who,
}

/// Which process in pi's sandbox was blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Who {
    /// pi's own process (its extensions, workflow tools): the consume reaches it.
    Pi,
    /// A process pi started (an MCP server): it inherits the grant only once started again.
    Child,
}

/// What the stream tells pi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// `for_child`: a process pi started was blocked (pi's own code can't tell which), so its MCP servers restart.
    Grant {
        id: u64,
        lift: LiveLift,
        for_child: bool,
    },
    Revoke {
        id: u64,
    },
}

/// What pi says about a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Consumed,
    Failed(String),
}

/// What a transition did, for the session to act on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Lines for pi's stream.
    pub send: Vec<Out>,
    /// Blocks to ask about again (their grants ended).
    pub forget: Vec<Block>,
    /// Grants that couldn't be consumed: what, and why.
    pub failed: Vec<(LiveLift, String)>,
}

/// Whether pi can be widened live: it has a stream open, or not (the addon didn't load, not macOS, not yet started).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Closed,
    Open,
}

/// Every grant of a session.
#[derive(Debug)]
pub struct Lifts {
    stream: Stream,
    grants: BTreeMap<u64, Grant>,
    next: u64,
}

impl Default for Lifts {
    fn default() -> Self {
        Self {
            stream: Stream::Closed,
            grants: BTreeMap::new(),
            next: 1,
        }
    }
}

impl Lifts {
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// pi opened its stream. A new stream replaces an old one: pi's grants from it were released with it.
    pub fn opened(&mut self) -> Effects {
        let effects = self.closed();
        self.stream = Stream::Open;
        effects
    }

    /// The stream closed (pi stopped, reloaded its extensions, or the session ended): every grant is gone.
    pub fn closed(&mut self) -> Effects {
        self.stream = Stream::Closed;
        let mut effects = Effects::default();
        for (_, grant) in std::mem::take(&mut self.grants) {
            effects.forget.extend(grant.block);
        }
        effects
    }

    /// Grants `lift` for `scope`. Not sent when the stream is closed (the caller checks [`Self::stream`] first).
    pub fn grant(&mut self, lift: LiveLift, scope: Scope, block: Option<Block>) -> Effects {
        match self.stream {
            Stream::Closed => Effects::default(),
            Stream::Open => {
                let id = self.next;
                self.next += 1;
                let for_child = match block.as_ref().map(|b| b.who) {
                    Some(Who::Child) => true,
                    Some(Who::Pi) | None => false,
                };
                self.grants.insert(
                    id,
                    Grant {
                        lift: lift.clone(),
                        scope,
                        state: State::Offered,
                        block,
                    },
                );
                Effects {
                    send: vec![Out::Grant { id, lift, for_child }],
                    ..Effects::default()
                }
            }
        }
    }

    /// pi's answer about grant `id`.
    pub fn reply(&mut self, id: u64, reply: Reply) -> Effects {
        let mut effects = Effects::default();
        match (self.grants.get_mut(&id), reply) {
            (Some(grant), Reply::Consumed) => grant.state = State::Active,
            (Some(_), Reply::Failed(why)) => {
                if let Some(grant) = self.grants.remove(&id) {
                    effects.forget.extend(grant.block);
                    effects.failed.push((grant.lift, why));
                }
            }
            // Revoked meanwhile, or never ours: nothing to do (pi releases on revoke).
            (None, Reply::Consumed | Reply::Failed(_)) => {}
        }
        effects
    }

    /// pi's run settled: every "until the run settles" grant ends.
    pub fn settled(&mut self) -> Effects {
        let mut effects = Effects::default();
        let ending: Vec<u64> = self
            .grants
            .iter()
            .filter(|(_, g)| match g.scope {
                Scope::UntilSettled => true,
                Scope::Session => false,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ending {
            if let Some(grant) = self.grants.remove(&id) {
                effects.send.push(Out::Revoke { id });
                effects.forget.extend(grant.block);
            }
        }
        effects
    }

    /// Whether a grant of this session already covers `lift` (an equal one, or a folder write over a path in it).
    pub fn covers(&self, lift: &LiveLift) -> bool {
        self.grants.values().any(|grant| covers(&grant.lift, lift))
    }
}

/// Whether `held` opens what `wanted` needs.
fn covers(held: &LiveLift, wanted: &LiveLift) -> bool {
    match (held, wanted) {
        (LiveLift::Write { path: held }, LiveLift::Write { path: wanted }) => wanted.starts_with(held),
        (LiveLift::Read { path: held }, LiveLift::Read { path: wanted }) => wanted.starts_with(held),
        (LiveLift::Service { name: held }, LiveLift::Service { name: wanted }) => held == wanted,
        (LiveLift::Listen, LiveLift::Listen) => true,
        (
            LiveLift::Write { .. } | LiveLift::Read { .. } | LiveLift::Service { .. } | LiveLift::Listen,
            LiveLift::Write { .. } | LiveLift::Read { .. } | LiveLift::Service { .. } | LiveLift::Listen,
        ) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &str) -> LiveLift {
        LiveLift::Write { path: path.into() }
    }

    fn block(target: &str) -> Block {
        Block {
            process: "node(1)".into(),
            operation: "file-write-data".into(),
            target: target.into(),
            who: Who::Pi,
        }
    }

    /// Nothing is granted without a stream; a grant is sent, consumed, and revoked when the run settles, and its block
    /// is asked about again.
    #[test]
    fn until_settled_grants_end_when_the_run_settles() {
        let mut lifts = Lifts::default();
        assert_eq!(lifts.grant(write("/a"), Scope::UntilSettled, None), Effects::default());
        lifts.opened();
        let e = lifts.grant(
            write("/a/.git/config"),
            Scope::UntilSettled,
            Some(block("/a/.git/config")),
        );
        assert_eq!(
            e.send,
            [Out::Grant {
                id: 1,
                lift: write("/a/.git/config"),
                for_child: false,
            }]
        );
        lifts.reply(1, Reply::Consumed);
        let e = lifts.grant(write("/b"), Scope::Session, None);
        assert_eq!(
            e.send,
            [Out::Grant {
                id: 2,
                lift: write("/b"),
                for_child: false,
            }]
        );
        // A block of a process pi started: its grant says so, so pi's MCP servers restart.
        let child = Block {
            who: Who::Child,
            ..block("/c")
        };
        let e = lifts.grant(write("/c"), Scope::Session, Some(child));
        assert_eq!(
            e.send,
            [Out::Grant {
                id: 3,
                lift: write("/c"),
                for_child: true,
            }]
        );
        assert!(lifts.covers(&write("/a/.git/config")));
        assert!(lifts.covers(&write("/b/inner")), "a folder covers what's in it");
        let e = lifts.settled();
        assert_eq!(e.send, [Out::Revoke { id: 1 }]);
        assert_eq!(e.forget, [block("/a/.git/config")]);
        assert!(!lifts.covers(&write("/a/.git/config")));
        assert!(lifts.covers(&write("/b")), "a session grant stays");
        assert_eq!(lifts.settled(), Effects::default());
    }

    /// A grant pi couldn't consume ends at once, says why, and its block is asked again.
    #[test]
    fn a_failed_grant_ends() {
        let mut lifts = Lifts::default();
        lifts.opened();
        lifts.grant(write("/a"), Scope::Session, Some(block("/a")));
        let e = lifts.reply(1, Reply::Failed("no addon".into()));
        assert_eq!(e.failed, [(write("/a"), "no addon".to_string())]);
        assert_eq!(e.forget, [block("/a")]);
        assert!(!lifts.covers(&write("/a")));
        assert_eq!(lifts.reply(1, Reply::Consumed), Effects::default(), "a late reply");
    }

    /// The stream closing ends every grant, and a new stream starts empty.
    #[test]
    fn a_closed_stream_ends_every_grant() {
        let mut lifts = Lifts::default();
        lifts.opened();
        lifts.grant(write("/a"), Scope::Session, Some(block("/a")));
        lifts.grant(LiveLift::Listen, Scope::UntilSettled, None);
        let e = lifts.closed();
        assert_eq!(e.forget, [block("/a")]);
        assert_eq!(lifts.stream(), Stream::Closed);
        assert!(!lifts.covers(&LiveLift::Listen));
        lifts.opened();
        lifts.grant(write("/c"), Scope::Session, None);
        let e = lifts.opened();
        assert!(e.send.is_empty());
        assert!(!lifts.covers(&write("/c")), "a reopened stream starts empty");
    }

    #[test]
    fn kinds_cover_only_their_own() {
        assert!(covers(&LiveLift::Listen, &LiveLift::Listen));
        assert!(!covers(&write("/a"), &LiveLift::Read { path: "/a".into() }));
        assert!(covers(
            &LiveLift::Service { name: "x".into() },
            &LiveLift::Service { name: "x".into() }
        ));
        assert!(!covers(&write("/a/b"), &write("/a")));
    }
}
