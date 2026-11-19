//! The resolved policy of one launch as an ordered list of rules: what `piton sandbox-exec` turns into Seatbelt, what
//! `piton sandbox show` prints, and what `piton sandbox why` evaluates. Seatbelt applies the last matching rule, and so
//! does [`Policy::decide`], so the explanation is the rule that decided.
//!
//! The base, applied first: everything is readable, the network is open, nothing is writable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::policy::Source;
use crate::agent::{GitControl, ProtectedRoots};

/// What a process does to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Read a file's contents, run it, or map it executable. Metadata (`stat`, listing a folder's entry) stays allowed.
    Read,
    /// Create, change, rename, or delete.
    Write,
    /// Connect to a Unix socket.
    Connect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
}

/// The paths a rule covers. Paths are absolute; [`Policy`] adds a rule's canonical form beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// The path and everything below it.
    Subpath { path: PathBuf },
    /// Exactly this path.
    Literal { path: PathBuf },
    /// Every path whose text starts with this one (`<dir>/name-` covers `<dir>/name-x.lock` and what's in it).
    Prefix { path: PathBuf },
    /// The protected files of a launch (`agent/protected.rs`): write rules only.
    Protected { source: PathBuf, checkout: PathBuf },
    /// Git's hooks and configuration in a launch's checkouts (`agent/git.rs`): write rules only.
    GitControl { control: GitControl },
}

impl Target {
    pub(super) fn covers(&self, path: &Path) -> bool {
        match self {
            Target::Subpath { path: root } => path.starts_with(root),
            Target::Literal { path: exact } => path == exact,
            Target::Prefix { path: prefix } => path
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(prefix.as_os_str().as_encoded_bytes()),
            Target::Protected { source, checkout } => ProtectedRoots {
                source: source.clone(),
                checkout: checkout.clone(),
            }
            .protected_root(path)
            .is_some(),
            Target::GitControl { control } => control.covers(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub effect: Effect,
    pub access: Vec<Access>,
    pub target: Target,
    pub source: Source,
}

/// One launch's sandbox, everything literal: the daemon resolves it and `piton sandbox-exec` applies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// In order; the last matching rule decides.
    pub rules: Vec<Rule>,
    /// Keychain Mach services allowed: an `expose` covers the user's keychains.
    pub keychain: bool,
}

/// Why an access is allowed or denied: the rule that decided, or the base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub allowed: bool,
    pub source: Source,
}

impl Policy {
    /// The decision for `access` to `path` (canonical, absolute), as Seatbelt makes it.
    pub fn decide(&self, path: &Path, access: Access) -> Decision {
        let mut decision = match access {
            Access::Read => Decision {
                allowed: true,
                source: Source::BuiltIn {
                    what: "everything is readable".into(),
                },
            },
            Access::Connect => Decision {
                allowed: true,
                source: Source::BuiltIn {
                    what: "the network is open".into(),
                },
            },
            Access::Write => Decision {
                allowed: false,
                source: Source::BuiltIn {
                    what: "only the workspace and granted paths are writable".into(),
                },
            },
        };
        for rule in &self.rules {
            if rule.access.contains(&access) && rule.target.covers(path) {
                decision = Decision {
                    allowed: match rule.effect {
                        Effect::Allow => true,
                        Effect::Deny => false,
                    },
                    source: rule.source.clone(),
                };
            }
        }
        decision
    }

    /// The Seatbelt rules, in order. Filesystem and Mach rules first, network rules last: the library emits them in
    /// two groups (after its file allows, and after its network section), each in this order.
    pub fn seatbelt(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for rule in &self.rules {
            let filter = |path: &Path, network: bool| -> Result<String, String> {
                let p = escape(path)?;
                Ok(match (&rule.target, network) {
                    (Target::Subpath { .. }, _) => format!("(subpath \"{p}\")"),
                    (Target::Literal { .. }, false) => format!("(literal \"{p}\")"),
                    (Target::Literal { .. }, true) => format!("(path \"{p}\")"),
                    (Target::Prefix { .. }, false) => format!("(prefix \"{p}\")"),
                    (Target::Prefix { .. }, true) => {
                        return Err(format!(
                            "{} is a name prefix, which can't name a socket",
                            path.display()
                        ));
                    }
                    (Target::Protected { .. } | Target::GitControl { .. }, _) => {
                        unreachable!("protected and Git control rules are emitted whole")
                    }
                })
            };
            let path = match &rule.target {
                Target::Subpath { path } | Target::Literal { path } | Target::Prefix { path } => path,
                Target::Protected { source, checkout } => {
                    // Write denies only: everything else of a protected file follows the other rules.
                    let roots = ProtectedRoots {
                        source: source.clone(),
                        checkout: checkout.clone(),
                    };
                    out.extend(roots.seatbelt_rules());
                    continue;
                }
                Target::GitControl { control } => {
                    // Write denies only, like the protected files.
                    out.extend(control.seatbelt_rules());
                    continue;
                }
            };
            for access in &rule.access {
                match (rule.effect, access) {
                    (Effect::Allow, Access::Read) => {
                        // Seatbelt ranks a rule's action above its order: a later `file-read*` allow loses to an
                        // earlier `file-read-data` deny. Name the actions the deny names, so the later allow wins.
                        out.push(format!("(allow file-read* {})", filter(path, false)?));
                        out.push(format!("(allow file-read-data {})", filter(path, false)?));
                        out.push(format!("(allow file-read-metadata {})", filter(path, false)?));
                        out.push(format!("(allow process-exec* {})", filter(path, false)?));
                        out.push(format!("(allow file-map-executable {})", filter(path, false)?));
                    }
                    (Effect::Allow, Access::Write) => out.push(format!("(allow file-write* {})", filter(path, false)?)),
                    (Effect::Allow, Access::Connect) => {
                        out.push(format!("(allow network-outbound {})", filter(path, true)?));
                    }
                    (Effect::Deny, Access::Read) => {
                        // A denied file can still be listed and stat'ed, as nono's deny groups do.
                        out.push(format!("(allow file-read-metadata {})", filter(path, false)?));
                        out.push(format!("(deny file-read-data {})", filter(path, false)?));
                        out.push(format!("(deny process-exec* {})", filter(path, false)?));
                        out.push(format!("(deny file-map-executable {})", filter(path, false)?));
                    }
                    (Effect::Deny, Access::Write) => out.push(format!("(deny file-write* {})", filter(path, false)?)),
                    (Effect::Deny, Access::Connect) => {
                        out.push(format!("(deny network-outbound {})", filter(path, true)?));
                    }
                }
            }
        }
        if self.keychain {
            for service in KEYCHAIN_SERVICES {
                out.push(format!("(allow mach-lookup (global-name \"{service}\"))"));
            }
        }
        // Network rules after the rest, keeping their own order.
        let (network, other): (Vec<String>, Vec<String>) = out
            .into_iter()
            .partition(|rule| rule.starts_with("(allow network") || rule.starts_with("(deny network"));
        Ok(other.into_iter().chain(network).collect())
    }
}

/// The Mach services that broker keychain access, which the library always denies. Reopened only when the user exposes
/// their keychains (nono's CLI does the same).
const KEYCHAIN_SERVICES: [&str; 5] = [
    "com.apple.SecurityServer",
    "com.apple.securityd",
    "com.apple.security.keychaind",
    "com.apple.secd",
    "com.apple.security.agent",
];

/// A path as a Seatbelt string literal's content.
fn escape(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} isn't valid UTF-8", path.display()))?;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => return Err(format!("{} contains a control character", path.display())),
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(effect: Effect, access: &[Access], path: &str, what: &str) -> Rule {
        Rule {
            effect,
            access: access.to_vec(),
            target: Target::Subpath { path: path.into() },
            source: Source::Piton { what: what.into() },
        }
    }

    #[test]
    fn the_last_matching_rule_decides() {
        let policy = Policy {
            rules: vec![
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Write],
                    target: Target::Subpath { path: "/w".into() },
                    source: Source::Workspace,
                },
                rule(Effect::Deny, &[Access::Read, Access::Write], "/h/.ssh", "deny"),
                rule(Effect::Allow, &[Access::Read], "/h/.ssh/config", "expose"),
            ],
            keychain: false,
        };
        let d = |p: &str, a| policy.decide(Path::new(p), a);
        assert!(d("/etc/hosts", Access::Read).allowed);
        assert!(!d("/h/.ssh/id_ed25519", Access::Read).allowed);
        assert!(d("/h/.ssh/config", Access::Read).allowed);
        assert!(!d("/h/.ssh/config", Access::Write).allowed);
        assert!(d("/w/src/main.rs", Access::Write).allowed);
        assert_eq!(d("/w/src/main.rs", Access::Write).source, Source::Workspace);
        assert!(!d("/elsewhere", Access::Write).allowed);
        assert!(d("/any.sock", Access::Connect).allowed);
    }

    #[test]
    fn seatbelt_keeps_network_rules_last_and_escapes_paths() {
        let policy = Policy {
            rules: vec![
                rule(Effect::Deny, &[Access::Write, Access::Connect], "/s/\"q\"", "state"),
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Connect],
                    target: Target::Literal {
                        path: "/s/agent.sock".into(),
                    },
                    source: Source::Piton { what: "agent".into() },
                },
                rule(Effect::Allow, &[Access::Read], "/s/logs", "logs"),
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Write],
                    target: Target::Prefix {
                        path: "/a/lock-".into(),
                    },
                    source: Source::AgentDir,
                },
            ],
            keychain: true,
        };
        assert!(policy.decide(Path::new("/a/lock-1/x"), Access::Write).allowed);
        assert!(!policy.decide(Path::new("/a/lock"), Access::Write).allowed);
        let rules = policy.seatbelt().unwrap();
        assert_eq!(rules[0], r#"(deny file-write* (subpath "/s/\"q\""))"#);
        // A read allow names `file-read-data`, which a deny names: a broader `file-read*` alone would lose to it.
        assert!(rules.contains(&r#"(allow file-read-data (subpath "/s/logs"))"#.to_string()));
        assert!(rules.contains(&r#"(allow file-write* (prefix "/a/lock-"))"#.to_string()));
        let first_network = rules.iter().position(|r| r.contains("network")).unwrap();
        assert!(rules[..first_network].iter().all(|r| !r.contains("network")));
        assert_eq!(
            &rules[first_network..],
            [
                r#"(deny network-outbound (subpath "/s/\"q\""))"#,
                r#"(allow network-outbound (path "/s/agent.sock"))"#
            ]
        );
        assert!(rules.iter().any(|r| r.contains("com.apple.secd")));
        assert!(
            Policy {
                rules: vec![rule(Effect::Deny, &[Access::Write], "/a\nb", "x")],
                ..policy
            }
            .seatbelt()
            .is_err()
        );
    }
}
