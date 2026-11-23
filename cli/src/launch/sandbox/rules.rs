//! The resolved policy of one launch as an ordered list of rules: what `piton sandbox-exec` turns into Seatbelt, what
//! `piton sandbox show` prints, and what `piton sandbox why` evaluates. Seatbelt applies the last matching rule, and so
//! does [`Policy::decide`], so the explanation is the rule that decided.
//!
//! The base, applied first: everything is readable, nothing is writable, and no Unix socket can be connected to or
//! created. TCP stays as nono leaves it (open; #228 routes it through a proxy).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::policy::Source;
use crate::{
    agent::{GitControl, ProtectedRoots},
    config::{NetworkTarget, ServiceName},
};

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
    /// Create a Unix socket (`bind`): listen on it.
    Bind,
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
    /// Whether the rule applies to `path`, compared without case as Seatbelt compares on APFS (case-insensitive by
    /// default): a lowercase rule covers `~/.SSH/x` and `TRUST.JSON.LOCK` too, existing or not.
    pub(super) fn covers(&self, path: &Path) -> bool {
        match self {
            Target::Subpath { path: root } => folded(path).starts_with(folded(root)),
            Target::Literal { path: exact } => folded(path) == folded(exact),
            Target::Prefix { path: prefix } => folded(path)
                .as_os_str()
                .as_encoded_bytes()
                .starts_with(folded(prefix).as_os_str().as_encoded_bytes()),
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

/// A path in one case, for comparing as the file system does (the protected files and Git's control files compare
/// the same way).
fn folded(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
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
    /// Keychain Mach services allowed: a `read` entry covers the user's keychains. Decided after `services`.
    pub keychain: bool,
    /// The Mach services the sandbox may look up.
    pub services: Services,
    /// Whether processes may launch apps and send Apple Events.
    pub apps: Apps,
    /// The network destinations `connect` grants, in order: carried for `piton sandbox show` and the network filter
    /// (#228). They decide nothing yet: TCP is open.
    #[serde(default)]
    pub network: Vec<NetworkGrant>,
    /// The launch's tag: every deny rule carries it as its message, so the kernel's record of a refusal names the
    /// session it came from (`(with message "piton-<tag>")`). Empty: untagged (`piton sandbox show`, tests).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
}

/// The prefix of every launch's tag in the kernel's log.
pub const TAG_PREFIX: &str = "piton-";

/// The Mach services a launch may look up. Any app's service is reachable through it, so a service whose app runs
/// commands (Hammerspoon's, Keyboard Maestro's) runs them on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Services {
    /// Every service but the keychain's, from this entry.
    Every { source: Source },
    /// Only these: the built-in list, then the presets' and the user's entries, each with where it came from.
    Listed { grants: Vec<ServiceGrant> },
}

/// A Mach service the policy allows, and where the grant came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceGrant {
    pub name: ServiceName,
    pub source: Source,
}

/// Launching apps (`lsopen`: `open`, Launch Services) and sending Apple Events (`osascript`, scripting another app).
/// What macOS starts this way runs outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Apps {
    Closed,
    /// Open, from this entry.
    Open {
        source: Source,
    },
}

/// A network destination the policy grants, and where the grant came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkGrant {
    pub target: NetworkTarget,
    pub source: Source,
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
                allowed: false,
                source: Source::BuiltIn {
                    what: CLOSED_SOCKETS.into(),
                },
            },
            Access::Bind => Decision {
                allowed: false,
                source: Source::BuiltIn {
                    what: CLOSED_BIND.into(),
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

    /// The decision for a lookup of the Mach service `service`, as Seatbelt makes it: the keychain's decide last.
    pub fn decide_service(&self, service: &str) -> Decision {
        if KEYCHAIN_SERVICES.contains(&service) {
            return match self.keychain {
                true => Decision {
                    allowed: true,
                    source: Source::BuiltIn {
                        what: "the keychain: a `read` entry covers it".into(),
                    },
                },
                false => Decision {
                    allowed: false,
                    source: Source::BuiltIn {
                        what: CLOSED_KEYCHAIN.into(),
                    },
                },
            };
        }
        match &self.services {
            Services::Every { source } => Decision {
                allowed: true,
                source: source.clone(),
            },
            Services::Listed { grants } => match grants.iter().rev().find(|g| g.name.matches(service)) {
                Some(grant) => Decision {
                    allowed: true,
                    source: grant.source.clone(),
                },
                None => Decision {
                    allowed: false,
                    source: Source::BuiltIn {
                        what: CLOSED_SERVICES.into(),
                    },
                },
            },
        }
    }

    /// The Mach service and app rules, after the path rules and before the keychain's: under a list, every lookup is
    /// denied and the listed ones allowed again (nono allows every lookup first).
    fn service_rules(&self) -> Vec<String> {
        let mut out = Vec::new();
        match &self.services {
            Services::Every { .. } => {}
            Services::Listed { grants } => {
                out.push("(deny mach-lookup)".to_string());
                for grant in grants {
                    out.push(match &grant.name {
                        ServiceName::Exact(name) => format!("(allow mach-lookup (global-name \"{name}\"))"),
                        ServiceName::Prefix(start) => format!("(allow mach-lookup (global-name-prefix \"{start}\"))"),
                    });
                }
            }
        }
        match &self.apps {
            // nono's own default deny already refuses both, without a message: repeated here so the tag pass marks them,
            // and a blocked `open` reaches its call in the kernel's record.
            Apps::Closed => {
                out.push("(deny lsopen)".to_string());
                out.push("(deny appleevent-send)".to_string());
            }
            Apps::Open { .. } => {
                out.push("(allow lsopen)".to_string());
                out.push("(allow appleevent-send)".to_string());
            }
        }
        out
    }

    /// The Seatbelt rules, in order. Filesystem and Mach rules first, network rules last: the library emits them in
    /// two groups (after its file allows, and after its network section), each in this order.
    ///
    /// With a tag, every deny carries it, and a leading tagged `(deny file-write*)` labels writes outside every grant
    /// (the library's own default deny has no message). It shadows nothing: the library grants no writes of its own
    /// for Piton's capability set, and every grant here comes after it.
    pub fn seatbelt(&self) -> Result<Vec<String>, String> {
        let tagged = self.seatbelt_untagged()?;
        if self.tag.is_empty() {
            return Ok(tagged);
        }
        let message = format!("(with message \"{TAG_PREFIX}{}\")", escape_tag(&self.tag));
        let tag = |rule: String| match rule.strip_prefix("(deny ").and_then(|r| r.strip_suffix(')')) {
            Some(body) => format!("(deny {body} {message})"),
            None => rule,
        };
        let is_network = |rule: &String| rule.starts_with("(allow network") || rule.starts_with("(deny network");
        let (network, other): (Vec<String>, Vec<String>) = tagged.into_iter().partition(is_network);
        let mut out = vec![tag("(deny file-write*)".into())];
        out.extend(other.into_iter().map(tag));
        out.extend(network.into_iter().map(tag));
        Ok(out)
    }

    fn seatbelt_untagged(&self) -> Result<Vec<String>, String> {
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
                    (Effect::Allow, Access::Bind) => {
                        out.push(format!("(allow network-bind {})", filter(path, true)?));
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
                    (Effect::Deny, Access::Bind) => {
                        out.push(format!("(deny network-bind {})", filter(path, true)?));
                    }
                }
            }
        }
        out.extend(self.service_rules());
        // The keychain's services decide after every service rule, so no entry (a `com.apple.*` prefix) reopens them;
        // the library's own denies come before the platform rules and carry no message.
        let effect = match self.keychain {
            true => "allow",
            false => "deny",
        };
        for service in KEYCHAIN_SERVICES {
            out.push(format!("({effect} mach-lookup (global-name \"{service}\"))"));
        }
        // Network rules after the rest, keeping their own order, after the base's socket denies: a path filter on a
        // network operation matches Unix sockets only, so TCP stays as the library leaves it.
        let (network, other): (Vec<String>, Vec<String>) = out
            .into_iter()
            .partition(|rule| rule.starts_with("(allow network") || rule.starts_with("(deny network"));
        let base = SOCKET_BASE.iter().map(|rule| rule.to_string());
        Ok(other.into_iter().chain(base).chain(network).collect())
    }
}

/// The base's Unix socket rules: no socket can be connected to or created. Every socket rule of the policy follows.
const SOCKET_BASE: [&str; 2] = [
    r#"(deny network-outbound (subpath "/"))"#,
    r#"(deny network-bind (subpath "/"))"#,
];

/// Why a socket connect is denied when no rule decides: what `piton sandbox why` prints.
const CLOSED_SOCKETS: &str = "Unix sockets are closed: a `connect` entry opens one (global config.toml or a \
                              config.local.toml)";

/// Why creating a socket is denied when no rule decides.
const CLOSED_BIND: &str = "creating Unix sockets is closed: a `bind` entry opens a path (global config.toml or a \
                           config.local.toml)";

/// Why a lookup of a service outside the list is denied: what `piton sandbox why --service` prints.
const CLOSED_SERVICES: &str = "Mach services outside the built-in list are closed: a `services` entry opens one (global \
                               config.toml or a config.local.toml)";

/// Why a lookup of a keychain service is denied.
const CLOSED_KEYCHAIN: &str = "the keychain is closed: a `read` of ~/Library/Keychains opens it";

/// The Mach services that broker keychain access, which the library always denies. Reopened only when the user exposes
/// their keychains (nono's CLI does the same).
const KEYCHAIN_SERVICES: [&str; 5] = [
    "com.apple.SecurityServer",
    "com.apple.securityd",
    "com.apple.security.keychaind",
    "com.apple.secd",
    "com.apple.security.agent",
];

/// A tag as a Seatbelt string's content: letters, digits, and `-` only.
fn escape_tag(tag: &str) -> String {
    tag.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect()
}

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
                rule(Effect::Allow, &[Access::Read], "/h/.ssh/config", "read"),
            ],
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: Vec::new(),
            tag: String::new(),
        };
        let d = |p: &str, a| policy.decide(Path::new(p), a);
        assert!(d("/etc/hosts", Access::Read).allowed);
        assert!(!d("/h/.ssh/id_ed25519", Access::Read).allowed);
        assert!(d("/h/.ssh/config", Access::Read).allowed);
        assert!(!d("/h/.ssh/config", Access::Write).allowed);
        assert!(d("/w/src/main.rs", Access::Write).allowed);
        assert_eq!(d("/w/src/main.rs", Access::Write).source, Source::Workspace);
        assert!(!d("/elsewhere", Access::Write).allowed);
        // Unix sockets are closed in the base.
        assert!(!d("/any.sock", Access::Connect).allowed);
        assert!(!d("/w/new.sock", Access::Bind).allowed);
    }

    /// Seatbelt compares without case on APFS, so the explanation does too: a deny holds and an allow inside it applies
    /// for other-case paths, including ones that don't exist yet (no on-disk case to restore).
    #[test]
    fn paths_compare_without_case() {
        let policy = Policy {
            rules: vec![
                rule(Effect::Deny, &[Access::Read, Access::Write], "/h/.ssh", "deny"),
                rule(Effect::Allow, &[Access::Read], "/h/.ssh/config", "read"),
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Write],
                    target: Target::Literal {
                        path: "/a/crashes.json".into(),
                    },
                    source: Source::AgentDir,
                },
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Write],
                    target: Target::Prefix {
                        path: "/a/lock-".into(),
                    },
                    source: Source::AgentDir,
                },
            ],
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: Vec::new(),
            tag: String::new(),
        };
        let d = |p: &str, a| policy.decide(Path::new(p), a).allowed;
        assert!(!d("/H/.SSH/id_ed25519", Access::Read));
        assert!(!d("/h/.Ssh", Access::Read));
        assert!(d("/h/.SSH/CONFIG", Access::Read));
        assert!(d("/a/CRASHES.JSON", Access::Write));
        assert!(d("/A/LOCK-1/x", Access::Write));
        assert!(!d("/a/crashes.json.bak", Access::Write));
        // A shared prefix of text isn't a parent folder.
        assert!(d("/h/.sshx", Access::Read));
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
                rule(Effect::Allow, &[Access::Bind], "/w", "bind"),
            ],
            keychain: true,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: Vec::new(),
            tag: String::new(),
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
        // The base's socket denies first, then the policy's socket rules in order.
        assert_eq!(
            &rules[first_network..],
            [
                r#"(deny network-outbound (subpath "/"))"#,
                r#"(deny network-bind (subpath "/"))"#,
                r#"(deny network-outbound (subpath "/s/\"q\""))"#,
                r#"(allow network-outbound (path "/s/agent.sock"))"#,
                r#"(allow network-bind (subpath "/w"))"#,
            ]
        );
        assert!(policy.decide(Path::new("/w/x.sock"), Access::Bind).allowed);
        assert!(!policy.decide(Path::new("/w/x.sock"), Access::Connect).allowed);
        assert!(rules.iter().any(|r| r.contains("com.apple.secd")));
        assert!(
            Policy {
                rules: vec![rule(Effect::Deny, &[Access::Write], "/a\nb", "x")],
                ..policy.clone()
            }
            .seatbelt()
            .is_err()
        );

        // Tagged: every deny carries the launch's tag, a leading write deny labels the base's refusals, and the
        // keychain denies are repeated with it. Allows and the order of everything else are unchanged.
        let tagged = Policy {
            tag: "abc\"1".into(),
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            ..policy
        }
        .seatbelt()
        .unwrap();
        let message = r#"(with message "piton-abc1")"#;
        assert_eq!(tagged[0], format!("(deny file-write* {message})"));
        assert_eq!(
            tagged[1],
            format!(r#"(deny file-write* (subpath "/s/\"q\"") {message})"#)
        );
        assert!(
            tagged
                .iter()
                .filter(|r| r.starts_with("(deny"))
                .all(|r| r.ends_with(&format!("{message})")))
        );
        assert!(tagged.contains(&format!(
            r#"(deny mach-lookup (global-name "com.apple.secd") {message})"#
        )));
        assert!(tagged.contains(&r#"(allow file-write* (prefix "/a/lock-"))"#.to_string()));
        let first_network = tagged.iter().position(|r| r.contains("network")).unwrap();
        assert!(tagged[first_network..].iter().all(|r| r.contains("network")));
        // The base's socket denies carry the tag, so a blocked socket reaches the block question.
        assert_eq!(
            tagged[first_network],
            format!(r#"(deny network-outbound (subpath "/") {message})"#)
        );
    }

    fn listed(names: &[ServiceName]) -> Services {
        Services::Listed {
            grants: names
                .iter()
                .map(|name| ServiceGrant {
                    name: name.clone(),
                    source: Source::Workspace,
                })
                .collect(),
        }
    }

    fn services_policy(services: Services, apps: Apps, keychain: bool) -> Policy {
        Policy {
            rules: Vec::new(),
            keychain,
            services,
            apps,
            network: Vec::new(),
            tag: String::new(),
        }
    }

    /// A list denies every lookup, then allows each entry; the keychain's services decide after every entry, so a
    /// prefix never reopens them. Every service emits nothing of its own: the library allows them.
    #[test]
    fn services_and_apps_in_seatbelt() {
        let prefix = ServiceName::Prefix("com.apple.".into());
        let exact = ServiceName::Exact("Hammerspoon".into());
        let p = services_policy(
            listed(&[exact.clone(), prefix.clone()]),
            Apps::Open {
                source: Source::Workspace,
            },
            false,
        );
        let rules = p.seatbelt().unwrap();
        let at = |r: &str| {
            rules
                .iter()
                .position(|x| x == r)
                .unwrap_or_else(|| panic!("{r} in {rules:?}"))
        };
        let deny_all = at("(deny mach-lookup)");
        assert!(deny_all < at(r#"(allow mach-lookup (global-name "Hammerspoon"))"#));
        let allow_prefix = at(r#"(allow mach-lookup (global-name-prefix "com.apple."))"#);
        assert!(allow_prefix < at(r#"(deny mach-lookup (global-name "com.apple.secd"))"#));
        at("(allow lsopen)");
        at("(allow appleevent-send)");
        assert!(p.decide_service("Hammerspoon").allowed);
        assert!(p.decide_service("com.apple.fonts").allowed);
        assert!(
            !p.decide_service("com.apple.secd").allowed,
            "a prefix doesn't reopen the keychain"
        );
        assert!(!p.decide_service("org.other").allowed);
        super::super::apply::capabilities(&p).expect("nono accepts it");

        let every = services_policy(
            Services::Every {
                source: Source::Workspace,
            },
            Apps::Closed,
            true,
        );
        let rules = every.seatbelt().unwrap();
        assert!(
            !rules
                .iter()
                .any(|r| r == "(deny mach-lookup)" || r.contains("allow lsopen"))
        );
        assert!(rules.contains(&"(deny lsopen)".to_string()));
        assert!(rules.contains(&"(deny appleevent-send)".to_string()));
        super::super::apply::capabilities(&every).expect("nono accepts the closed apps' denies");
        let closed = Policy {
            tag: "t".into(),
            ..every.clone()
        }
        .seatbelt()
        .unwrap();
        assert!(closed.contains(&r#"(deny lsopen (with message "piton-t"))"#.to_string()));
        assert!(closed.contains(&r#"(deny appleevent-send (with message "piton-t"))"#.to_string()));
        assert!(rules.contains(&r#"(allow mach-lookup (global-name "com.apple.secd"))"#.to_string()));
        assert!(every.decide_service("org.other").allowed);
        assert!(every.decide_service("com.apple.secd").allowed, "keychain read");

        // Tagged: the list's deny carries the tag, so a blocked lookup reaches the question.
        let tagged = Policy { tag: "t".into(), ..p }.seatbelt().unwrap();
        assert!(tagged.contains(&r#"(deny mach-lookup (with message "piton-t"))"#.to_string()));
    }
}
