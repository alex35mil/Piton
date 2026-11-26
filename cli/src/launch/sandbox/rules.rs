//! The resolved policy of one launch as an ordered list of rules: what `piton sandbox-exec` turns into Seatbelt, what
//! `piton sandbox show` prints, and what `piton sandbox why` evaluates. Seatbelt applies the last matching rule, and so
//! does [`Policy::decide`], so the explanation is the rule that decided.
//!
//! The base, applied first: everything is readable, nothing is writable, and no Unix socket can be connected to or
//! created. The network is [`Network::Open`] (as nono leaves it) or [`Network::Filtered`]: only the listed ports and
//! the gate's, Piton's proxy, which decides named hosts (`network/`). TCP listening is [`Listen::Closed`] or open on
//! every address: Seatbelt can't limit it to this Mac (docs/pi-findings.md).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::policy::Source;
use crate::{
    agent::{GitControl, ProtectedRoots},
    config::{NetworkTarget, ServiceKey, ServiceName},
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
    /// Create a Unix socket (`bind`): listen on it. TCP listening is [`Listen`].
    Bind,
}

/// A rule's effect: the configuration's own (`"allow"`, `"deny"`).
pub use crate::config::Effect;

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
    /// Keychain Mach services allowed: a `"read"` entry covers the user's keychains. Decided after `services`.
    pub keychain: bool,
    /// The Mach services the sandbox may look up.
    pub services: Services,
    /// Whether processes may launch apps and send Apple Events.
    pub apps: Apps,
    /// TCP and UDP: open, or only what `[sandbox.network.connections]` allows (ports in Seatbelt, named hosts through
    /// the gate).
    pub network: Network,
    /// TCP listening, on every address or none.
    pub listen: Listen,
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
    /// Every service but the keychain's, from this entry: what an approved command runs with.
    Every { source: Source },
    /// Entries: the built-in list, the profile's and presets', and the user's. The most specific matching entry
    /// decides (an exact name over a longer prefix over a shorter one over `*`; a deny over an allow at the same).
    Listed { grants: Vec<ServiceGrant> },
}

/// A Mach service entry of the policy, its effect, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceGrant {
    pub key: ServiceKey,
    pub effect: Effect,
    pub source: Source,
}

/// The entry of `grants` that decides for `matches`: the most specific, a deny winning a tie.
fn deciding<T, K: Ord>(grants: &[T], matches: impl Fn(&T) -> bool, rank: impl Fn(&T) -> (K, u8)) -> Option<&T> {
    grants.iter().filter(|g| matches(g)).max_by_key(|g| rank(g))
}

/// A deny ranks over an allow at the same specificity.
fn effect_rank(effect: Effect) -> u8 {
    match effect {
        Effect::Allow => 0,
        Effect::Deny => 1,
    }
}

/// Launching apps (`lsopen`: `open`, Launch Services) and sending Apple Events (`osascript`, scripting another app).
/// What macOS starts this way runs outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Apps {
    /// Closed by the base: asked about.
    Closed,
    /// Open, from this entry.
    Open { source: Source },
    /// Closed by the user's `allow_launching_apps = false`: only logged.
    Denied { source: Source },
}

/// TCP listening (`listen()`, Seatbelt's `network-inbound`). Seatbelt can't tell a loopback listen from an
/// all-addresses one, or filter by port, so it's every address or none. UDP isn't covered: closing it would refuse DNS
/// replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Listen {
    /// Closed by the base: asked about.
    Closed,
    /// Open on every address, from this entry.
    Open { source: Source },
    /// Closed by the user's `allow_tcp_listening = false`: only logged.
    Denied { source: Source },
}

/// A connection entry of the policy, its effect, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkGrant {
    pub target: NetworkTarget,
    pub effect: Effect,
    pub source: Source,
}

impl NetworkGrant {
    fn rank(&self) -> ((u8, usize, u8), u8) {
        (self.target.specificity(), effect_rank(self.effect))
    }
}

/// A launch's network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Network {
    /// Every host and port, from this entry: nothing is emitted, nono's network stays open (an approved command).
    Open { source: Source },
    /// Direct connections, as the entries decide: `*` allows and no named host decides otherwise, so the sandbox's port
    /// rules are the whole of it and no gate runs.
    Direct { grants: Vec<NetworkGrant> },
    /// Direct connections only to the port entries; everything else through the gate, which decides named hosts.
    Filtered { grants: Vec<NetworkGrant>, gate: Gate },
}

/// The gate a filtered network reaches named hosts through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Gate {
    /// No gate: named hosts are unreachable (the environment tools' sandbox, `piton sandbox show`).
    None,
    /// Listening on `127.0.0.1:port`, for this sandbox's proxy user and credential. `ssh`: the folder holding the `ssh`
    /// wrapper that tunnels through the gate (first on the sandbox's `PATH`), when the daemon could make it.
    Listening {
        port: u16,
        user: String,
        credential: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh: Option<PathBuf>,
    },
}

impl Network {
    /// The proxy URL this sandbox's processes use and the `ssh` wrapper's folder, when it has a gate.
    pub fn proxy(&self) -> Option<(String, Option<&Path>)> {
        match self {
            Network::Open { .. } | Network::Direct { .. } | Network::Filtered { gate: Gate::None, .. } => None,
            Network::Filtered {
                gate:
                    Gate::Listening {
                        port,
                        user,
                        credential,
                        ssh,
                    },
                ..
            } => Some((
                crate::launch::sandbox::network::proxy_url(*port, user, credential),
                ssh.as_deref(),
            )),
        }
    }
}

/// A destination a process connects to, as the gate or the kernel names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// A host name or an IP address, lowercase, without brackets.
    pub host: String,
    pub port: u16,
}

impl Destination {
    /// `host:port`, `[v6]:port`.
    pub fn parse(text: &str) -> Option<Self> {
        let (host, port) = text.rsplit_once(':')?;
        let port = port.parse::<u16>().ok().filter(|p| *p != 0)?;
        let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        match host.is_empty() || host.contains(['/', ' ']) {
            true => None,
            false => Some(Self {
                host: host.to_ascii_lowercase(),
                port,
            }),
        }
    }

    /// Whether this names this Mac: `localhost`, a name under `.localhost`, or a loopback address.
    pub fn local(&self) -> bool {
        self.host == "localhost"
            || self.host.ends_with(".localhost")
            || self.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }
}

impl std::fmt::Display for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.host.contains(':') {
            true => write!(f, "[{}]:{}", self.host, self.port),
            false => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

impl NetworkTarget {
    /// Whether this entry allows connecting to `to`.
    pub fn allows(&self, to: &Destination) -> bool {
        match self {
            NetworkTarget::Anywhere => true,
            NetworkTarget::AnyHostPort { port } => *port == to.port,
            NetworkTarget::LocalAnyPort => to.local(),
            NetworkTarget::LocalPort { port } => to.local() && *port == to.port,
            NetworkTarget::Host { host, port } => {
                let host_matches = match host.strip_prefix("*.") {
                    Some(domain) => to
                        .host
                        .strip_suffix(domain)
                        .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
                    None => *host == to.host,
                };
                host_matches && port.is_none_or(|p| p == to.port)
            }
        }
    }
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

    /// The connection entry that decides `to`: the most specific that matches, a deny winning a tie. `None` under an
    /// open network, or when no entry matches.
    pub fn network_entry(&self, to: &Destination) -> Option<&NetworkGrant> {
        match &self.network {
            Network::Open { .. } => None,
            Network::Direct { grants } | Network::Filtered { grants, .. } => {
                deciding(grants, |g| g.target.allows(to), NetworkGrant::rank)
            }
        }
    }

    /// The decision for connecting to `to` by this policy's entries, or the base. A named host no entry allows may
    /// still pass the gate (pi's own hosts, the user's answer), which the gate decides after this.
    pub fn decide_network(&self, to: &Destination) -> Decision {
        if let Network::Open { source } = &self.network {
            return Decision {
                allowed: true,
                source: source.clone(),
            };
        }
        match self.network_entry(to) {
            Some(grant) => Decision {
                allowed: match grant.effect {
                    Effect::Allow => true,
                    Effect::Deny => false,
                },
                source: grant.source.clone(),
            },
            None => Decision {
                allowed: false,
                source: Source::BuiltIn {
                    what: CLOSED_NETWORK.into(),
                },
            },
        }
    }

    /// The TCP and UDP rules, after the socket rules. Direct: each port entry, least specific first. Filtered: every
    /// destination denied, then each port entry, then the gate's port. Named hosts emit nothing: Seatbelt can't name a
    /// host ("host must be * or localhost"), so only the gate decides them.
    fn network_rules(&self) -> Vec<String> {
        let (grants, gate) = match &self.network {
            Network::Open { .. } => return Vec::new(),
            Network::Direct { grants } => (grants, None),
            Network::Filtered { grants, gate } => (grants, Some(gate)),
        };
        let mut out = Vec::new();
        if gate.is_some() {
            out.push(r#"(deny network-outbound (remote ip "*:*"))"#.to_string());
        }
        let mut ports: Vec<&NetworkGrant> = grants.iter().collect();
        ports.sort_by_key(|g| g.rank());
        for grant in ports {
            let address = match &grant.target {
                NetworkTarget::LocalPort { port } => format!("localhost:{port}"),
                NetworkTarget::LocalAnyPort => "localhost:*".to_string(),
                NetworkTarget::AnyHostPort { port } => format!("*:{port}"),
                // `*` is the mode itself; a host is the gate's.
                NetworkTarget::Anywhere | NetworkTarget::Host { .. } => continue,
            };
            let effect = match grant.effect {
                Effect::Allow => "allow",
                Effect::Deny => "deny",
            };
            out.push(format!(r#"({effect} network-outbound (remote ip "{address}"))"#));
        }
        match gate {
            None | Some(Gate::None) => {}
            Some(Gate::Listening { port, .. }) => {
                out.push(format!(r#"(allow network-outbound (remote tcp "localhost:{port}"))"#));
            }
        }
        out
    }

    /// The decision for listening on TCP.
    pub fn decide_listen(&self) -> Decision {
        match &self.listen {
            Listen::Open { source } => Decision {
                allowed: true,
                source: source.clone(),
            },
            Listen::Denied { source } => Decision {
                allowed: false,
                source: source.clone(),
            },
            Listen::Closed => Decision {
                allowed: false,
                source: Source::BuiltIn {
                    what: CLOSED_LISTEN.into(),
                },
            },
        }
    }

    /// The listening rule, after every other network rule: closed refuses every TCP `listen()` (nono allows it); open
    /// emits nothing. `tcp`, not `ip`: an `ip` filter also refuses UDP replies, DNS included (docs/pi-findings.md).
    fn listen_rules(&self) -> Vec<String> {
        match &self.listen {
            Listen::Closed | Listen::Denied { .. } => vec![LISTEN_DENY.to_string()],
            Listen::Open { .. } => Vec::new(),
        }
    }

    /// The decision for a lookup of the Mach service `service`, as Seatbelt makes it: the keychain's decide last.
    pub fn decide_service(&self, service: &str) -> Decision {
        if KEYCHAIN_SERVICES.contains(&service) {
            return match self.keychain {
                true => Decision {
                    allowed: true,
                    source: Source::BuiltIn {
                        what: "the keychain: a `\"read\"` entry covers it".into(),
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
            Services::Listed { grants } => match deciding(
                grants,
                |g| g.key.matches(service),
                |g| (g.key.specificity(), effect_rank(g.effect)),
            ) {
                Some(grant) => Decision {
                    allowed: match grant.effect {
                        Effect::Allow => true,
                        Effect::Deny => false,
                    },
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
                // Least specific first: Seatbelt's last match decides, as `decide_service` does.
                out.push("(deny mach-lookup)".to_string());
                let mut sorted: Vec<&ServiceGrant> = grants.iter().collect();
                sorted.sort_by_key(|g| (g.key.specificity(), effect_rank(g.effect)));
                for grant in sorted {
                    let effect = match grant.effect {
                        Effect::Allow => "allow",
                        Effect::Deny => "deny",
                    };
                    out.push(match &grant.key {
                        ServiceKey::Every => format!("({effect} mach-lookup)"),
                        ServiceKey::Name(ServiceName::Exact(name)) => {
                            format!("({effect} mach-lookup (global-name \"{name}\"))")
                        }
                        ServiceKey::Name(ServiceName::Prefix(start)) => {
                            format!("({effect} mach-lookup (global-name-prefix \"{start}\"))")
                        }
                    });
                }
            }
        }
        match &self.apps {
            // The default refuses both; with a tag, its records carry it.
            Apps::Closed | Apps::Denied { .. } => {}
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
    /// With a tag, every deny carries it, and two leading rules label what no rule decides: a tagged
    /// `(deny default)`, which replaces the library's untagged default (a profile has one default action, and the last
    /// one wins: docs/pi-findings.md), so a refusal of any operation reaches its call; and a tagged
    /// `(deny file-write*)` for writes outside every grant. Neither shadows anything: the library grants no writes of its
    /// own for Piton's capability set, and every allow comes after them.
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
        let mut out = vec![tag("(deny default)".into()), tag("(deny file-write*)".into())];
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
        Ok(other
            .into_iter()
            .chain(base)
            .chain(network)
            .chain(self.network_rules())
            .chain(self.listen_rules())
            .collect())
    }
}

/// The rule that closes TCP listening.
const LISTEN_DENY: &str = r#"(deny network-inbound (local tcp "*:*"))"#;

/// Whether the kernel's refusal of `operation` on `target` is a TCP listen: `network-inbound local:*:<port>`.
pub fn is_listen(operation: &str, target: &str) -> bool {
    operation == "network-inbound" && target.starts_with("local:")
}

/// The base's Unix socket rules: no socket can be connected to or created. Every socket rule of the policy follows.
const SOCKET_BASE: [&str; 2] = [
    r#"(deny network-outbound (subpath "/"))"#,
    r#"(deny network-bind (subpath "/"))"#,
];

/// Why a socket connect is denied when no rule decides: what `piton sandbox why` prints.
const CLOSED_SOCKETS: &str = "Unix sockets are closed: a [sandbox.network.unix_socket_connections] entry opens one \
                              (global config.toml or a config.local.toml)";

/// Why a connection is denied when no entry allows it: what `piton sandbox why --connect` prints for a host.
const CLOSED_NETWORK: &str = "the network is filtered: a [sandbox.network.connections] entry opens a host or port \
                              (global config.toml or a config.local.toml)";

/// Why creating a socket is denied when no rule decides.
const CLOSED_BIND: &str = "creating Unix sockets is closed: a [sandbox.network.unix_socket_bindings] entry opens a path \
                           (global config.toml or a config.local.toml)";

/// Why listening on TCP is denied: what `piton sandbox why --listen` prints.
const CLOSED_LISTEN: &str = "TCP listening is closed: `allow_tcp_listening = true` opens it, on every address (global \
                             config.toml or a config.local.toml)";

/// Why a lookup of a service outside the list is denied: what `piton sandbox why --service` prints.
const CLOSED_SERVICES: &str = "Mach services outside the built-in list are closed: a [sandbox.macos.services] entry \
                               opens one (global config.toml or a config.local.toml)";

/// Why a lookup of a keychain service is denied.
const CLOSED_KEYCHAIN: &str = "the keychain is closed: `\"~/Library/Keychains\" = \"read\"` in [sandbox.filesystem] \
                               opens it";

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
            source: Source::BuiltIn { what: what.into() },
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
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
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
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
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
                    source: Source::Piton {
                        area: crate::launch::sandbox::PitonArea::AgentSocket,
                    },
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
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
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
        // The base's socket denies first, then the policy's socket rules in order, then closed listening.
        assert_eq!(
            &rules[first_network..],
            [
                r#"(deny network-outbound (subpath "/"))"#,
                r#"(deny network-bind (subpath "/"))"#,
                r#"(deny network-outbound (subpath "/s/\"q\""))"#,
                r#"(allow network-outbound (path "/s/agent.sock"))"#,
                r#"(allow network-bind (subpath "/w"))"#,
                r#"(deny network-inbound (local tcp "*:*"))"#,
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
        assert_eq!(tagged[0], format!("(deny default {message})"));
        assert_eq!(tagged[1], format!("(deny file-write* {message})"));
        assert_eq!(
            tagged[2],
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
                    key: ServiceKey::Name(name.clone()),
                    effect: Effect::Allow,
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
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
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
        assert!(!rules.iter().any(|r| r.contains("lsopen") || r.contains("appleevent")));
        assert!(
            !rules.iter().any(|r| r.starts_with("(deny default")),
            "untagged: nono's default only"
        );
        // Tagged: the default carries the tag, so closed apps' refusals do too; nono accepts it.
        let closed = Policy {
            tag: "t".into(),
            ..every.clone()
        };
        assert_eq!(
            closed.seatbelt().unwrap()[0],
            r#"(deny default (with message "piton-t"))"#
        );
        super::super::apply::capabilities(&closed).expect("nono accepts the tagged default");
        assert!(rules.contains(&r#"(allow mach-lookup (global-name "com.apple.secd"))"#.to_string()));
        assert!(every.decide_service("org.other").allowed);
        assert!(every.decide_service("com.apple.secd").allowed, "keychain read");

        // Tagged: the list's deny carries the tag, so a blocked lookup reaches the question.
        let tagged = Policy { tag: "t".into(), ..p }.seatbelt().unwrap();
        assert!(tagged.contains(&r#"(deny mach-lookup (with message "piton-t"))"#.to_string()));
    }

    fn grant(text: &str) -> NetworkGrant {
        effect_grant(text, Effect::Allow)
    }

    fn effect_grant(text: &str, effect: Effect) -> NetworkGrant {
        NetworkGrant {
            target: NetworkTarget::try_from(text.to_string()).unwrap(),
            effect,
            source: Source::Workspace,
        }
    }

    /// The most specific entry decides, a deny winning a tie; a direct network emits only its port entries, least
    /// specific first, so Seatbelt's last match agrees.
    #[test]
    fn the_most_specific_connection_entry_decides() {
        let direct = network_policy(Network::Direct {
            grants: vec![
                effect_grant("*", Effect::Allow),
                effect_grant("*:22", Effect::Deny),
                effect_grant("localhost:*", Effect::Allow),
            ],
        });
        let allowed = |p: &Policy, t: &str| p.decide_network(&to(t)).allowed;
        assert!(allowed(&direct, "example.com:443"));
        assert!(!allowed(&direct, "example.com:22"));
        assert!(
            allowed(&direct, "localhost:22"),
            "localhost:* is more specific than *:22"
        );
        let remote: Vec<String> = direct
            .seatbelt()
            .unwrap()
            .into_iter()
            .filter(|r| r.contains("remote"))
            .collect();
        assert_eq!(
            remote,
            [
                r#"(deny network-outbound (remote ip "*:22"))"#,
                r#"(allow network-outbound (remote ip "localhost:*"))"#,
            ]
        );
        super::super::apply::capabilities(&direct).expect("nono accepts it");

        let named = network_policy(Network::Filtered {
            grants: vec![
                effect_grant("*.example.com", Effect::Allow),
                effect_grant("bad.example.com", Effect::Deny),
                effect_grant("github.com", Effect::Allow),
                effect_grant("github.com:22", Effect::Deny),
            ],
            gate: Gate::None,
        });
        assert!(allowed(&named, "a.example.com:443"));
        assert!(!allowed(&named, "bad.example.com:443"));
        assert!(allowed(&named, "github.com:443"));
        assert!(!allowed(&named, "github.com:22"));
    }

    /// Services: an exact name over a prefix over `*`, a deny winning a tie; Seatbelt gets them least specific first.
    #[test]
    fn the_most_specific_service_entry_decides() {
        let entry = |key: &str, effect| ServiceGrant {
            key: ServiceKey::try_from(key.to_string()).unwrap(),
            effect,
            source: Source::Workspace,
        };
        let p = services_policy(
            Services::Listed {
                grants: vec![
                    entry("com.example.agent", Effect::Allow),
                    entry("*", Effect::Allow),
                    entry("com.example.*", Effect::Deny),
                ],
            },
            Apps::Closed,
            false,
        );
        assert!(p.decide_service("org.other").allowed);
        assert!(!p.decide_service("com.example.x").allowed);
        assert!(p.decide_service("com.example.agent").allowed);
        let rules = p.seatbelt().unwrap();
        let at = |r: &str| rules.iter().position(|x| x == r).unwrap_or_else(|| panic!("{r}"));
        assert!(at("(allow mach-lookup)") < at(r#"(deny mach-lookup (global-name-prefix "com.example."))"#));
        assert!(
            at(r#"(deny mach-lookup (global-name-prefix "com.example."))"#)
                < at(r#"(allow mach-lookup (global-name "com.example.agent"))"#)
        );
        super::super::apply::capabilities(&p).expect("nono accepts it");
    }

    fn network_policy(network: Network) -> Policy {
        Policy {
            network,
            ..services_policy(
                Services::Every {
                    source: Source::Workspace,
                },
                Apps::Closed,
                false,
            )
        }
    }

    /// Closed listening denies every TCP listen after every other network rule (tagged with the launch's tag); open emits
    /// nothing, leaving nono's allow. Only a listen's record counts as one.
    #[test]
    fn listening_in_seatbelt() {
        let closed = network_policy(Network::Filtered {
            grants: vec![grant("localhost:*")],
            gate: Gate::None,
        });
        assert_eq!(closed.listen, Listen::Closed);
        let rules = closed.seatbelt().unwrap();
        assert_eq!(rules.last().map(String::as_str), Some(LISTEN_DENY));
        assert!(!closed.decide_listen().allowed);
        assert_eq!(
            closed.decide_listen().source,
            Source::BuiltIn {
                what: CLOSED_LISTEN.into()
            }
        );
        super::super::apply::capabilities(&closed).expect("nono accepts it");
        let tagged = Policy {
            tag: "t".into(),
            ..closed.clone()
        }
        .seatbelt()
        .unwrap();
        assert_eq!(
            tagged.last().map(String::as_str),
            Some(r#"(deny network-inbound (local tcp "*:*") (with message "piton-t"))"#)
        );

        let open = Policy {
            listen: Listen::Open {
                source: Source::Workspace,
            },
            ..closed
        };
        assert!(!open.seatbelt().unwrap().iter().any(|r| r.contains("network-inbound")));
        assert!(open.decide_listen().allowed);
        assert_eq!(open.decide_listen().source, Source::Workspace);

        assert!(is_listen("network-inbound", "local:*:50958"));
        assert!(!is_listen("network-inbound", "remote:*:53"));
        assert!(!is_listen("network-bind", "local:*:1"));
    }

    fn to(text: &str) -> Destination {
        Destination::parse(text).unwrap_or_else(|| panic!("{text}"))
    }

    /// An open network emits nothing of its own; a filtered one denies every destination after the socket rules, then
    /// allows each port entry and the gate's port, in order. Named hosts emit nothing: only the gate reaches them.
    #[test]
    fn a_filtered_network_in_seatbelt() {
        let open = network_policy(Network::Open {
            source: Source::Workspace,
        });
        assert!(!open.seatbelt().unwrap().iter().any(|r| r.contains("remote")));

        let filtered = network_policy(Network::Filtered {
            grants: ["localhost:5432", "localhost:*", "*:22", "github.com", "*.npmjs.org:443"]
                .map(grant)
                .to_vec(),
            gate: Gate::Listening {
                port: 60001,
                user: "pi".into(),
                credential: "c".into(),
                ssh: None,
            },
        });
        let rules = filtered.seatbelt().unwrap();
        let remote: Vec<&str> = rules
            .iter()
            .filter(|r| r.contains("remote"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            remote,
            [
                r#"(deny network-outbound (remote ip "*:*"))"#,
                // Least specific first.
                r#"(allow network-outbound (remote ip "*:22"))"#,
                r#"(allow network-outbound (remote ip "localhost:*"))"#,
                r#"(allow network-outbound (remote ip "localhost:5432"))"#,
                r#"(allow network-outbound (remote tcp "localhost:60001"))"#,
            ]
        );
        let socket_base = rules.iter().position(|r| r.contains(r#"(subpath "/")"#)).unwrap();
        let deny_all = rules.iter().position(|r| r.contains(r#"remote ip "*:*""#)).unwrap();
        assert!(socket_base < deny_all, "after the socket rules");
        super::super::apply::capabilities(&filtered).expect("nono accepts it");

        // Tagged: the deny carries the launch's tag, so a direct connection's refusal reaches its call.
        let tagged = Policy {
            tag: "t".into(),
            ..filtered
        }
        .seatbelt()
        .unwrap();
        assert!(tagged.contains(&r#"(deny network-outbound (remote ip "*:*") (with message "piton-t"))"#.to_string()));

        // No gate: no gate port.
        let none = network_policy(Network::Filtered {
            grants: Vec::new(),
            gate: Gate::None,
        });
        let rules = none.seatbelt().unwrap();
        assert!(rules.iter().any(|r| r.contains(r#"remote ip "*:*""#)));
        assert!(!rules.iter().any(|r| r.contains("remote tcp")));
    }

    /// `decide_network` by entry: hosts exactly or by subdomain, on any or the listed port; ports on any host; local
    /// ports for this Mac's names and loopback addresses only.
    #[test]
    fn network_entries_decide_destinations() {
        let policy = network_policy(Network::Filtered {
            grants: ["github.com:22", "*.npmjs.org", "10.0.0.5", "*:5432", "localhost:3000"]
                .map(grant)
                .to_vec(),
            gate: Gate::None,
        });
        let allowed = |t: &str| policy.decide_network(&to(t)).allowed;
        assert!(allowed("github.com:22"));
        assert!(!allowed("github.com:443"), "only the listed port");
        assert!(!allowed("api.github.com:22"), "exact host");
        assert!(allowed("registry.npmjs.org:443"));
        assert!(allowed("a.b.npmjs.org:80"));
        assert!(
            !allowed("npmjs.org:443"),
            "a subdomain pattern doesn't cover the domain itself"
        );
        assert!(!allowed("evilnpmjs.org:443"));
        assert!(allowed("10.0.0.5:8080"));
        assert!(allowed("db.example.com:5432"), "a port on any host");
        assert!(allowed("localhost:3000"));
        assert!(allowed("127.0.0.1:3000"));
        assert!(allowed("[::1]:3000"));
        assert!(!allowed("localhost:3001"));
        assert!(
            !allowed("example.com:3000"),
            "a local port isn't a port on another host"
        );
        assert!(!allowed("example.com:443"));
        assert_eq!(
            policy.decide_network(&to("example.com:443")).source,
            Source::BuiltIn {
                what: CLOSED_NETWORK.into()
            }
        );

        let open = network_policy(Network::Open {
            source: Source::Workspace,
        });
        assert!(open.decide_network(&to("example.com:443")).allowed);

        // Destinations as the gate and the kernel name them.
        assert_eq!(to("GitHub.com:22").host, "github.com");
        assert_eq!(to("[2001:db8::1]:443").to_string(), "[2001:db8::1]:443");
        for bad in ["github.com", "github.com:0", ":443", "/tmp/x.sock", "a b:1"] {
            assert!(Destination::parse(bad).is_none(), "{bad}");
        }
    }
}
