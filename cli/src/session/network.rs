//! A sandboxed session's filtered network: its gate, the proxy user of each sandbox it runs (pi's and each bash call's),
//! and the decision for each connection the gate holds. The decision, in order: the sandbox's
//! `[sandbox.network.connections]` entries, pi's trusted hosts (`pi::catalog`), the hosts the user allowed for this
//! session, a `silence` entry (refused without asking), else the user is asked while the connection waits: a
//! [`NetworkQuestion`] in the session's questions (`questions.rs`), which every connection to the same destination joins.

use std::collections::HashMap;

use tokio::sync::oneshot;

use crate::launch::sandbox::{
    Destination, Policy,
    network::{self as gate, User, Verdict},
};

/// The proxy user of pi's own sandbox.
pub const PI_USER: &str = "pi";

/// One connection waiting for the user.
pub struct Waiting {
    pub user: User,
    pub to: Destination,
    pub reply: oneshot::Sender<Verdict>,
}

/// The question for a destination, and the connections waiting on it.
pub struct NetworkQuestion {
    pub to: Destination,
    pub waiting: Vec<Waiting>,
}

impl NetworkQuestion {
    pub fn new(waiting: Waiting) -> Self {
        Self {
            to: waiting.to.clone(),
            waiting: vec![waiting],
        }
    }

    /// Drops the connections that stopped waiting (the gate let them go): true when none is left.
    pub fn prune(&mut self) -> bool {
        self.waiting.retain(|w| !w.reply.is_closed());
        self.waiting.is_empty()
    }

    /// Answers every connection waiting on it.
    pub fn settle(self, verdict: &Verdict) {
        for waiting in self.waiting {
            let _ = waiting.reply.send(verdict.clone());
        }
    }
}

/// The session's network.
pub struct Network {
    pub gate: gate::Gate,
    pub catalog: crate::pi::catalog::Catalog,
    /// The `ssh` wrapper's folder, when the daemon made it.
    pub ssh: Option<std::path::PathBuf>,
    /// Each sandbox's policy by proxy user: pi's, and each call's while it runs.
    policies: HashMap<String, Policy>,
    /// Destinations the user allowed for this session.
    allowed: Vec<Destination>,
}

impl Network {
    pub fn new(gate: gate::Gate, catalog: crate::pi::catalog::Catalog, ssh: Option<std::path::PathBuf>) -> Self {
        Self {
            gate,
            catalog,
            ssh,
            policies: HashMap::new(),
            allowed: Vec::new(),
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

    /// The decision for a connection, before any question: a named-host entry decides; then pi's own hosts and the
    /// hosts allowed for this session; then a generic entry (`*`, `*:PORT`, `localhost…`), so `"*" = "deny"` refuses
    /// without asking but never cuts pi off from its providers; else the user is asked.
    pub fn decide(&self, user: User, to: Destination, reply: oneshot::Sender<Verdict>) -> Option<Waiting> {
        use crate::config::Effect;
        let refused = |entry: &crate::launch::sandbox::NetworkGrant| Verdict::Deny {
            reason: format!(
                "Piton's sandbox refused {to}: `{}` = \"deny\" in [sandbox.network.connections]",
                entry.target
            ),
        };
        let verdict = match self.policies.get(&user.0) {
            None => Some(Verdict::Deny {
                reason: format!("Piton's sandbox refused {to}: the sandbox that asked has ended"),
            }),
            Some(policy) => {
                let literal = to.host.parse::<std::net::IpAddr>().is_ok();
                let entry = policy.network_entry(&to);
                let open = matches!(policy.network, crate::launch::sandbox::Network::Open { .. });
                match (open, entry) {
                    (true, _) => Some(Verdict::Allow { literal }),
                    (false, Some(entry)) if entry.target.is_named() => match entry.effect {
                        Effect::Allow => Some(Verdict::Allow { literal }),
                        Effect::Deny => Some(refused(entry)),
                    },
                    (false, entry) => {
                        if self.catalog.allows(&to.host, to.port).is_some() || self.allowed.contains(&to) {
                            Some(Verdict::Allow { literal: false })
                        } else {
                            match entry.map(|e| (e, e.effect)) {
                                Some((_, Effect::Allow)) => Some(Verdict::Allow { literal }),
                                Some((entry, Effect::Deny)) => Some(refused(entry)),
                                None => None,
                            }
                        }
                    }
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

    /// Lets every policy's network allow `to` from now on, as a saved `[sandbox.network.connections]` entry would after
    /// a restart. The gate decides named hosts, so nothing in Seatbelt changes.
    pub fn add_entry(&mut self, target: crate::config::NetworkTarget, source: crate::launch::sandbox::Source) {
        for policy in self.policies.values_mut() {
            if let crate::launch::sandbox::Network::Filtered { grants, .. } = &mut policy.network {
                grants.retain(|g| g.target != target);
                grants.push(crate::launch::sandbox::NetworkGrant {
                    target: target.clone(),
                    effect: crate::config::Effect::Allow,
                    source: source.clone(),
                });
            }
        }
    }
}

/// A refusal's reason when the user refused or didn't answer.
pub fn refused(to: &Destination, why: &str) -> Verdict {
    Verdict::Deny {
        reason: format!(
            "Piton's sandbox refused {to}: {why}; `\"{to}\" = \"allow\"` in [sandbox.network.connections] (global \
             config.toml or a config.local.toml) allows it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::sandbox::{Gate, Network as Net, NetworkGrant, Source};

    fn policy(entries: &[(&str, crate::config::Effect)]) -> Policy {
        let grants = entries
            .iter()
            .map(|(e, effect)| NetworkGrant {
                target: crate::config::NetworkTarget::try_from(e.to_string()).unwrap(),
                effect: *effect,
                source: Source::Workspace,
            })
            .collect();
        Policy {
            rules: Vec::new(),
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: Net::Filtered {
                grants,
                gate: Gate::None,
            },
            listen: crate::launch::sandbox::Listen::Closed,
            tag: String::new(),
            lifts: Default::default(),
        }
    }

    fn to(text: &str) -> Destination {
        Destination::parse(text).unwrap()
    }

    fn allow_(literal: bool) -> Option<Verdict> {
        Some(Verdict::Allow { literal })
    }

    /// The decision order: a named-host entry, pi's hosts, the session's answers, a generic entry, else the user. Each
    /// sandbox decides under its own policy; an unknown user is refused.
    #[tokio::test]
    async fn decides_in_order() {
        use crate::config::Effect::{Allow, Deny};
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let deny: gate::Decide =
            std::sync::Arc::new(|_, _| Box::pin(async { Verdict::Deny { reason: String::new() } }));
        let gate = gate::Gate::start(deny, Some(gate::Route::Direct), tx).unwrap();
        let builtin = crate::pi::catalog::Builtin {
            providers: [("anthropic".to_string(), vec!["api.anthropic.com".to_string()])].into(),
            sign_in: Vec::new(),
        };
        let files = crate::pi::catalog::Files::new(dir.path().join("agent"), dir.path(), &[]);
        let mut net = Network::new(gate, crate::pi::catalog::Catalog::new(builtin, files), None);
        let mut pi = policy(&[("github.com:22", Allow), ("bad.example", Deny)]);
        net.issue(PI_USER, &mut pi);
        assert!(matches!(&pi.network, Net::Filtered { gate: Gate::Listening { user, .. }, .. } if user == PI_USER));
        let mut call = policy(&[("api.vendor.com", Allow)]);
        net.issue("s-1", &mut call);
        // `*` denied: refused without asking, except pi's own hosts and a named host allowed.
        let mut closed = policy(&[("*", Deny), ("ok.example", Allow)]);
        net.issue("s-2", &mut closed);
        // `*` allowed with a named host denied: the rest passes without asking.
        let mut open = policy(&[("*", Allow), ("evil.example", Deny)]);
        net.issue("s-3", &mut open);
        let decide = |net: &Network, user: &str, dest: &str| {
            let (reply, mut verdict) = oneshot::channel();
            let waiting = net.decide(User(user.into()), to(dest), reply);
            (waiting.is_some(), verdict.try_recv().ok())
        };
        let denied = |r: (bool, Option<Verdict>)| matches!(r, (false, Some(Verdict::Deny { .. })));
        assert!(denied(decide(&net, "s-2", "other.example:443")), "`*` denied");
        assert_eq!(
            decide(&net, "s-2", "api.anthropic.com:443"),
            (false, allow_(false)),
            "pi's host"
        );
        assert_eq!(decide(&net, "s-2", "ok.example:443"), (false, allow_(false)));
        assert!(denied(decide(&net, "s-3", "evil.example:443")), "the named deny");
        assert_eq!(
            decide(&net, "s-3", "other.example:443"),
            (false, allow_(false)),
            "`*` allowed"
        );
        assert_eq!(
            decide(&net, "s-3", "api.anthropic.com:443"),
            (false, allow_(false)),
            "pi's host"
        );
        let allow = allow_;
        assert_eq!(decide(&net, PI_USER, "github.com:22"), (false, allow(false)));
        assert_eq!(
            decide(&net, PI_USER, "api.anthropic.com:443"),
            (false, allow(false)),
            "pi's host"
        );
        assert_eq!(
            decide(&net, "s-1", "api.vendor.com:443"),
            (false, allow(false)),
            "the call's own grant"
        );
        assert!(decide(&net, PI_USER, "api.vendor.com:443").0, "not pi's: asked");
        assert!(matches!(
            decide(&net, PI_USER, "bad.example:443"),
            (false, Some(Verdict::Deny { .. }))
        ));
        assert!(matches!(
            decide(&net, "s-9", "github.com:22"),
            (false, Some(Verdict::Deny { .. }))
        ));
        net.allow_for_session(to("api.vendor.com:443"));
        assert_eq!(
            decide(&net, PI_USER, "api.vendor.com:443"),
            (false, allow(false)),
            "allowed for the session"
        );
        net.forget("s-1");
        assert!(matches!(
            decide(&net, "s-1", "github.com:22"),
            (false, Some(Verdict::Deny { .. }))
        ));
        // A saved entry applies at once to every sandbox's policy.
        net.add_entry(
            crate::config::NetworkTarget::Host {
                host: "new.example".into(),
                port: Some(443),
            },
            Source::Workspace,
        );
        assert_eq!(decide(&net, PI_USER, "new.example:443"), (false, allow(false)));
    }
}
