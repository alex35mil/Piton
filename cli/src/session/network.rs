//! A sandboxed session's filtered network: its gate, the proxy user of each sandbox it runs (pi's and each bash call's),
//! and the decision for each connection the gate holds. The decision, in order: the sandbox's `connect` entries, pi's
//! trusted hosts (`pi_catalog`), the hosts the user allowed for this session, a `silence` entry (refused without asking),
//! else the user is asked while the connection waits, one question at a time, sharing the session's question slot.

use std::collections::{HashMap, VecDeque};

use tokio::sync::oneshot;

use crate::{
    gate::{self, User, Verdict},
    launch::sandbox::{Destination, Policy},
};

/// The proxy user of pi's own sandbox.
pub const PI_USER: &str = "pi";

/// One connection waiting for the user.
pub struct Waiting {
    pub user: User,
    pub to: Destination,
    pub reply: oneshot::Sender<Verdict>,
}

/// The question showing for a connection, and the connections waiting on the same destination.
pub struct Asking {
    pub dialog_id: String,
    pub to: Destination,
    pub waiting: Vec<Waiting>,
    pub deadline: tokio::time::Instant,
}

/// The session's network.
pub struct Network {
    pub gate: gate::Gate,
    pub catalog: crate::pi_catalog::Catalog,
    /// The `ssh` wrapper's folder, when the daemon made it.
    pub ssh: Option<std::path::PathBuf>,
    /// Each sandbox's policy by proxy user: pi's, and each call's while it runs.
    policies: HashMap<String, Policy>,
    /// Destinations the user allowed for this session.
    allowed: Vec<Destination>,
    pub asking: Option<Asking>,
    pub queued: VecDeque<Waiting>,
}

impl Network {
    pub fn new(gate: gate::Gate, catalog: crate::pi_catalog::Catalog, ssh: Option<std::path::PathBuf>) -> Self {
        Self {
            gate,
            catalog,
            ssh,
            policies: HashMap::new(),
            allowed: Vec::new(),
            asking: None,
            queued: VecDeque::new(),
        }
    }

    /// A proxy user for a sandbox with `policy`: the policy's network gets this gate, with that user's credential.
    pub fn issue(&mut self, user: &str, policy: &mut Policy) {
        let (name, credential) = self.gate.issue(user);
        if let crate::launch::sandbox::Network::Filtered { gate, .. } = &mut policy.network {
            *gate = crate::launch::sandbox::Gate::Listening {
                port: self.gate.port,
                user: name.clone(),
                credential,
                ssh: self.ssh.clone(),
            };
        }
        self.policies.insert(name, policy.clone());
    }

    /// Forgets a sandbox's user (its call ended).
    pub fn forget(&mut self, user: &str) {
        self.gate.forget(user);
        self.policies.remove(user);
    }

    /// The decision for a connection, before any question. `silence` is the session's.
    pub fn decide(
        &self,
        user: User,
        to: Destination,
        silence: &[crate::launch::sandbox::Silenced],
        reply: oneshot::Sender<Verdict>,
    ) -> Option<Waiting> {
        let verdict = match self.policies.get(&user.0) {
            None => Some(Verdict::Deny {
                reason: format!("Piton's sandbox refused {to}: the sandbox that asked has ended"),
            }),
            Some(policy) => {
                let by_entry = policy.decide_network(&to).allowed;
                let literal = to.host.parse::<std::net::IpAddr>().is_ok();
                if by_entry {
                    Some(Verdict::Allow { literal })
                } else if self.catalog.allows(&to.host, to.port).is_some() || self.allowed.contains(&to) {
                    Some(Verdict::Allow { literal: false })
                } else if crate::launch::sandbox::silenced(silence, "", "network-outbound", &to.to_string()).is_some() {
                    Some(Verdict::Deny {
                        reason: format!(
                            "Piton's sandbox refused {to}: denied earlier (a `silence` entry); a `connect` entry \
                             allows it"
                        ),
                    })
                } else {
                    None
                }
            }
        };
        match verdict {
            Some(verdict) => {
                let _ = reply.send(verdict);
                None
            }
            None => Some(Waiting { user, to, reply }),
        }
    }

    /// Allows `to` for the rest of the session.
    pub fn allow_for_session(&mut self, to: Destination) {
        if !self.allowed.contains(&to) {
            self.allowed.push(to);
        }
    }

    /// Lets every policy's network allow `to` from now on, as a saved `connect` entry would after a restart. The gate
    /// decides named hosts, so nothing in Seatbelt changes.
    pub fn add_entry(&mut self, target: crate::config::NetworkTarget, source: crate::launch::sandbox::Source) {
        for policy in self.policies.values_mut() {
            if let crate::launch::sandbox::Network::Filtered { grants, .. } = &mut policy.network {
                grants.push(crate::launch::sandbox::NetworkGrant {
                    target: target.clone(),
                    source: source.clone(),
                });
            }
        }
    }

    /// Answers every connection waiting on the question showing.
    pub fn settle(&mut self, verdict: &Verdict) {
        if let Some(asking) = self.asking.take() {
            for waiting in asking.waiting {
                let _ = waiting.reply.send(verdict.clone());
            }
        }
    }
}

/// A refusal's reason when the user refused or didn't answer.
pub fn refused(to: &Destination, why: &str) -> Verdict {
    Verdict::Deny {
        reason: format!(
            "Piton's sandbox refused {to}: {why}; a `connect = [\"{to}\"]` entry (global config.toml or a \
             config.local.toml) allows it"
        ),
    }
}
