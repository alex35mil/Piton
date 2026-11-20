//! What a command the user approved runs with: the launch's policy widened by a per-command grant (`all`, or the
//! `[sandbox]` lists of a narrowed one), never past the floor.
//!
//! The floor is what no approval lifts: protected files, every configuration level and pi's agent directory (what a
//! later pi or Piton loads, so a lifted command could grant the agent more), the daemon's state and control socket, the
//! user's own `deny`, and the credential denies while fnox is on (the sandbox could run `fnox get`). Git's hooks and
//! configuration and the credential denies otherwise are liftable: `husky install` and Git's keychain sign-in work once
//! approved.

use std::path::{Path, PathBuf};

use super::{
    policy::Source,
    rules::{Access, Effect, Policy, Rule, Target},
};
use crate::config::{AllGrant, CommandGrant, Grant, GrantLists, LaunchConfig, Listed};

/// The policies a session's approved commands run with, resolved at launch: `all`, and each `[[sandbox.commands]]`
/// entry's.
#[derive(Debug, Clone)]
pub struct CommandPolicies {
    /// The launch's own policy: what a process block is judged against.
    pub launch: Policy,
    /// Everything except the floor: "Allow once" and `grant = "all"`.
    pub all: Policy,
    /// Whether credentials are floor (fnox is on).
    pub credentials: Credentials,
    /// Each entry in matching order, with its policy, or why it couldn't be resolved (a narrowed grant naming a `$VAR`
    /// that isn't set).
    pub entries: Vec<(Listed<CommandGrant>, Result<Policy, String>)>,
}

impl CommandPolicies {
    /// Tags every policy with the launch's tag, as pi's own policy is.
    pub fn set_tag(&mut self, tag: &str) {
        self.launch.tag = tag.to_string();
        self.all.tag = tag.to_string();
        for (_, policy) in &mut self.entries {
            if let Ok(policy) = policy {
                policy.tag = tag.to_string();
            }
        }
    }

    /// The policy for a matched entry.
    pub fn of(&self, entry: &Listed<CommandGrant>) -> Result<&Policy, String> {
        self.entries.iter().find(|(e, _)| e == entry).map_or_else(
            || Err("the grant changed since the session started; restart the session".into()),
            |(_, policy)| policy.as_ref().map_err(Clone::clone),
        )
    }
}

/// The configuration a narrowed grant resolves with: the launch's, with the grant's lists added as if the file that
/// lists the command listed them.
pub fn widened_config(config: &LaunchConfig, lists: &GrantLists, file: &Path) -> LaunchConfig {
    let mut config = config.clone();
    config.sandbox.add(
        crate::config::SandboxTable {
            write: lists.write.clone(),
            write_file: lists.write_file.clone(),
            read: lists.read.clone(),
            connect: lists.connect.clone(),
            presets: lists.presets.clone(),
            ..crate::config::SandboxTable::default()
        },
        file,
    );
    config
}

/// The policies for every entry of `config`: `all` from `policy`, and each narrowed grant from `resolve` (the launch's
/// resolution with a widened configuration).
pub fn command_policies(
    config: &LaunchConfig,
    policy: &Policy,
    credentials: Credentials,
    resolve: impl Fn(&LaunchConfig) -> Result<Policy, String>,
) -> CommandPolicies {
    let widened = all(policy, credentials);
    let entries = config
        .sandbox
        .commands
        .iter()
        .map(|entry| {
            let policy = match &entry.value.grant {
                Grant::All(AllGrant::All) => Ok(widened.clone()),
                Grant::Lists(lists) => resolve(&widened_config(config, lists, &entry.file)),
            };
            (entry.clone(), policy)
        })
        .collect();
    CommandPolicies {
        launch: policy.clone(),
        all: widened,
        credentials,
        entries,
    }
}

/// Whether the credential denies are part of the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credentials {
    /// fnox is off for the launch: an approved command may read credentials (Git's keychain sign-in).
    Liftable,
    /// fnox is on: the keychain holds its secret sources' credentials.
    Floor,
}

/// The launch's `policy` with everything lifted except the floor: what `grant = "all"` and \"Allow once\" run with.
///
/// Every write and connect is allowed first, then the launch's rules follow with every liftable deny left out, so each
/// floor deny (and the allows that reopen part of it, such as the daemon's logs and `agent.sock`) still decides last.
pub fn all(policy: &Policy, credentials: Credentials) -> Policy {
    let everything = |access: Access| Rule {
        effect: Effect::Allow,
        access: vec![access],
        target: Target::Subpath { path: "/".into() },
        source: Source::BuiltIn {
            what: "a command the user approved: everything except the floor".into(),
        },
    };
    let mut rules = vec![everything(Access::Write), everything(Access::Connect)];
    rules.extend(policy.rules.iter().filter(|r| kept(r, credentials)).cloned());
    Policy {
        rules,
        keychain: match credentials {
            Credentials::Liftable => true,
            Credentials::Floor => policy.keychain,
        },
        network: policy.network.clone(),
        tag: policy.tag.clone(),
    }
}

/// Whether `rule` stays in a widened policy: every allow does, and a deny only when it's floor.
fn kept(rule: &Rule, credentials: Credentials) -> bool {
    match rule.effect {
        Effect::Allow => true,
        Effect::Deny => floor(rule, credentials),
    }
}

/// Whether a deny is part of the floor.
pub fn floor(rule: &Rule, credentials: Credentials) -> bool {
    match &rule.target {
        Target::Protected { .. } => true,
        Target::GitControl { .. } => false,
        Target::Subpath { .. } | Target::Literal { .. } | Target::Prefix { .. } => match &rule.source {
            Source::NonoGroup { .. } => match credentials {
                Credentials::Liftable => false,
                Credentials::Floor => true,
            },
            // Configuration levels, pi's agent directories, the daemon's state and control socket.
            Source::Piton { .. } => true,
            // The user's own `deny`.
            Source::Config { .. } => true,
            // No such deny exists; kept, so a new one is closed until it's classified here.
            Source::Workspace | Source::BuiltIn { .. } | Source::AgentDir | Source::Preset { .. } => true,
        },
    }
}

/// The floor deny that decides `access` to `path` under `policy`, if any: what a refusal can't be offered for.
pub fn floor_decides(policy: &Policy, path: &Path, access: Access, credentials: Credentials) -> bool {
    policy
        .rules
        .iter()
        .rev()
        .find(|r| r.access.contains(&access) && r.target.covers(path))
        .is_some_and(|r| match r.effect {
            Effect::Allow => false,
            Effect::Deny => floor(r, credentials),
        })
}

/// The `[sandbox]` entry that lifts a refusal of a process that isn't a bash call (an MCP server, an extension, pi):
/// the narrowest that covers it. Persisted as the user's own entry, so it applies to the whole session after a
/// restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lift {
    /// `write_file = [path]`.
    WriteFile(PathBuf),
    /// `read = [path]`: lifts a credential deny.
    Read(PathBuf),
    /// `connect = [path]`: a Unix socket.
    Connect(PathBuf),
    /// `read = ["~/Library/Keychains"]`: reopens the keychain's services.
    Keychain,
}

impl Lift {
    /// The key and value as written in `[sandbox]`.
    pub fn entry(&self) -> (&'static str, String) {
        match self {
            Lift::WriteFile(path) => ("write_file", path.display().to_string()),
            Lift::Read(path) => ("read", path.display().to_string()),
            Lift::Connect(path) => ("connect", path.display().to_string()),
            Lift::Keychain => ("read", "~/Library/Keychains".into()),
        }
    }
}

/// Why a refusal can't be offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoLift {
    /// The floor, or the user's own `deny`.
    Floor,
    /// An operation no `[sandbox]` entry lifts (process-exec of a denied file, a Mach service other than the keychain's).
    Unsupported,
}

/// The lift for a refusal of `operation` on `target` (as the kernel names them) under `policy`.
pub fn lift_for(policy: &Policy, operation: &str, target: &str, credentials: Credentials) -> Result<Lift, NoLift> {
    let path = || Path::new(target).is_absolute().then(|| PathBuf::from(target));
    let (lift, access) = match operation {
        op if op.starts_with("file-write") => (path().map(Lift::WriteFile), Access::Write),
        op if op.starts_with("file-read") => (path().map(Lift::Read), Access::Read),
        "network-outbound" => (path().map(Lift::Connect), Access::Connect),
        "mach-lookup" if KEYCHAIN_TARGETS.contains(&target) => {
            return match credentials {
                Credentials::Liftable => Ok(Lift::Keychain),
                Credentials::Floor => Err(NoLift::Floor),
            };
        }
        _ => return Err(NoLift::Unsupported),
    };
    let lift = lift.ok_or(NoLift::Unsupported)?;
    match floor_decides(policy, Path::new(target), access, credentials) {
        true => Err(NoLift::Floor),
        false => Ok(lift),
    }
}

/// The Mach services that broker keychain access (the library denies them).
const KEYCHAIN_TARGETS: [&str; 5] = [
    "com.apple.SecurityServer",
    "com.apple.securityd",
    "com.apple.security.keychaind",
    "com.apple.secd",
    "com.apple.security.agent",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::GitControl,
        launch::sandbox::policy::{Provenance, Source},
    };

    fn deny(access: &[Access], path: &str, source: Source) -> Rule {
        Rule {
            effect: Effect::Deny,
            access: access.to_vec(),
            target: Target::Subpath { path: path.into() },
            source,
        }
    }

    fn piton(what: &str) -> Source {
        Source::Piton { what: what.into() }
    }

    /// A launch policy with one rule of each kind the floor decides about.
    fn launch() -> Policy {
        let all = [Access::Read, Access::Write, Access::Connect];
        Policy {
            rules: vec![
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Write],
                    target: Target::Subpath { path: "/w".into() },
                    source: Source::Workspace,
                },
                deny(&all, "/h/.ssh", Source::NonoGroup { group: "ssh".into() }),
                deny(
                    &all,
                    "/h/Library/Keychains",
                    Source::NonoGroup {
                        group: "keychain".into(),
                    },
                ),
                deny(&[Access::Write], "/h/.pi/agent", piton("pi's agent directory")),
                deny(
                    &all,
                    "/h/secret",
                    Source::Config {
                        key: "sandbox.deny".into(),
                        from: Provenance::File {
                            path: "/g/config.toml".into(),
                        },
                    },
                ),
                deny(&[Access::Write], "/h/.config/piton", piton("a configuration level")),
                Rule {
                    effect: Effect::Deny,
                    access: vec![Access::Write],
                    target: Target::Protected {
                        source: "/w".into(),
                        checkout: "/w".into(),
                    },
                    source: piton("a protected file"),
                },
                Rule {
                    effect: Effect::Deny,
                    access: vec![Access::Write],
                    target: Target::GitControl {
                        control: GitControl::of(&[Path::new("/w")]),
                    },
                    source: piton("Git's hooks and configuration"),
                },
                deny(&all, "/h/.local/state/piton", piton("the daemon's state folder")),
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Connect],
                    target: Target::Literal {
                        path: "/h/.local/state/piton/agent.sock".into(),
                    },
                    source: piton("the agent socket"),
                },
            ],
            keychain: false,
            network: Vec::new(),
            tag: String::new(),
        }
    }

    /// `all` opens everything the launch closes except the floor, which still decides wherever it applies.
    #[test]
    fn all_keeps_the_floor() {
        let launch = launch();
        let widened = all(&launch, Credentials::Liftable);
        let allowed = |p: &str, a| widened.decide(Path::new(p), a).allowed;
        // Lifted.
        assert!(allowed("/h/.zshrc", Access::Write), "outside every grant");
        assert!(allowed("/h/.ssh/id_ed25519", Access::Read), "credentials, fnox off");
        assert!(
            allowed("/w/.git/hooks/pre-commit", Access::Write),
            "Git's control files"
        );
        assert!(allowed("/w/.git/config", Access::Write));
        assert!(allowed("/tmp/sock", Access::Connect));
        assert!(widened.keychain);
        // The floor.
        assert!(!allowed("/w/.pi/settings.json", Access::Write), "a protected file");
        assert!(
            !allowed("/h/.config/piton/config.toml", Access::Write),
            "a configuration level"
        );
        assert!(
            !allowed("/h/.pi/agent/extensions/x.ts", Access::Write),
            "pi's agent directory"
        );
        assert!(!allowed("/h/secret/x", Access::Read), "the user's deny");
        assert!(
            !allowed("/h/.local/state/piton/piton.sock", Access::Connect),
            "the daemon's state"
        );
        assert!(
            allowed("/h/.local/state/piton/agent.sock", Access::Connect),
            "agent.sock stays reachable"
        );
        // Nothing the launch allows is lost.
        for (path, access) in [("/w/x", Access::Write), ("/etc/hosts", Access::Read)] {
            assert!(launch.decide(Path::new(path), access).allowed);
            assert!(allowed(path, access));
        }
    }

    /// With fnox on, the credential denies are floor too.
    #[test]
    fn credentials_are_floor_behind_fnox() {
        let widened = all(&launch(), Credentials::Floor);
        assert!(!widened.decide(Path::new("/h/.ssh/id_ed25519"), Access::Read).allowed);
        assert!(
            !widened
                .decide(Path::new("/h/Library/Keychains/login.keychain-db"), Access::Read)
                .allowed
        );
        assert!(!widened.keychain);
        assert!(widened.decide(Path::new("/h/.zshrc"), Access::Write).allowed);
    }

    /// Which deny decides a refusal: the floor's can't be offered.
    #[test]
    fn floor_decides_names_the_unliftable() {
        let launch = launch();
        let floor = |p: &str, a, c| floor_decides(&launch, Path::new(p), a, c);
        assert!(floor("/w/.pi/x", Access::Write, Credentials::Liftable));
        assert!(floor("/h/secret/x", Access::Read, Credentials::Liftable));
        assert!(!floor("/h/.ssh/x", Access::Read, Credentials::Liftable));
        assert!(floor("/h/.ssh/x", Access::Read, Credentials::Floor));
        assert!(!floor("/w/.git/config", Access::Write, Credentials::Liftable));
        assert!(
            !floor("/h/.zshrc", Access::Write, Credentials::Liftable),
            "the base, not a deny"
        );
        assert!(!floor(
            "/h/.local/state/piton/agent.sock",
            Access::Connect,
            Credentials::Floor
        ));
    }

    /// The narrowest entry per operation; the floor and unsupported operations are never offered.
    #[test]
    fn lifts_for_refusals() {
        let launch = launch();
        let lift = |op: &str, target: &str, c| lift_for(&launch, op, target, c);
        use Credentials::{Floor, Liftable};
        assert_eq!(
            lift("file-write-create", "/h/.cache/x", Liftable),
            Ok(Lift::WriteFile("/h/.cache/x".into()))
        );
        assert_eq!(
            lift("file-write-data", "/w/.git/config", Liftable),
            Ok(Lift::WriteFile("/w/.git/config".into()))
        );
        assert_eq!(
            lift("file-read-data", "/h/.ssh/config", Liftable),
            Ok(Lift::Read("/h/.ssh/config".into()))
        );
        assert_eq!(lift("file-read-data", "/h/.ssh/config", Floor), Err(NoLift::Floor));
        assert_eq!(
            lift("network-outbound", "/tmp/x.sock", Liftable),
            Ok(Lift::Connect("/tmp/x.sock".into()))
        );
        assert_eq!(
            lift("mach-lookup", "com.apple.SecurityServer", Liftable),
            Ok(Lift::Keychain)
        );
        assert_eq!(
            lift("mach-lookup", "com.apple.SecurityServer", Floor),
            Err(NoLift::Floor)
        );
        assert_eq!(
            lift("mach-lookup", "com.apple.diagnosticd", Liftable),
            Err(NoLift::Unsupported)
        );
        assert_eq!(
            lift("file-write-create", "/w/.pi/settings.json", Liftable),
            Err(NoLift::Floor)
        );
        assert_eq!(lift("file-read-data", "/h/secret/x", Liftable), Err(NoLift::Floor));
        assert_eq!(
            lift("user-preference-write", "com.x", Liftable),
            Err(NoLift::Unsupported)
        );
        assert_eq!(Lift::Keychain.entry(), ("read", "~/Library/Keychains".to_string()));
    }
}
