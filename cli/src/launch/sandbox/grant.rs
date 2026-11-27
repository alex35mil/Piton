//! What a command the user approved runs with: the launch's policy widened by a per-command grant (`all`, or the
//! `[sandbox]` entries of a narrowed one), never past the floor.
//!
//! The floor is what no approval lifts: protected files, every configuration level and pi's agent directory (what a
//! later pi or Piton loads, so a lifted command could grant the agent more), the daemon's state and control socket, the
//! user's own `"deny"` and `"read"` entries, and the credential denies while fnox is on (the sandbox could run `fnox get`). Git's hooks and
//! configuration and the credential denies otherwise are liftable: `husky install` and Git's keychain sign-in work once
//! approved. So are Mach services, launching apps, and TCP listening: an approved `open https://…` opens the page (what
//! `open` may name is checked before the question, `agent/open.rs`).

use std::path::{Path, PathBuf};

use super::{
    policy::{self, PitonArea, Source},
    rules::{Access, Apps, Effect, Listen, Network, Policy, Rule, ServiceGrant, Services, Target, is_listen},
};
use crate::config::{
    AllGrant, CommandGrant, CommandRuntime, FileAccess, Grant, GrantEntries, LaunchConfig, Listed, NetworkTarget,
    Origin, PathKey, ServiceKey, ServiceName,
};

/// The policies a session's approved commands run with, resolved at launch: `all`, and each `[[sandbox.commands]]`
/// entry's.
#[derive(Debug, Clone)]
pub struct CommandPolicies {
    /// The launch's own policy: what a process block is judged against.
    pub launch: Policy,
    /// Everything except the floor: "Allow once" in the sandbox and `grant = "all"`.
    pub all: Policy,
    /// Each entry in matching order, with where it runs.
    pub entries: Vec<(Listed<CommandGrant>, EntryRuntime)>,
    /// The launch's resolution with a widened configuration: what a process block's entry is checked against.
    pub resolver: Resolver,
    /// `[sandbox] silence` and the presets' entries: the blocks of processes they match are only logged.
    pub silence: Vec<super::Silenced>,
    /// Whether the credential denies are floor (fnox on): what a keychain block says.
    pub credentials: Credentials,
    /// How the `pi-claude-code` preset signs `claude` in, when the preset is on: what a keychain block of `claude`
    /// suggests.
    pub claude_login: Option<crate::config::ClaudeLogin>,
    /// The variables of the launch's environment a file the agent could write set: what the block question shows on
    /// its host side.
    pub writable_vars: Vec<crate::launch::env::WritableVars>,
}

/// Resolves a launch's configuration again, with the launch's own facts (its environment, its fnox files): the policy
/// a restart with that configuration gets.
#[derive(Clone)]
pub struct Resolver {
    pub config: LaunchConfig,
    pub(crate) resolve: std::sync::Arc<ResolveFn>,
}

/// How a [`Resolver`] resolves a configuration.
type ResolveFn = dyn Fn(&LaunchConfig) -> Result<Policy, String> + Send + Sync;

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish_non_exhaustive()
    }
}

impl Resolver {
    pub fn new(
        config: LaunchConfig,
        resolve: impl Fn(&LaunchConfig) -> Result<Policy, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            config,
            resolve: std::sync::Arc::new(resolve),
        }
    }

    /// The launch's policy with `entries` added as if `file` (the user's own) held them.
    pub fn with(&self, entries: &GrantEntries, file: &Path) -> Result<Policy, String> {
        (self.resolve)(&widened_config(&self.config, entries, file))
    }
}

impl CommandPolicies {
    /// Tags every policy with the launch's tag, as pi's own policy is.
    pub fn set_tag(&mut self, tag: &str) {
        self.launch.tag = tag.to_string();
        self.all.tag = tag.to_string();
        for (_, runtime) in &mut self.entries {
            match runtime {
                EntryRuntime::Sandbox(Ok(policy)) => policy.tag = tag.to_string(),
                EntryRuntime::Sandbox(Err(_)) | EntryRuntime::Host => {}
            }
        }
    }

    /// Where a matched entry runs.
    pub fn of(&self, entry: &Listed<CommandGrant>) -> Result<&EntryRuntime, String> {
        self.entries.iter().find(|(e, _)| e == entry).map_or_else(
            || Err("the grant changed since the session started; restart the session".into()),
            |(_, runtime)| Ok(runtime),
        )
    }
}

/// Where a `[[sandbox.commands]]` entry's command runs, resolved at launch.
#[derive(Debug, Clone)]
pub enum EntryRuntime {
    /// `runtime = "host"`.
    Host,
    /// `runtime = "sandbox"`: its policy, or why it couldn't be resolved (a narrowed grant naming a `$VAR` that isn't
    /// set).
    Sandbox(Result<Box<Policy>, String>),
}

/// The configuration a narrowed grant resolves with: the launch's, with the grant's entries added as the nearest file,
/// the one that lists the command (commands are the user's own, so it is too).
pub fn widened_config(config: &LaunchConfig, entries: &GrantEntries, file: &Path) -> LaunchConfig {
    let mut config = config.clone();
    config.sandbox.add(entries.table(), file, Origin::UsersOwn);
    config
}

/// The policies for every entry of the resolver's configuration: `all` from `policy`, and each narrowed grant resolved
/// again with its lists.
pub fn command_policies(
    policy: &Policy,
    credentials: Credentials,
    resolver: Resolver,
    silence: Vec<super::Silenced>,
    claude_login: Option<crate::config::ClaudeLogin>,
    writable_vars: Vec<crate::launch::env::WritableVars>,
) -> CommandPolicies {
    let widened = all(policy, credentials);
    let entries = resolver
        .config
        .sandbox
        .commands
        .iter()
        .map(|entry| {
            let runtime = match &entry.value.runtime {
                CommandRuntime::Host => EntryRuntime::Host,
                CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => {
                    EntryRuntime::Sandbox(Ok(Box::new(widened.clone())))
                }
                CommandRuntime::Sandbox(Grant::Entries(entries)) => {
                    EntryRuntime::Sandbox(resolver.with(entries, &entry.file).map(Box::new))
                }
            };
            (entry.clone(), runtime)
        })
        .collect();
    CommandPolicies {
        launch: policy.clone(),
        all: widened,
        entries,
        resolver,
        silence,
        credentials,
        claude_login,
        writable_vars,
    }
}

/// Whether the credential denies are part of the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credentials {
    /// fnox is off for the launch: an approved command may read credentials (Git's keychain sign-in).
    Liftable,
    /// fnox is on: the keychain holds its secret sources' credentials.
    Floor,
}

/// The launch's `policy` with everything lifted except the floor: what `grant = "all"` and \"Allow once\" run with.
///
/// Every write, connect, and bind is allowed first, then the launch's rules follow with every liftable deny left out, so each
/// floor deny (and the allows that reopen part of it, such as the daemon's logs and `agent.sock`) still decides last.
/// Every Mach service and launching apps are open; the keychain's services stay closed behind fnox.
///
/// Writes open below each entry of the root ([`root_entries`]), never `/` itself: nono refuses a platform rule that
/// grants `(allow file-write* (subpath "/"))`. The root's own entries (`/Users`, `/tmp`) stay as the launch decides:
/// nothing a command needs is created there.
pub fn all(policy: &Policy, credentials: Credentials) -> Policy {
    all_below(policy, credentials, &root_entries())
}

/// [`all`], with writes opened below each of `roots`.
fn all_below(policy: &Policy, credentials: Credentials, roots: &[PathBuf]) -> Policy {
    let source = || Source::BuiltIn {
        what: "a command the user approved: everything except the floor".into(),
    };
    let mut rules: Vec<Rule> = roots
        .iter()
        .map(|root| Rule {
            effect: Effect::Allow,
            access: vec![Access::Write],
            target: Target::Subpath { path: root.clone() },
            source: source(),
        })
        .collect();
    // nono validates file rules only: a socket allow on `/` is accepted (yolo's `"/" = "allow"` Unix socket entry emits
    // it too).
    rules.push(Rule {
        effect: Effect::Allow,
        access: vec![Access::Connect, Access::Bind],
        target: Target::Subpath { path: "/".into() },
        source: source(),
    });
    rules.extend(policy.rules.iter().filter(|r| kept(r, credentials)).cloned());
    // Every service, except what the user's own entries deny.
    let user_denies: Vec<ServiceGrant> = match &policy.services {
        Services::Every { .. } => Vec::new(),
        Services::Listed { grants } => grants
            .iter()
            .filter(|g| g.effect == Effect::Deny && matches!(g.source, Source::Config { .. }))
            .cloned()
            .collect(),
    };
    let services = match user_denies.is_empty() {
        true => Services::Every { source: source() },
        false => Services::Listed {
            grants: std::iter::once(ServiceGrant {
                key: ServiceKey::Every,
                effect: Effect::Allow,
                source: source(),
            })
            .chain(user_denies)
            .collect(),
        },
    };
    Policy {
        rules,
        keychain: match credentials {
            Credentials::Liftable => true,
            Credentials::Floor => policy.keychain,
        },
        services,
        // The user's own `false` stays: only a host run passes it.
        apps: match &policy.apps {
            Apps::Denied { source } => Apps::Denied { source: source.clone() },
            Apps::Closed | Apps::Open { .. } => Apps::Open { source: source() },
        },
        // The network isn't floor: an approved command reaches any host directly, without the gate.
        network: Network::Open { source: source() },
        listen: match &policy.listen {
            Listen::Denied { source } => Listen::Denied { source: source.clone() },
            Listen::Closed | Listen::Open { .. } => Listen::Open { source: source() },
        },
        tag: policy.tag.clone(),
        // An approved command's sandbox is never given a token.
        lifts: super::rules::Lifts::Off,
    }
}

/// The entries of `/`, each with its real path too when it's a link (`/tmp` is `/private/tmp`), as Seatbelt compares real
/// paths. Read when a launch's policies are built.
fn root_entries() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir("/").into_iter().flatten().flatten();
    for path in entries.map(|e| e.path()) {
        let real = std::fs::canonicalize(&path).ok().filter(|r| *r != path);
        for candidate in std::iter::once(path).chain(real) {
            if candidate.parent().is_some() && !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
    }
    roots.sort();
    roots
}

/// Whether `rule` stays in a widened policy: every allow does, and a deny only when it's floor.
fn kept(rule: &Rule, credentials: Credentials) -> bool {
    match rule.effect {
        Effect::Allow => true,
        Effect::Deny => match liftability(rule, credentials) {
            Liftability::Floor(_) => true,
            Liftability::Liftable(_) => false,
        },
    }
}

/// Whether the user's approval can lift a deny: the one classification every widening and every block report reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liftability {
    /// No approval lifts it; only the user's own edit, or for some items a host run, gets past it.
    Floor(FloorItem),
    /// An approval lifts it.
    Liftable(LiftableDeny),
}

/// The floor: what no approval lifts from a sandbox, each for its reason (docs/design.md, "The floor").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorItem {
    /// The daemon's control socket: a sandbox that reached it could approve its own requests and start host sessions.
    ControlSocket,
    /// The daemon's state folder: it holds trust decisions and the agent socket.
    StateFolder,
    /// Configuration levels, pi's agent directories, protected files, and the folders on their way: they decide what
    /// later sessions may do.
    Configuration,
    /// A file fnox loads, or a folder on its path: one edit could send secrets anywhere.
    FnoxFiles,
    /// A socket an entry allows, or a folder on its way: kept in place so no other socket is moved there.
    AllowedSocket,
    /// The user's own `"deny"`, `"read"`, or `false`.
    UserSwitch,
    /// A credential while fnox is on: the sandbox could run `fnox get`.
    Credential,
    /// A deny no classification covers yet: kept closed until it's classified here.
    Unclassified,
}

/// A deny an approval lifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiftableDeny {
    /// Git's hooks and configuration.
    GitControl,
    /// A credential while fnox is off.
    Credential,
}

/// The liftability of a deny `rule`, under the launch's `credentials`.
pub fn liftability(rule: &Rule, credentials: Credentials) -> Liftability {
    use Liftability::{Floor, Liftable};
    match &rule.target {
        Target::Protected { .. } => Floor(FloorItem::Configuration),
        Target::GitControl { .. } => Liftable(LiftableDeny::GitControl),
        Target::Subpath { .. } | Target::Literal { .. } | Target::Prefix { .. } => match &rule.source {
            Source::NonoGroup { .. } => match credentials {
                Credentials::Liftable => Liftable(LiftableDeny::Credential),
                Credentials::Floor => Floor(FloorItem::Credential),
            },
            Source::Piton { area } => match area {
                PitonArea::ConfigLevel | PitonArea::AgentDir | PitonArea::FixedFolder | PitonArea::ProtectedFile => {
                    Floor(FloorItem::Configuration)
                }
                PitonArea::SocketPath => Floor(FloorItem::AllowedSocket),
                PitonArea::GitControl => Liftable(LiftableDeny::GitControl),
                PitonArea::StateFolder => Floor(FloorItem::StateFolder),
                PitonArea::ControlSocket => Floor(FloorItem::ControlSocket),
                // Allows only; a deny of them would be new.
                PitonArea::Logs | PitonArea::SshWrapper | PitonArea::Native | PitonArea::AgentSocket => {
                    Floor(FloorItem::Unclassified)
                }
            },
            Source::FnoxConfig => Floor(FloorItem::FnoxFiles),
            Source::Config { .. } => Floor(FloorItem::UserSwitch),
            Source::Workspace | Source::BuiltIn { .. } | Source::AgentDir | Source::Preset { .. } => {
                Floor(FloorItem::Unclassified)
            }
        },
    }
}

/// The `[sandbox]` entry that lifts a refusal of a process that isn't a bash call (an MCP server, an extension, pi).
/// Persisted as the user's own entry, so it applies to the whole session after a restart. Reads are never offered:
/// the base reads everything, so a process's read is refused only by a credential, the user's `"deny"`, or the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lift {
    /// `[sandbox.filesystem] "<path>" = "write"`: the path and everything below it.
    Write(PathBuf),
    /// `[sandbox.network.unix_socket_connections] "<path>" = "allow"`.
    Connect(PathBuf),
    /// `[sandbox.network.unix_socket_bindings] "<path>" = "allow"`.
    Bind(PathBuf),
    /// `[sandbox.macos.services] "<name>" = "allow"`: one Mach service.
    Service(ServiceName),
    /// `[sandbox.network.connections] "<host:port>" = "allow"`: a network destination (the gate's question).
    Network(NetworkTarget),
    /// `allow_tcp_listening = true`: listening on TCP, on every address.
    TcpListen,
}

/// A [`Lift`] as written: a key under `[sandbox]`, as its parts, and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct LiftEntry {
    /// The key's parts below `sandbox` (`["filesystem", "/x"]`).
    pub key: Vec<String>,
    pub value: serde_json::Value,
}

impl LiftEntry {
    /// The full key's parts, from the top of the file.
    pub fn path(&self) -> Vec<String> {
        std::iter::once("sandbox".to_string())
            .chain(self.key.iter().cloned())
            .collect()
    }
}

impl std::fmt::Display for LiftEntry {
    /// As TOML, a dotted key under `[sandbox]`: `filesystem."/x" = "write"`, `allow_tcp_listening = true`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key: Vec<String> = self
            .key
            .iter()
            .map(|part| toml_edit::Key::new(part.as_str()).display_repr().into_owned())
            .collect();
        let value = match &self.value {
            serde_json::Value::String(text) => toml::Value::String(text.clone()).to_string(),
            other @ (serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Array(_)
            | serde_json::Value::Object(_)) => other.to_string(),
        };
        write!(f, "{} = {value}", key.join("."))
    }
}

impl Lift {
    /// The entry as written in `[sandbox]`.
    pub fn entry(&self) -> LiftEntry {
        let entry = |table: &[&str], key: String, value: &str| LiftEntry {
            key: table.iter().map(|t| t.to_string()).chain([key]).collect(),
            value: serde_json::Value::String(value.into()),
        };
        match self {
            Lift::Write(path) => entry(&["filesystem"], path.display().to_string(), "write"),
            Lift::Connect(path) => entry(
                &["network", "unix_socket_connections"],
                path.display().to_string(),
                "allow",
            ),
            Lift::Bind(path) => entry(
                &["network", "unix_socket_bindings"],
                path.display().to_string(),
                "allow",
            ),
            Lift::Service(name) => entry(&["macos", "services"], name.to_string(), "allow"),
            Lift::Network(target) => entry(&["network", "connections"], target.to_string(), "allow"),
            Lift::TcpListen => LiftEntry {
                key: vec!["allow_tcp_listening".into()],
                value: serde_json::Value::Bool(true),
            },
        }
    }

    /// The `[sandbox]` entries with only this one: what [`widened_config`] adds.
    pub fn entries(&self) -> GrantEntries {
        let mut entries = GrantEntries::default();
        let key = |path: &Path| PathKey::new(&path.display().to_string());
        match self {
            Lift::Write(path) => {
                if let Ok(key) = key(path) {
                    entries.filesystem.insert(key, FileAccess::Write);
                }
            }
            Lift::Connect(path) => {
                if let Ok(key) = key(path) {
                    entries.network.unix_socket_connections.insert(key, Effect::Allow);
                }
            }
            Lift::Bind(path) => {
                if let Ok(key) = key(path) {
                    entries.network.unix_socket_bindings.insert(key, Effect::Allow);
                }
            }
            Lift::Service(name) => {
                entries
                    .macos
                    .services
                    .insert(ServiceKey::Name(name.clone()), Effect::Allow);
            }
            Lift::Network(target) => {
                entries.network.connections.insert(target.clone(), Effect::Allow);
            }
            Lift::TcpListen => entries.allow_tcp_listening = Some(true),
        }
        entries
    }
}

/// Where an entry is saved: in this project (its `config.local.toml`) or everywhere (the global `config.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    Everywhere,
}

/// What a blocked process is offered: the entry each scope saves. Each was checked to lift the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub project: Lift,
    /// The parent folder above the path's per-project part, when the path has one; otherwise the project's entry.
    pub everywhere: Lift,
}

impl Offer {
    pub fn at(&self, scope: Scope) -> &Lift {
        match scope {
            Scope::Project => &self.project,
            Scope::Everywhere => &self.everywhere,
        }
    }
}

/// Why a refusal isn't offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoLift {
    /// A deny decides it, and no `[sandbox]` entry would beat it.
    Decided(DeniedBy),
    /// An operation no `[sandbox]` entry lifts (process-exec of a denied file, a Mach service other than the keychain's,
    /// a target that isn't a path).
    Unsupported,
    /// The path names the refusing process itself (its pid is a component's whole number: `64421.sock`,
    /// `nvim.64421.0`), so an entry for it would never match again, and the folder above it isn't one to open from a
    /// question (temp, the home folder, the root) or opening it wouldn't lift the block. `folder` is what a hand-written
    /// entry would have to name.
    PerProcess { folder: PathBuf },
}

/// A keychain block: a lookup of its services, or a read of a keychain file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeychainBlock {
    /// fnox is off: `read = ["~/Library/Keychains"]` lifts it.
    Readable,
    /// fnox is on: the credentials of its secret sources live there, so a launch whose sandbox can read the keychain
    /// is refused, and no entry lifts it.
    ClosedForFnox,
}

/// The deny that decides a refusal no entry lifts. Each is reported differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeniedBy {
    /// A credential (nono's deny groups) other than the keychain, and the access refused: `read` lifts a read (written
    /// by hand); nothing the question saves lifts a write or a connect.
    Credential(Access),
    /// The keychain.
    Keychain(KeychainBlock),
    /// Git's hooks and configuration: `allow_git_control_files` opens them.
    GitControl,
    /// An entry the user wrote: a `"deny"`, a `"read"` for a write, a `false` boolean. Only logged.
    UserDeny,
    /// A file fnox loads, or a folder on its path.
    FnoxConfig,
    /// The rest of the floor: protected files, configuration levels, pi's agent directory, the daemon's state.
    Floor,
}

/// The deny that decides `access` to `path` under `policy`, if a deny does.
fn denied_by(policy: &Policy, path: &Path, access: Access) -> Option<DeniedBy> {
    let rule = policy
        .rules
        .iter()
        .rev()
        .find(|r| r.access.contains(&access) && r.target.covers(path))?;
    match rule.effect {
        Effect::Allow => None,
        // Credentials are reported alike whether fnox is on or not: the keychain, the one that differs, is decided
        // before this.
        Effect::Deny => Some(match liftability(rule, Credentials::Liftable) {
            Liftability::Liftable(LiftableDeny::GitControl) => DeniedBy::GitControl,
            Liftability::Liftable(LiftableDeny::Credential) | Liftability::Floor(FloorItem::Credential) => {
                DeniedBy::Credential(access)
            }
            Liftability::Floor(FloorItem::UserSwitch) => DeniedBy::UserDeny,
            Liftability::Floor(FloorItem::FnoxFiles) => DeniedBy::FnoxConfig,
            Liftability::Floor(
                FloorItem::ControlSocket
                | FloorItem::StateFolder
                | FloorItem::Configuration
                | FloorItem::AllowedSocket
                | FloorItem::Unclassified,
            ) => DeniedBy::Floor,
        }),
    }
}

/// One refusal of a process, as the kernel names it.
#[derive(Debug, Clone, Copy)]
pub struct Refused<'a> {
    pub operation: &'a str,
    pub target: &'a str,
    /// The refusing process's pid.
    pub pid: i32,
}

/// What an offer is bounded by, fixed for a launch.
#[derive(Debug, Clone, Copy)]
pub struct Bounds<'a> {
    /// The folders a per-project path component may name (the source and the checkout).
    pub projects: &'a [&'a Path],
    /// How far "everywhere" may reach.
    pub home: &'a Path,
    /// Folders where host processes create sockets and files of their own (temp): no folder at or below them is offered.
    pub shared: &'a [PathBuf],
    /// Whether the credential denies are floor (fnox on).
    pub credentials: Credentials,
}

/// The keychain folders nono denies, below `home` or the system's.
fn keychain_path(path: &Path, home: &Path) -> bool {
    path.starts_with(home.join("Library/Keychains")) || path.starts_with("/Library/Keychains")
}

/// The keychain block, by whether fnox is on.
fn keychain(credentials: Credentials) -> NoLift {
    NoLift::Decided(DeniedBy::Keychain(match credentials {
        Credentials::Liftable => KeychainBlock::Readable,
        Credentials::Floor => KeychainBlock::ClosedForFnox,
    }))
}

/// The part of `target` up to its first component that is the process's own: one with `pid` as a whole digit run.
/// Pids below 100 belong to the system's own processes at boot, never a session's: too short to tell from other
/// numbers in a name.
fn per_process(target: &Path, pid: i32) -> Option<&Path> {
    if pid < 100 {
        return None;
    }
    let pid = pid.to_string();
    let own = |name: &std::ffi::OsStr| {
        name.to_string_lossy()
            .split(|c: char| !c.is_ascii_digit())
            .any(|run| run == pid)
    };
    // Ancestors run from the target up: the last match is the outermost component.
    target.ancestors().filter(|a| a.file_name().is_some_and(own)).last()
}

/// Whether `folder` may be offered from a question: not the root, the home folder or a folder directly in it, nor at
/// or below a shared folder.
fn safe_folder(folder: &Path, bounds: &Bounds) -> bool {
    folder.parent().is_some()
        && folder != bounds.home
        && folder.parent() != Some(bounds.home)
        && !bounds.shared.iter().any(|s| folder.starts_with(s))
}

/// The offer for a refusal under the launch's `policy`, or why there's none.
///
/// `widened` resolves the launch's configuration again with one entry added in a scope: the policy a restart would
/// get. An entry is offered only when that policy allows the refused access, so an entry a later deny beats never is.
/// A path naming the refusing process itself is offered as its folder, when that is safe and lifts it, else
/// [`NoLift::PerProcess`].
pub fn lift_for(
    policy: &Policy,
    refused: Refused,
    bounds: &Bounds,
    widened: impl Fn(&Lift, Scope) -> Result<Policy, String>,
) -> Result<Offer, NoLift> {
    let target = Path::new(refused.target);
    let absolute = target.is_absolute();
    let operation = refused.operation;
    // A TCP listen: `allow_tcp_listening = true` in both scopes, offered only when the widened policy opens it; never
    // over the user's own `false`.
    if is_listen(operation, refused.target) {
        if let Listen::Denied { .. } = policy.listen {
            return Err(NoLift::Decided(DeniedBy::UserDeny));
        }
        let opens = |scope| widened(&Lift::TcpListen, scope).is_ok_and(|p| p.decide_listen().allowed);
        return match opens(Scope::Project) && opens(Scope::Everywhere) {
            true => Ok(Offer {
                project: Lift::TcpListen,
                everywhere: Lift::TcpListen,
            }),
            false => Err(NoLift::Unsupported),
        };
    }
    let (access, project) = match operation {
        "mach-lookup" if policy::KEYCHAIN_TARGETS.contains(&refused.target) => {
            return Err(keychain(bounds.credentials));
        }
        // One service, the same entry in both scopes; offered only when the widened policy opens it.
        "mach-lookup" => {
            if let Source::Config { .. } = policy.decide_service(refused.target).source {
                return Err(NoLift::Decided(DeniedBy::UserDeny));
            }
            let name = ServiceName::parse(refused.target).map_err(|_| NoLift::Unsupported)?;
            let lift = Lift::Service(name);
            let opens = |scope| widened(&lift, scope).is_ok_and(|p| p.decide_service(refused.target).allowed);
            return match opens(Scope::Project) && opens(Scope::Everywhere) {
                true => Ok(Offer {
                    project: lift.clone(),
                    everywhere: lift,
                }),
                false => Err(NoLift::Unsupported),
            };
        }
        op if op.starts_with("file-read") && absolute && keychain_path(target, bounds.home) => {
            return Err(keychain(bounds.credentials));
        }
        op if op.starts_with("file-read") && absolute => {
            return Err(NoLift::Decided(
                denied_by(policy, target, Access::Read).unwrap_or(DeniedBy::Floor),
            ));
        }
        op if op.starts_with("file-write") && absolute => (Access::Write, Lift::Write(target.to_path_buf())),
        "network-outbound" if absolute => (Access::Connect, Lift::Connect(target.to_path_buf())),
        "network-bind" if absolute => (Access::Bind, Lift::Bind(target.to_path_buf())),
        _ => return Err(NoLift::Unsupported),
    };
    // A deny rule decides it. A credential is lifted only for connecting, by an entry naming the socket (the widened
    // check below decides that one); a credential write needs a hand-written entry.
    if let Some(denied) = denied_by(policy, target, access) {
        match (access, &denied) {
            (Access::Connect | Access::Bind, DeniedBy::Credential(_)) => {}
            (
                Access::Read | Access::Write | Access::Connect | Access::Bind,
                DeniedBy::Credential(_)
                | DeniedBy::Keychain(_)
                | DeniedBy::GitControl
                | DeniedBy::UserDeny
                | DeniedBy::FnoxConfig
                | DeniedBy::Floor,
            ) => return Err(NoLift::Decided(denied)),
        }
    }
    let project = match per_process(target, refused.pid) {
        None => project,
        Some(own) => {
            let folder = own.parent().unwrap_or(own).to_path_buf();
            let lift = match access {
                Access::Connect => Lift::Connect(folder.clone()),
                Access::Bind => Lift::Bind(folder.clone()),
                Access::Read | Access::Write => Lift::Write(folder.clone()),
            };
            let offered = safe_folder(&folder, bounds)
                && [Scope::Project, Scope::Everywhere]
                    .iter()
                    .all(|scope| widened(&lift, *scope).is_ok_and(|p| p.decide(target, access).allowed));
            return match offered {
                true => Ok(Offer {
                    project: lift.clone(),
                    everywhere: lift,
                }),
                false => Err(NoLift::PerProcess { folder }),
            };
        }
    };
    let lifts = |lift: &Lift, scope: Scope| -> Result<(), NoLift> {
        let policy = widened(lift, scope).map_err(|_| NoLift::Unsupported)?;
        match policy.decide(target, access).allowed {
            true => Ok(()),
            false => Err(NoLift::Decided(
                denied_by(&policy, target, access).unwrap_or(DeniedBy::Floor),
            )),
        }
    };
    lifts(&project, Scope::Project)?;
    let everywhere = match everywhere_folder(target, bounds.projects, bounds.home) {
        Some(folder) => {
            let lift = Lift::Write(folder);
            match lifts(&lift, Scope::Everywhere) {
                Ok(()) => lift,
                Err(_) => project.clone(),
            }
        }
        None => project.clone(),
    };
    Ok(Offer { project, everywhere })
}

/// What a token opens in pi's live sandbox (#408): one resolved path, one Mach service, or TCP listening. A token's
/// class is its kind's (`rules::LIFT_*`); the companion rules decide what it can reopen.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveLift {
    /// Read a file's contents (a credential while fnox is off).
    Read { path: PathBuf },
    /// Write a file or what's in a folder, connect to a socket there, or create one.
    Write { path: PathBuf },
    /// Look up one Mach service.
    Service { name: String },
    /// Listen on TCP.
    Listen,
}

impl LiveLift {
    /// As a question names it.
    pub fn describe(&self) -> String {
        match self {
            LiveLift::Read { path } => format!("reading {}", path.display()),
            LiveLift::Write { path } => format!("writing {}", path.display()),
            LiveLift::Service { name } => format!("the Mach service {name}"),
            LiveLift::Listen => "listening on TCP".into(),
        }
    }
}

/// Why a live lift is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LiveRefusal {
    /// A floor deny or the user's own covers it (or something below a folder): no approval lifts it.
    #[error("{0}")]
    Floor(String),
    /// Nothing a token can reopen: an operation or target the model doesn't lift live.
    #[error("{0}")]
    Unsupported(String),
    /// Not blocked: nothing to lift.
    #[error("it isn't blocked")]
    NotBlocked,
}

/// The live lift for a refusal of pi's own process, or why there's none: what the block question's "until the run
/// settles" and "for this session" grant. A per-process path lifts its folder when that's safe, as [`lift_for`]'s does.
/// A credential read stays a notice (probes), so it's [`LiveRefusal::Unsupported`] here.
pub fn live_lift(policy: &Policy, refused: Refused, bounds: &Bounds) -> Result<LiveLift, LiveRefusal> {
    let target = Path::new(refused.target);
    let unsupported = || LiveRefusal::Unsupported(format!("{} {}", refused.operation, refused.target));
    let lift = match refused.operation {
        op if is_listen(op, refused.target) => LiveLift::Listen,
        "mach-lookup" => LiveLift::Service {
            name: refused.target.to_string(),
        },
        op if (op.starts_with("file-write") || op == "network-outbound" || op == "network-bind")
            && target.is_absolute() =>
        {
            let path = match per_process(target, refused.pid) {
                Some(own) => {
                    let folder = own.parent().unwrap_or(own).to_path_buf();
                    if !safe_folder(&folder, bounds) {
                        return Err(unsupported());
                    }
                    folder
                }
                None => target.to_path_buf(),
            };
            LiveLift::Write { path }
        }
        _ => return Err(unsupported()),
    };
    check_live(policy, &lift, bounds.credentials)?;
    Ok(lift)
}

/// Whether `lift` may be granted under the launch's `policy`: refused when any deny that covers it is floor or the
/// user's own, not only the deciding one (a companion would reopen an earlier one: Seatbelt's last match), and for a
/// folder when such a deny covers anything below it. The daemon's check, independent of the profile's `require-not`.
pub fn check_live(policy: &Policy, lift: &LiveLift, credentials: Credentials) -> Result<(), LiveRefusal> {
    match lift {
        LiveLift::Listen => match &policy.listen {
            Listen::Denied { source } => Err(LiveRefusal::Floor(format!("listening is closed by {source:?}"))),
            Listen::Closed => Ok(()),
            Listen::Open { .. } => Err(LiveRefusal::NotBlocked),
        },
        LiveLift::Service { name } => {
            if policy::KEYCHAIN_TARGETS.contains(&name.as_str()) {
                return match (credentials, policy.keychain) {
                    (_, true) => Err(LiveRefusal::NotBlocked),
                    (Credentials::Liftable, false) => Ok(()),
                    (Credentials::Floor, false) => {
                        Err(LiveRefusal::Floor("the keychain stays closed while fnox is on".into()))
                    }
                };
            }
            if ServiceName::parse(name).is_err() {
                return Err(LiveRefusal::Unsupported(format!("{name} isn't a service name")));
            }
            let decision = policy.decide_service(name);
            match (decision.allowed, &decision.source) {
                (true, _) => Err(LiveRefusal::NotBlocked),
                (false, Source::Config { .. }) => {
                    Err(LiveRefusal::Floor(format!("{name} is closed by your own entry")))
                }
                (false, _) => Ok(()),
            }
        }
        LiveLift::Read { path } | LiveLift::Write { path } => {
            // Every access the token's class reopens: a write token's companions also cover socket connects and binds.
            let accesses: &[Access] = match lift {
                LiveLift::Read { .. } => &[Access::Read],
                LiveLift::Write { .. } | LiveLift::Service { .. } | LiveLift::Listen => {
                    &[Access::Write, Access::Connect, Access::Bind]
                }
            };
            if !path.is_absolute() {
                return Err(LiveRefusal::Unsupported(format!("{} isn't absolute", path.display())));
            }
            // Any deny of those accesses covering the path, or below it (a folder token opens what's inside).
            let below = |rule: &Rule| match &rule.target {
                Target::Subpath { path: p } | Target::Literal { path: p } | Target::Prefix { path: p } => {
                    p.starts_with(path)
                }
                // Their regexes reach any depth of a checkout: a folder above or at one covers them.
                Target::Protected { source, checkout } => source.starts_with(path) || checkout.starts_with(path),
                Target::GitControl { control } => control.checkouts.iter().any(|c| c.starts_with(path)),
            };
            for rule in &policy.rules {
                if rule.effect != Effect::Deny || !rule.access.iter().any(|a| accesses.contains(a)) {
                    continue;
                }
                if !(rule.target.covers(path) || below(rule)) {
                    continue;
                }
                match liftability(rule, credentials) {
                    Liftability::Liftable(_) => {}
                    Liftability::Floor(item) => {
                        return Err(LiveRefusal::Floor(format!(
                            "{} is closed by {} ({item:?})",
                            path.display(),
                            describe_source(&rule.source)
                        )));
                    }
                }
            }
            Ok(())
        }
    }
}

/// What a widening asked by code in pi's process (#407) would reopen that an entry of the launch's own configuration
/// closes. The user closed it, so no approval reopens it (the floor's "user's own `\"deny\"`, `\"read\"`, and `false`").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WideningRefusal {
    /// A switch the chain sets to `false`.
    #[error("`{key} = false` in {} closes it", file.display())]
    Switch { key: &'static str, file: PathBuf },
    /// A path at or inside one the chain sets to `"deny"`, or to `"read"` when the request writes.
    #[error("{asked} is inside `{table}.\"{closed}\" = \"{value}\"` in {}", file.display())]
    Path {
        asked: String,
        table: &'static str,
        closed: PathKey,
        value: &'static str,
        file: PathBuf,
    },
    /// A connection or service entry the chain sets to `"deny"`.
    #[error("`{table}.\"{key}\" = \"deny\"` in {} closes it", file.display())]
    Entry {
        table: &'static str,
        key: String,
        file: PathBuf,
    },
    /// A path key that can't be expanded (an unset `$VAR`).
    #[error("{0}")]
    Unexpanded(String),
}

/// Checks `asked` against the launch's own `[sandbox]` chain: every key it would open that an entry there closes is
/// refused, whichever file closes it. A request is resolved as the nearest file ([`Resolver::with`]), which would
/// replace those values, and the policy takes the most specific path entry, so a path inside a closed folder is checked
/// by containment, as the resolver checks a committed file's. `expand` turns a key into the absolute path the sandbox
/// would use.
pub fn check_widening(
    config: &LaunchConfig,
    asked: &GrantEntries,
    expand: impl Fn(&PathKey) -> Result<PathBuf, String>,
) -> Result<(), WideningRefusal> {
    let settings = &config.sandbox;
    let switches = [
        (
            "allow_tcp_listening",
            asked.allow_tcp_listening,
            &settings.allow_tcp_listening,
        ),
        (
            "allow_git_control_files",
            asked.allow_git_control_files,
            &settings.allow_git_control_files,
        ),
        (
            "allow_launching_apps",
            asked.allow_launching_apps,
            &settings.allow_launching_apps,
        ),
    ];
    for (key, asked, set) in switches {
        if let (Some(true), Some(Listed { value: false, file })) = (asked, set) {
            return Err(WideningRefusal::Switch {
                key,
                file: file.clone(),
            });
        }
    }
    let expanded = |key: &PathKey| expand(key).map_err(WideningRefusal::Unexpanded);
    // A path asked for `write` (or a socket `allow`) is closed by a `deny`, and a write also by the user's `read`.
    let closed_by = |asked: &PathKey, writes: bool| -> Result<(), WideningRefusal> {
        let path = expanded(asked)?;
        for (key, entry) in &settings.filesystem {
            let closes = match entry.value {
                FileAccess::Deny => true,
                FileAccess::Read => writes,
                FileAccess::Write => false,
            };
            if closes && path.starts_with(expanded(key)?) {
                return Err(WideningRefusal::Path {
                    asked: asked.to_string(),
                    table: "filesystem",
                    closed: key.clone(),
                    value: entry.value.name(),
                    file: entry.file.clone(),
                });
            }
        }
        Ok(())
    };
    for (key, access) in &asked.filesystem {
        match access {
            FileAccess::Read => closed_by(key, false)?,
            FileAccess::Write => closed_by(key, true)?,
            // A deny only closes.
            FileAccess::Deny => {}
        }
    }
    let sockets = [
        (
            "network.unix_socket_connections",
            &asked.network.unix_socket_connections,
            &settings.unix_socket_connections,
        ),
        (
            "network.unix_socket_bindings",
            &asked.network.unix_socket_bindings,
            &settings.unix_socket_bindings,
        ),
    ];
    for (table, asked, set) in sockets {
        for (key, effect) in asked {
            match effect {
                crate::config::Effect::Deny => continue,
                crate::config::Effect::Allow => {}
            }
            // A socket under a folder the user denies is closed by that folder too.
            closed_by(key, false)?;
            let path = expanded(key)?;
            for (closed, entry) in set {
                match entry.value {
                    crate::config::Effect::Allow => continue,
                    crate::config::Effect::Deny => {}
                }
                if path.starts_with(expanded(closed)?) {
                    return Err(WideningRefusal::Path {
                        asked: key.to_string(),
                        table,
                        closed: closed.clone(),
                        value: "deny",
                        file: entry.file.clone(),
                    });
                }
            }
        }
    }
    for (target, effect) in &asked.network.connections {
        match effect {
            crate::config::Effect::Deny => continue,
            crate::config::Effect::Allow => {}
        }
        if let Some(entry) = settings.connections.get(target)
            && entry.value == crate::config::Effect::Deny
        {
            return Err(WideningRefusal::Entry {
                table: "network.connections",
                key: target.to_string(),
                file: entry.file.clone(),
            });
        }
    }
    for (key, effect) in &asked.macos.services {
        match effect {
            crate::config::Effect::Deny => continue,
            crate::config::Effect::Allow => {}
        }
        if let Some(entry) = settings.services.get(key)
            && entry.value == crate::config::Effect::Deny
        {
            return Err(WideningRefusal::Entry {
                table: "macos.services",
                key: key.to_string(),
                file: entry.file.clone(),
            });
        }
    }
    Ok(())
}

/// A rule's source, as a refusal names it.
fn describe_source(source: &Source) -> String {
    match source {
        Source::Config { key, from } => format!("your entry `{key}` ({from})"),
        Source::Piton { area } => area.describe().to_string(),
        Source::NonoGroup { group } => format!("the credential group {group}"),
        Source::FnoxConfig => "fnox's configuration".into(),
        Source::Workspace | Source::BuiltIn { .. } | Source::AgentDir | Source::Preset { .. } => "the sandbox".into(),
    }
}

/// What a block in a bash call's own sandbox is, for asking about it without the agent: whether an approved rerun in
/// the sandbox (`all`) would lift it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandBlock {
    /// `all` allows it: an approved rerun lifts it.
    Liftable,
    /// A credential read (nono's deny groups): usually a probe the program recovers from (ssh opens each private key
    /// to read its public half on every SSH Git command), so it isn't asked about by itself.
    CredentialRead,
    /// `all` still denies it: only a host run passes the floor, and the floor is never asked about by itself.
    Floor,
    /// A refusal programs hit and recover from ([`INCIDENTAL`]): logged only, never a reason to ask.
    Incidental,
    /// An operation or target the policy model doesn't know.
    Unsupported,
}

/// Refusals programs hit routinely and recover from, as the kernel names them (operation, target), measured on the host
/// (docs/pi-findings.md): `open` and `osascript` write a CPU-type sysctl and connect to syslog's socket on their way to
/// Launch Services, and carry on without them.
pub const INCIDENTAL: [(&str, &str); 2] = [
    ("sysctl-write", "kern.grade_cputype"),
    ("network-outbound", "/private/var/run/syslog"),
];

/// Whether a refusal is one programs hit and recover from.
pub fn incidental(operation: &str, target: &str) -> bool {
    INCIDENTAL.iter().any(|(op, t)| *op == operation && *t == target)
}

/// Classifies the kernel's refusal of `operation` on `target` in a call that ran under `launch`, against `all` (the
/// launch widened to everything but the floor).
pub fn command_block(launch: &Policy, all: &Policy, operation: &str, target: &str) -> CommandBlock {
    let lifted = |allowed: bool| match allowed {
        true => CommandBlock::Liftable,
        false => CommandBlock::Floor,
    };
    if incidental(operation, target) {
        return CommandBlock::Incidental;
    }
    match operation {
        // Launching apps: open when `all` opens apps.
        "lsopen" | "appleevent-send" => {
            return match (&launch.apps, &all.apps) {
                // The user's own `false`: only a host run passes it.
                (Apps::Denied { .. }, _) => CommandBlock::Floor,
                (Apps::Closed | Apps::Open { .. }, Apps::Open { .. }) => CommandBlock::Liftable,
                (Apps::Closed | Apps::Open { .. }, Apps::Closed | Apps::Denied { .. }) => CommandBlock::Floor,
            };
        }
        "mach-lookup" => return lifted(all.decide_service(target).allowed),
        // cfprefsd checks the caller's write access to the domain's plist in the preferences folder, which `all` opens.
        "user-preference-write" => return CommandBlock::Liftable,
        // A TCP listen: `all` opens it.
        op if is_listen(op, target) => return lifted(all.decide_listen().allowed),
        // `all` opens files, sockets, services, and apps, never these: only a host run passes them. A refused
        // `file-issue-extension` (the app-bundle extension `osascript`'s Apple Event carries) stays refused with apps
        // open too (checked on the host).
        "signal" | "sysctl-write" | "job-creation" | "file-issue-extension" => return CommandBlock::Floor,
        _ => {}
    }
    let path = Path::new(target);
    // A TCP or UDP connection the sandbox refused directly (a program that ignores the proxy variables): an approved
    // rerun opens the network.
    if operation == "network-outbound"
        && !path.is_absolute()
        && let Some(to) = super::Destination::parse(target)
    {
        return lifted(all.decide_network(&to).allowed);
    }
    if !path.is_absolute() {
        return CommandBlock::Unsupported;
    }
    let access = match operation {
        op if op.starts_with("file-read") || op.starts_with("process-exec") || op == "file-map-executable" => {
            Access::Read
        }
        op if op.starts_with("file-write") => Access::Write,
        "network-outbound" => Access::Connect,
        "network-bind" => Access::Bind,
        _ => return CommandBlock::Unsupported,
    };
    match (access, denied_by(launch, path, access)) {
        (Access::Read, Some(DeniedBy::Credential(_))) => CommandBlock::CredentialRead,
        (
            Access::Read | Access::Write | Access::Connect | Access::Bind,
            None
            | Some(
                DeniedBy::Credential(_)
                | DeniedBy::Keychain(_)
                | DeniedBy::GitControl
                | DeniedBy::UserDeny
                | DeniedBy::FnoxConfig
                | DeniedBy::Floor,
            ),
        ) => lifted(all.decide(path, access).allowed),
    }
}

/// The folder above `target`'s per-project part: the parent of its first component that names a project folder the way
/// pi and Claude Code name per-folder state (the folder's path with every `/` made `-`). `None` when no component does,
/// or when that parent is too broad to grant everywhere: the root, the home folder or a folder directly in it, or a
/// folder holding a project.
fn everywhere_folder(target: &Path, projects: &[&Path], home: &Path) -> Option<PathBuf> {
    let encoded: Vec<String> = projects.iter().map(|p| p.to_string_lossy().replace('/', "-")).collect();
    let component = target.ancestors().find(|a| {
        a.file_name()
            .is_some_and(|n| encoded.iter().any(|e| *e == n.to_string_lossy()))
    })?;
    let parent = component.parent()?;
    let too_broad = parent.parent().is_none()
        || parent == home
        || parent.parent() == Some(home)
        || projects.iter().any(|p| p.starts_with(parent));
    (!too_broad).then(|| parent.to_path_buf())
}

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

    fn piton(area: PitonArea) -> Source {
        Source::Piton { area }
    }

    /// A launch policy with one rule of each kind the floor decides about.
    fn launch() -> Policy {
        let all = [Access::Read, Access::Write, Access::Connect, Access::Bind];
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
                deny(&[Access::Write], "/h/.pi/agent", piton(PitonArea::AgentDir)),
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
                deny(&[Access::Write], "/h/.config/piton", piton(PitonArea::ConfigLevel)),
                Rule {
                    effect: Effect::Deny,
                    access: vec![Access::Write],
                    target: Target::Protected {
                        source: "/w".into(),
                        checkout: "/w".into(),
                    },
                    source: piton(PitonArea::ProtectedFile),
                },
                Rule {
                    effect: Effect::Deny,
                    access: vec![Access::Write],
                    target: Target::GitControl {
                        control: GitControl::of(&[Path::new("/w")]),
                    },
                    source: piton(PitonArea::GitControl),
                },
                deny(&all, "/h/.local/state/piton", piton(PitonArea::StateFolder)),
                Rule {
                    effect: Effect::Allow,
                    access: vec![Access::Connect],
                    target: Target::Literal {
                        path: "/h/.local/state/piton/agent.sock".into(),
                    },
                    source: piton(PitonArea::AgentSocket),
                },
            ],
            keychain: false,
            services: Services::Listed { grants: Vec::new() },
            apps: Apps::Closed,
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
            tag: String::new(),
            lifts: Default::default(),
        }
    }

    /// Each deny of the launch classifies as the floor table says; credentials follow fnox.
    #[test]
    fn every_deny_classifies() {
        use FloorItem as F;
        use Liftability::{Floor, Liftable};
        let denies: Vec<Rule> = launch()
            .rules
            .into_iter()
            .filter(|r| r.effect == Effect::Deny)
            .collect();
        let classes =
            |credentials| -> Vec<Liftability> { denies.iter().map(|r| liftability(r, credentials)).collect() };
        let credential = |item| {
            [
                item,
                item,
                Floor(F::Configuration),
                Floor(F::UserSwitch),
                Floor(F::Configuration),
                Floor(F::Configuration),
                Liftable(LiftableDeny::GitControl),
                Floor(F::StateFolder),
            ]
        };
        assert_eq!(
            classes(Credentials::Liftable),
            credential(Liftable(LiftableDeny::Credential))
        );
        assert_eq!(classes(Credentials::Floor), credential(Floor(F::Credential)));
        let control = deny(&[Access::Connect], "/s/piton.sock", piton(PitonArea::ControlSocket));
        assert_eq!(liftability(&control, Credentials::Liftable), Floor(F::ControlSocket));
        let fnox = deny(&[Access::Write], "/h/.config/fnox", Source::FnoxConfig);
        assert_eq!(liftability(&fnox, Credentials::Liftable), Floor(F::FnoxFiles));
    }

    /// `all` opens everything the launch closes except the floor, which still decides wherever it applies.
    /// Mach services and launching apps open too; the keychain's services stay closed behind fnox.
    #[test]
    fn all_opens_services_and_apps() {
        let widened = all_below(&launch(), Credentials::Liftable, &roots());
        assert!(widened.decide_service("com.example.agent").allowed);
        assert!(widened.decide_service("com.apple.SecurityServer").allowed, "fnox off");
        assert!(matches!(widened.apps, Apps::Open { .. }));
        assert!(widened.decide_listen().allowed, "TCP listening isn't floor");
        let floor = all_below(&launch(), Credentials::Floor, &roots());
        assert!(floor.decide_service("com.example.agent").allowed);
        assert!(!floor.decide_service("com.apple.SecurityServer").allowed, "fnox on");
        super::super::apply::capabilities(&widened).expect("nono accepts it");
    }
    #[test]
    fn all_keeps_the_floor() {
        let launch = launch();
        let widened = all_below(&launch, Credentials::Liftable, &roots());
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
        assert!(allowed("/w/test.sock", Access::Bind), "creating sockets");
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
        // The root itself isn't opened, only what's below its entries.
        assert!(!allowed("/new-top-level", Access::Write));
    }

    /// The root entries of the test policies' paths.
    fn roots() -> Vec<PathBuf> {
        ["/h", "/w", "/tmp", "/etc"].map(PathBuf::from).to_vec()
    }

    /// A call's block is asked about only when `all` lifts it; a credential read is a probe, the floor is never asked.
    #[test]
    fn command_blocks_are_classified_against_all() {
        let launch = launch();
        let lifted = all_below(&launch, Credentials::Liftable, &roots());
        let fnox = all_below(&launch, Credentials::Floor, &roots());
        let block = |all: &Policy, op: &str, target: &str| command_block(&launch, all, op, target);
        assert_eq!(block(&lifted, "file-write-create", "/h/x"), CommandBlock::Liftable);
        // A direct connection the filtered network refused: an approved rerun opens the network.
        assert!(matches!(lifted.network, Network::Open { .. }));
        assert_eq!(
            block(&lifted, "network-outbound", "140.82.112.3:443"),
            CommandBlock::Liftable
        );
        assert_eq!(block(&lifted, "network-outbound", "[::1]:5432"), CommandBlock::Liftable);
        assert_eq!(
            block(&lifted, "file-write-data", "/w/.git/hooks/pre-commit"),
            CommandBlock::Liftable
        );
        assert_eq!(
            block(&lifted, "file-write-create", "/h/.pi/agent/x"),
            CommandBlock::Floor
        );
        assert_eq!(
            block(&lifted, "file-write-create", "/h/secret/x"),
            CommandBlock::Floor,
            "the user's deny"
        );
        assert_eq!(
            block(&lifted, "file-read-data", "/h/.ssh/id_ed25519"),
            CommandBlock::CredentialRead
        );
        assert_eq!(
            block(&fnox, "file-read-data", "/h/.ssh/id_ed25519"),
            CommandBlock::CredentialRead
        );
        assert_eq!(
            block(&lifted, "mach-lookup", "com.apple.SecurityServer"),
            CommandBlock::Liftable,
            "fnox off"
        );
        assert_eq!(
            block(&fnox, "mach-lookup", "com.apple.SecurityServer"),
            CommandBlock::Floor,
            "fnox on"
        );
        assert_eq!(
            block(&lifted, "mach-lookup", "com.example.agent"),
            CommandBlock::Liftable
        );
        assert_eq!(block(&lifted, "lsopen", ""), CommandBlock::Liftable);
        assert_eq!(
            block(&lifted, "appleevent-send", "com.apple.Terminal"),
            CommandBlock::Liftable
        );
        assert_eq!(
            block(&lifted, "network-outbound", "/h/.local/state/piton/piton.sock"),
            CommandBlock::Floor
        );
        assert_eq!(
            block(&lifted, "network-outbound", "/tmp/nvim.sock"),
            CommandBlock::Liftable
        );
        assert_eq!(block(&lifted, "system-info", "x"), CommandBlock::Unsupported);
        // Operations only the tagged default reports.
        assert_eq!(
            block(&lifted, "user-preference-write", "com.foo"),
            CommandBlock::Liftable
        );
        assert_eq!(
            block(
                &lifted,
                "file-issue-extension",
                "target:/System/Applications/Utilities/Terminal.app"
            ),
            CommandBlock::Floor
        );
        assert_eq!(
            block(&lifted, "signal", "pgrp [bash(1)] signum:15"),
            CommandBlock::Floor
        );
        assert_eq!(block(&lifted, "sysctl-write", "kern.x"), CommandBlock::Floor);
        assert_eq!(
            block(&lifted, "sysctl-write", "kern.grade_cputype"),
            CommandBlock::Incidental
        );
        assert_eq!(
            block(&lifted, "network-outbound", "/private/var/run/syslog"),
            CommandBlock::Incidental
        );
        // A TCP listen: the kernel names the local address it checked.
        assert_eq!(
            block(&lifted, "network-inbound", "local:*:50958"),
            CommandBlock::Liftable
        );
        assert_eq!(block(&launch, "network-inbound", "local:*:50958"), CommandBlock::Floor);
    }

    /// nono applies the widened policy: it refuses a write grant on `/` (`platform rule must not grant root-level write
    /// access`), which left every approved command unable to start.
    #[test]
    fn nono_accepts_the_widened_policy() {
        let widened = all(&launch(), Credentials::Liftable);
        assert!(
            !widened.rules.iter().any(|r| r.access.contains(&Access::Write)
                && r.effect == Effect::Allow
                && r.target == Target::Subpath { path: "/".into() }),
            "no write grant on the root"
        );
        assert!(widened.decide(Path::new("/Users/x/.zshrc"), Access::Write).allowed);
        assert!(widened.decide(Path::new("/private/tmp/x"), Access::Write).allowed);
        super::super::apply::capabilities(&widened).expect("nono accepts every rule");
    }

    /// The widened policy applies: a forked child sandboxes itself with it, as `piton sandbox-exec` does. Skipped inside
    /// a sandbox, where macOS doesn't start another one.
    #[test]
    #[allow(unsafe_code)]
    fn the_widened_policy_applies() {
        use super::super::apply::{Containment, capabilities, containment};
        match containment() {
            Containment::Inherited => return,
            Containment::Apply => {}
        }
        let caps = capabilities(&all(&launch(), Credentials::Liftable)).unwrap();
        // SAFETY: the child only applies the sandbox and exits; the parent waits for it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            let code = match nono::Sandbox::apply_auto(&caps) {
                Ok(_) => 0,
                Err(_) => 1,
            };
            // SAFETY: leave the forked test process without running the parent's test harness.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "applying failed: {status}"
        );
    }

    /// pi's sandbox: every liftable deny is followed by its companion in Piton's class, which excludes every floor deny
    /// of the same operation; floor denies get none. A bash call's sandbox carries none.
    #[test]
    fn liftable_denies_get_companions_that_exclude_the_floor() {
        use super::super::rules::{LIFT_MACH, LIFT_READ, LIFT_WRITE, Lifts};
        let mut pi = launch();
        pi.tag = "t".into();
        pi.lifts = Lifts::Pi {
            credentials: Credentials::Liftable,
        };
        let rules = pi.seatbelt().unwrap();
        let companions: Vec<&String> = rules
            .iter()
            .filter(|r| r.contains("(extension \"com.piton.lift"))
            .collect();
        let after = |deny: &str| -> Option<&String> {
            let i = rules.iter().position(|r| r.starts_with(deny))?;
            rules.get(i + 1).filter(|r| r.contains("(extension "))
        };
        // Writes outside every grant: the leading tagged deny's companion.
        let base = after("(deny file-write* (with message").expect("the base write deny has a companion");
        assert!(base.contains(LIFT_WRITE), "{base}");
        // Credentials while fnox is off: a read companion.
        let ssh = after("(deny file-read-data (subpath \"/h/.ssh\")").expect("credential read companion");
        assert!(ssh.contains(LIFT_READ), "{ssh}");
        // Git's control files: one companion per regex deny.
        let git = after("(deny file-write* (regex #\"^/w/\\.").expect("Git control companion");
        assert!(git.contains(LIFT_WRITE), "{git}");
        // Every write companion excludes every floor write deny: pi's agent dir, the configuration, the user's deny,
        // the state folder, the protected files.
        for floor in [
            "(require-not (subpath \"/h/.pi/agent\"))",
            "(require-not (subpath \"/h/.config/piton\"))",
            "(require-not (subpath \"/h/secret\"))",
            "(require-not (subpath \"/h/.local/state/piton\"))",
            "(require-not (regex #\"^/w/",
        ] {
            assert!(git.contains(floor), "{floor} in {git}");
            assert!(base.contains(floor), "{floor} in {base}");
        }
        // Floor denies get none.
        for floor in [
            "(deny file-write* (subpath \"/h/.pi/agent\")",
            "(deny file-write* (subpath \"/h/secret\")",
            "(deny file-write* (subpath \"/h/.local/state/piton\")",
        ] {
            assert_eq!(after(floor), None, "{floor}");
        }
        // The closed Mach list and the keychain (fnox off) lift with a Mach token; listening with a listen token.
        assert!(after("(deny mach-lookup (with").is_some_and(|c| c.contains(LIFT_MACH)));
        assert!(after("(deny mach-lookup (global-name \"com.apple.SecurityServer\")").is_some());
        assert!(after("(deny network-inbound").is_some());
        // The control socket is floor: the socket base's connect companion excludes it.
        let connect = after("(deny network-outbound (subpath \"/\")").expect("socket base companion");
        assert!(
            connect.contains("(require-not (path \"/h/.local/state/piton\"))")
                || connect.contains("(require-not (subpath \"/h/.local/state/piton\"))"),
            "{connect}"
        );
        // Companions only in pi's sandbox.
        assert!(!companions.is_empty());
        let mut call = launch();
        call.tag = "t".into();
        assert!(!call.seatbelt().unwrap().iter().any(|r| r.contains("(extension ")));
        // Behind fnox, credentials and the keychain are floor: no companions, and they're excluded.
        pi.lifts = Lifts::Pi {
            credentials: Credentials::Floor,
        };
        let rules = pi.seatbelt().unwrap();
        let i = rules
            .iter()
            .position(|r| r.starts_with("(deny file-read-data (subpath \"/h/.ssh\")"))
            .unwrap();
        assert!(!rules[i + 1].contains("(extension "), "{}", rules[i + 1]);
        let i = rules
            .iter()
            .position(|r| r.starts_with("(deny mach-lookup (global-name \"com.apple.SecurityServer\")"))
            .unwrap();
        assert!(!rules[i + 1].contains("(extension "), "{}", rules[i + 1]);
        // nono accepts every companion.
        super::super::apply::capabilities(&pi).expect("nono accepts the companions");
    }

    fn live_bounds<'a>(credentials: Credentials, shared: &'a [PathBuf]) -> Bounds<'a> {
        Bounds {
            projects: &[],
            home: Path::new("/h"),
            shared,
            credentials,
        }
    }

    fn refused<'a>(operation: &'a str, target: &'a str) -> Refused<'a> {
        Refused {
            operation,
            target,
            pid: 1,
        }
    }

    /// What a block of pi's process lifts live, and what the floor refuses.
    #[test]
    fn live_lifts_for_refusals() {
        let p = launch();
        let shared = Vec::new();
        let b = live_bounds(Credentials::Liftable, &shared);
        let write = |path: &str| LiveLift::Write { path: path.into() };
        // Outside every grant; Git's control files: liftable.
        assert_eq!(
            live_lift(&p, refused("file-write-create", "/h/notes.txt"), &b),
            Ok(write("/h/notes.txt"))
        );
        assert_eq!(
            live_lift(&p, refused("file-write-data", "/w/.git/config"), &b),
            Ok(write("/w/.git/config"))
        );
        assert_eq!(
            live_lift(&p, refused("network-outbound", "/h/x.sock"), &b),
            Ok(write("/h/x.sock"))
        );
        assert_eq!(
            live_lift(&p, refused("mach-lookup", "com.example.agent"), &b),
            Ok(LiveLift::Service {
                name: "com.example.agent".into()
            })
        );
        assert_eq!(
            live_lift(&p, refused("network-inbound", "local:*:5000"), &b),
            Ok(LiveLift::Listen)
        );
        // The floor and the user's own: refused.
        for (op, target) in [
            ("file-write-data", "/h/.pi/agent/settings.json"),
            ("file-write-data", "/h/.config/piton/config.toml"),
            ("file-write-data", "/h/secret/x"),
            ("file-write-data", "/w/.piton/config.toml"),
            ("file-write-data", "/h/.local/state/piton/trust.json"),
        ] {
            assert!(
                matches!(live_lift(&p, refused(op, target), &b), Err(LiveRefusal::Floor(_))),
                "{target}"
            );
        }
        // Reads, signals: nothing a token reopens from a block.
        assert!(matches!(
            live_lift(&p, refused("file-read-data", "/h/.ssh/id"), &b),
            Err(LiveRefusal::Unsupported(_))
        ));
        assert!(matches!(
            live_lift(&p, refused("signal", "x"), &b),
            Err(LiveRefusal::Unsupported(_))
        ));
        // A folder over the floor: refused, for what's below it.
        assert!(matches!(
            check_live(&p, &write("/h"), Credentials::Liftable),
            Err(LiveRefusal::Floor(_))
        ));
        assert!(matches!(
            check_live(&p, &write("/w"), Credentials::Liftable),
            Err(LiveRefusal::Floor(_)),
        ));
        // The keychain: liftable with fnox off, floor with it on.
        let keychain = LiveLift::Service {
            name: "com.apple.SecurityServer".into(),
        };
        assert_eq!(check_live(&p, &keychain, Credentials::Liftable), Ok(()));
        assert!(matches!(
            check_live(&p, &keychain, Credentials::Floor),
            Err(LiveRefusal::Floor(_))
        ));
        let read = LiveLift::Read {
            path: "/h/.ssh/id".into(),
        };
        assert_eq!(check_live(&p, &read, Credentials::Liftable), Ok(()));
        assert!(matches!(
            check_live(&p, &read, Credentials::Floor),
            Err(LiveRefusal::Floor(_))
        ));
    }

    /// A write token's class also reopens socket connects and binds: a floor or user deny of either refuses it.
    #[test]
    fn socket_denies_refuse_a_write_lift() {
        let mut p = launch();
        p.rules.push(Rule {
            effect: Effect::Deny,
            access: vec![Access::Connect],
            target: Target::Literal {
                path: "/h/x.sock".into(),
            },
            source: Source::Config {
                key: "sandbox.network.unix_socket_connections.\"/h/x.sock\"".into(),
                from: Provenance::File {
                    path: "/g/config.toml".into(),
                },
            },
        });
        p.rules.push(Rule {
            effect: Effect::Deny,
            access: vec![Access::Bind],
            target: Target::Literal {
                path: "/h/run/kept.sock".into(),
            },
            source: piton(PitonArea::SocketPath),
        });
        let write = |path: &str| LiveLift::Write { path: path.into() };
        for path in ["/h/x.sock", "/h/run/kept.sock", "/h/run"] {
            assert!(
                matches!(
                    check_live(&p, &write(path), Credentials::Liftable),
                    Err(LiveRefusal::Floor(_))
                ),
                "{path}"
            );
        }
        assert_eq!(check_live(&p, &write("/h/other.sock"), Credentials::Liftable), Ok(()));
    }

    /// A floor deny placed before the liftable one that decides (the user's `"read"` on a checkout, then Git's
    /// control-file deny) refuses the lift: a companion would reopen it (Seatbelt's last match).
    #[test]
    fn an_earlier_floor_deny_refuses_a_live_lift() {
        let mut p = launch();
        let read_only = Rule {
            effect: Effect::Deny,
            access: vec![Access::Write],
            target: Target::Subpath { path: "/src".into() },
            source: Source::Config {
                key: "sandbox.filesystem.\"/src\"".into(),
                from: Provenance::File {
                    path: "/g/config.toml".into(),
                },
            },
        };
        let git = Rule {
            effect: Effect::Deny,
            access: vec![Access::Write],
            target: Target::GitControl {
                control: GitControl::of(&[Path::new("/src")]),
            },
            source: piton(PitonArea::GitControl),
        };
        p.rules.insert(1, read_only);
        p.rules.push(git);
        let shared = Vec::new();
        let b = live_bounds(Credentials::Liftable, &shared);
        assert!(matches!(
            live_lift(&p, refused("file-write-data", "/src/.git/config"), &b),
            Err(LiveRefusal::Floor(_))
        ));
        // And the profile excludes it from the Git companion too.
        p.tag = "t".into();
        p.lifts = super::super::rules::Lifts::Pi {
            credentials: Credentials::Liftable,
        };
        let rules = p.seatbelt().unwrap();
        let i = rules
            .iter()
            .position(|r| r.starts_with("(deny file-write* (regex #\"^/src/\\."))
            .unwrap();
        assert!(
            rules[i + 1].contains("(require-not (subpath \"/src\"))"),
            "{}",
            rules[i + 1]
        );
    }

    /// pi's sandbox with companions applies: a forked child sandboxes itself with it. Skipped inside a sandbox.
    #[test]
    #[allow(unsafe_code)]
    fn the_lifting_policy_applies() {
        use super::super::apply::{Containment, capabilities, containment};
        match containment() {
            Containment::Inherited => return,
            Containment::Apply => {}
        }
        let mut pi = launch();
        pi.tag = "t".into();
        pi.lifts = super::super::rules::Lifts::Pi {
            credentials: Credentials::Liftable,
        };
        let caps = capabilities(&pi).unwrap();
        // SAFETY: the child only applies the sandbox and exits; the parent waits for it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            let code = match nono::Sandbox::apply_auto(&caps) {
                Ok(_) => 0,
                Err(_) => 1,
            };
            // SAFETY: leave the forked test process without running the parent's test harness.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "applying failed: {status}"
        );
    }

    /// With fnox on, the credential denies are floor too.
    #[test]
    fn credentials_are_floor_behind_fnox() {
        let widened = all_below(&launch(), Credentials::Floor, &roots());
        assert!(!widened.decide(Path::new("/h/.ssh/id_ed25519"), Access::Read).allowed);
        assert!(
            !widened
                .decide(Path::new("/h/Library/Keychains/login.keychain-db"), Access::Read)
                .allowed
        );
        assert!(!widened.keychain);
        assert!(widened.decide(Path::new("/h/.zshrc"), Access::Write).allowed);
    }

    /// The launch with `lift` added where `resolve` puts the user's grants: after the first rule (the workspace),
    /// before every deny; a connect also after the credential denies, as `resolve` adds one inside a credential folder.
    /// The resolution itself is tested in `resolve.rs`.
    fn with_lift(launch: &Policy, lift: &Lift) -> Policy {
        let (access, target) = match lift {
            Lift::Write(p) => (Access::Write, Target::Subpath { path: p.clone() }),
            Lift::Connect(p) => (Access::Connect, Target::Subpath { path: p.clone() }),
            Lift::Bind(p) => (Access::Bind, Target::Subpath { path: p.clone() }),
            Lift::Service(name) => {
                let mut policy = launch.clone();
                match &mut policy.services {
                    Services::Every { .. } => {}
                    Services::Listed { grants } => grants.push(crate::launch::sandbox::ServiceGrant {
                        key: ServiceKey::Name(name.clone()),
                        effect: Effect::Allow,
                        source: Source::Workspace,
                    }),
                }
                return policy;
            }
            Lift::Network(to) => {
                let mut policy = launch.clone();
                if let Network::Filtered { grants, .. } = &mut policy.network {
                    grants.push(crate::launch::sandbox::NetworkGrant {
                        target: to.clone(),
                        effect: Effect::Allow,
                        source: Source::Workspace,
                    });
                }
                return policy;
            }
            Lift::TcpListen => {
                return Policy {
                    listen: Listen::Open {
                        source: Source::Workspace,
                    },
                    ..launch.clone()
                };
            }
        };
        let mut policy = launch.clone();
        policy.rules.insert(
            1,
            Rule {
                effect: Effect::Allow,
                access: vec![access],
                target: target.clone(),
                source: Source::Workspace,
            },
        );
        match lift {
            Lift::Connect(_) => {
                let after_credentials = policy
                    .rules
                    .iter()
                    .rposition(|r| matches!(r.source, Source::NonoGroup { .. }))
                    .map_or(policy.rules.len(), |i| i + 1);
                policy.rules.insert(
                    after_credentials,
                    Rule {
                        effect: Effect::Allow,
                        access: vec![Access::Connect],
                        target,
                        source: Source::Workspace,
                    },
                );
            }
            Lift::Write(_) | Lift::Bind(_) | Lift::Service(_) | Lift::Network(_) | Lift::TcpListen => {}
        }
        policy
    }

    /// The temp folders a test's bounds name as shared.
    fn shared() -> Vec<PathBuf> {
        ["/private/tmp", "/tmp", "/private/var/folders", "/var/folders"]
            .map(PathBuf::from)
            .to_vec()
    }

    fn lift_with(
        launch: &Policy,
        refused: Refused,
        projects: &[&Path],
        credentials: Credentials,
    ) -> Result<Offer, NoLift> {
        let shared = shared();
        let bounds = Bounds {
            projects,
            home: Path::new("/h"),
            shared: &shared,
            credentials,
        };
        lift_for(launch, refused, &bounds, |lift, _| Ok(with_lift(launch, lift)))
    }

    fn lift_in(launch: &Policy, op: &str, target: &str, projects: &[&Path]) -> Result<Offer, NoLift> {
        let refused = Refused {
            operation: op,
            target,
            pid: 4242,
        };
        lift_with(launch, refused, projects, Credentials::Liftable)
    }

    fn only(lift: Lift) -> Offer {
        Offer {
            project: lift.clone(),
            everywhere: lift,
        }
    }

    /// The narrowest entry per operation, a folder for a folder's create; what a deny decides is never offered, and is
    /// named by the deny that decides it.
    #[test]
    fn lifts_for_refusals() {
        let launch = launch();
        let lift = |op: &str, target: &str| lift_in(&launch, op, target, &[Path::new("/w")]);
        let decided = |by| Err(NoLift::Decided(by));
        // A path that doesn't exist (or a folder): `write` on it, so what's created inside isn't blocked again.
        assert_eq!(
            lift("file-write-create", "/h/.cache/x"),
            Ok(only(Lift::Write("/h/.cache/x".into())))
        );
        assert_eq!(
            lift("file-write-data", "/h/.cache/x"),
            Ok(only(Lift::Write("/h/.cache/x".into()))),
            "an entry covers a file too"
        );
        assert_eq!(
            lift("network-outbound", "/tmp/x.sock"),
            Ok(only(Lift::Connect("/tmp/x.sock".into())))
        );
        assert_eq!(
            lift("network-outbound", "/h/run/x-1.sock"),
            Ok(only(Lift::Connect("/h/run/x-1.sock".into()))),
            "another number isn't the process's own"
        );
        assert_eq!(
            lift("network-bind", "/w/x.sock"),
            Ok(only(Lift::Bind("/w/x.sock".into())))
        );
        // A socket in a credential folder: its `[sandbox.network.unix_socket_connections]` entry opens it (for
        // connecting only), so it's offered.
        assert_eq!(
            lift("network-outbound", "/h/.ssh/agent.sock"),
            Ok(only(Lift::Connect("/h/.ssh/agent.sock".into())))
        );
        // Creating one there isn't: nothing lifts it.
        assert_eq!(
            lift("network-bind", "/h/.ssh/agent.sock"),
            decided(DeniedBy::Credential(Access::Bind))
        );
        // The user's deny and the daemon's state still decide a connect.
        assert_eq!(
            lift("network-outbound", "/h/secret/s.sock"),
            decided(DeniedBy::UserDeny)
        );
        assert_eq!(
            lift("network-outbound", "/h/.local/state/piton/piton.sock"),
            decided(DeniedBy::Floor)
        );
        // Git's control files, after every user entry: reported, with `allow_git_control_files`.
        assert_eq!(
            lift("file-write-unlink", "/w/.git/config"),
            decided(DeniedBy::GitControl)
        );
        // Credentials: never offered, fnox or not.
        assert_eq!(
            lift("file-read-data", "/h/.ssh/id_ed25519"),
            decided(DeniedBy::Credential(Access::Read))
        );
        // The keychain is its own case: readable with fnox off.
        assert_eq!(
            lift("mach-lookup", "com.apple.SecurityServer"),
            decided(DeniedBy::Keychain(KeychainBlock::Readable))
        );
        assert_eq!(
            lift("file-read-data", "/h/Library/Keychains/login.keychain-db"),
            decided(DeniedBy::Keychain(KeychainBlock::Readable))
        );
        // ssh appending to known_hosts: a write, which `read` wouldn't lift.
        assert_eq!(
            lift("file-write-data", "/h/.ssh/known_hosts"),
            decided(DeniedBy::Credential(Access::Write))
        );
        assert_eq!(lift("file-read-data", "/h/secret/x"), decided(DeniedBy::UserDeny));
        assert_eq!(
            lift("file-write-create", "/w/.pi/settings.json"),
            decided(DeniedBy::Floor)
        );
        assert_eq!(lift("file-write-create", "/h/.pi/agent/x"), decided(DeniedBy::Floor));
        // A Mach service outside the list: that service, in both scopes; a name no entry can hold isn't offered.
        assert_eq!(
            lift("mach-lookup", "com.example.agent"),
            Ok(only(Lift::Service(crate::config::ServiceName::Exact(
                "com.example.agent".into()
            ))))
        );
        assert_eq!(lift("mach-lookup", "bad name"), Err(NoLift::Unsupported));
        assert_eq!(lift("user-preference-write", "com.x"), Err(NoLift::Unsupported));
        // A TCP listen (pi's OAuth callback, an MCP server's): `allow_tcp_listening = true` in both scopes.
        assert_eq!(lift("network-inbound", "local:*:50958"), Ok(only(Lift::TcpListen)));
        // An inbound refusal that isn't a listen isn't offered.
        assert_eq!(lift("network-inbound", "remote:*:53"), Err(NoLift::Unsupported));
    }

    /// How each entry is written: a dotted key under `[sandbox]`, a path quoted, and its value.
    #[test]
    fn entries_are_written_as_toml() {
        assert_eq!(
            Lift::Write("/x".into()).entry().to_string(),
            r#"filesystem."/x" = "write""#
        );
        assert_eq!(
            Lift::Bind("/x".into()).entry().to_string(),
            r#"network.unix_socket_bindings."/x" = "allow""#
        );
        assert_eq!(
            Lift::Network(NetworkTarget::try_from("a.example:443".to_string()).unwrap())
                .entry()
                .to_string(),
            r#"network.connections."a.example:443" = "allow""#
        );
        assert_eq!(Lift::TcpListen.entry().to_string(), "allow_tcp_listening = true");
        assert_eq!(Lift::TcpListen.entries().allow_tcp_listening, Some(true));
        assert_eq!(Lift::Write("/x".into()).entry().path(), ["sandbox", "filesystem", "/x"]);
    }

    /// A block the user's own entry decides (a `false`, a service deny) is never offered; `all` keeps it closed.
    #[test]
    fn the_users_own_entries_are_never_offered_or_lifted() {
        let user = Source::Config {
            key: "sandbox.allow_tcp_listening".into(),
            from: Provenance::File {
                path: "/g/config.toml".into(),
            },
        };
        let mut launch = launch();
        launch.listen = Listen::Denied { source: user.clone() };
        launch.apps = Apps::Denied { source: user.clone() };
        launch.services = Services::Listed {
            grants: vec![crate::launch::sandbox::ServiceGrant {
                key: ServiceKey::try_from("com.example.*".to_string()).unwrap(),
                effect: Effect::Deny,
                source: user,
            }],
        };
        let lift = |op: &str, target: &str| lift_in(&launch, op, target, &[Path::new("/w")]);
        let decided = Err(NoLift::Decided(DeniedBy::UserDeny));
        assert_eq!(lift("network-inbound", "local:*:50958"), decided);
        assert_eq!(lift("mach-lookup", "com.example.agent"), decided);
        let widened = all_below(&launch, Credentials::Liftable, &roots());
        assert!(!widened.decide_listen().allowed);
        assert!(matches!(widened.apps, Apps::Denied { .. }));
        assert!(!widened.decide_service("com.example.agent").allowed);
        assert!(widened.decide_service("org.other").allowed);
        assert_eq!(command_block(&launch, &widened, "lsopen", ""), CommandBlock::Floor);
    }

    /// An entry the resolved policy doesn't let through isn't offered: the deny that still decides is named.
    #[test]
    fn an_entry_that_doesnt_lift_is_never_offered() {
        let launch = launch();
        let refused = Refused {
            operation: "file-write-create",
            target: "/h/.cache/x",
            pid: 4242,
        };
        let shared = shared();
        let bounds = Bounds {
            projects: &[],
            home: Path::new("/h"),
            shared: &shared,
            credentials: Credentials::Liftable,
        };
        let unchanged = |_: &Lift, _| Ok(launch.clone());
        assert_eq!(
            lift_for(&launch, refused, &bounds, unchanged),
            Err(NoLift::Decided(DeniedBy::Floor))
        );
        let unresolvable = |_: &Lift, _| Err("$X isn't set".to_string());
        assert_eq!(
            lift_for(&launch, refused, &bounds, unresolvable),
            Err(NoLift::Unsupported)
        );
    }

    /// With fnox on, a keychain block says it stays closed for fnox: no entry, whatever the access.
    #[test]
    fn the_keychain_stays_closed_behind_fnox() {
        let launch = launch();
        for (operation, target) in [
            ("mach-lookup", "com.apple.SecurityServer"),
            ("mach-lookup", "com.apple.secd"),
            ("file-read-data", "/h/Library/Keychains/login.keychain-db"),
        ] {
            let refused = Refused {
                operation,
                target,
                pid: 4242,
            };
            assert_eq!(
                lift_with(&launch, refused, &[], Credentials::Floor),
                Err(NoLift::Decided(DeniedBy::Keychain(KeychainBlock::ClosedForFnox))),
                "{target}"
            );
        }
    }

    /// A path named after the refusing process is offered as its folder when that is safe and lifts it; under temp,
    /// the home folder, or the root it never is.
    #[test]
    fn a_process_s_own_name_is_never_offered() {
        let launch = launch();
        let lift =
            |operation, target, pid| lift_with(&launch, Refused { operation, target, pid }, &[], Credentials::Liftable);
        let per_process = |folder: &str| Err(NoLift::PerProcess { folder: folder.into() });
        // Claude Code's sockets: `/tmp/cc-socks` reaches every interactive Claude Code on the host.
        assert_eq!(
            lift("network-outbound", "/private/tmp/cc-socks/64421.sock", 64421),
            per_process("/private/tmp/cc-socks")
        );
        assert_eq!(
            lift("network-outbound", "/var/folders/x/T/nvim.Alex/a1/nvim.64421.0", 64421),
            per_process("/var/folders/x/T/nvim.Alex/a1")
        );
        // A folder of the process's own, deeper: the folder above the outermost own component.
        assert_eq!(
            lift("file-write-create", "/h/.cache/tool/64421/x.lock", 64421),
            Ok(only(Lift::Write("/h/.cache/tool".into())))
        );
        assert_eq!(
            lift("network-outbound", "/h/run/app/64421.sock", 64421),
            Ok(only(Lift::Connect("/h/run/app".into())))
        );
        // Directly in the home folder's folders: too broad.
        assert_eq!(
            lift("network-outbound", "/h/run/64421.sock", 64421),
            per_process("/h/run")
        );
        // Another pid's name is a plain path.
        assert_eq!(
            lift("network-outbound", "/h/run/app/64421.sock", 777),
            Ok(only(Lift::Connect("/h/run/app/64421.sock".into())))
        );
    }

    /// "Everywhere" saves the folder above a path's per-project part, named as pi and Claude Code name per-folder state;
    /// never by a name's substring, and never a folder too broad to grant everywhere.
    #[test]
    fn everywhere_saves_the_parent_of_the_project_part() {
        let launch = launch();
        let project = Path::new("/h/Projects/cli");
        let lift = |target: &str| lift_in(&launch, "file-write-create", target, &[project]);
        assert_eq!(
            lift("/h/Library/Caches/claude-cli-nodejs/-h-Projects-cli"),
            Ok(Offer {
                project: Lift::Write("/h/Library/Caches/claude-cli-nodejs/-h-Projects-cli".into()),
                everywhere: Lift::Write("/h/Library/Caches/claude-cli-nodejs".into()),
            })
        );
        // Deeper inside the per-project folder: still its parent.
        assert_eq!(
            lift("/h/Library/Caches/claude-cli-nodejs/-h-Projects-cli/logs/x")
                .unwrap()
                .everywhere,
            Lift::Write("/h/Library/Caches/claude-cli-nodejs".into())
        );
        // A checkout named `cli` doesn't cut `claude-cli-nodejs`.
        let unrelated = "/h/Library/Caches/claude-cli-nodejs/other";
        assert_eq!(lift(unrelated), Ok(only(Lift::Write(unrelated.into()))));
        // A sibling of the checkout: never cut to the folder holding projects.
        let sibling = "/h/Projects/cli-build/x";
        assert_eq!(lift(sibling), Ok(only(Lift::Write(sibling.into()))));
        // Directly under home, or holding the project: too broad.
        assert_eq!(
            everywhere_folder(Path::new("/h/.cache/-h-Projects-cli/x"), &[project], Path::new("/h")),
            None
        );
        assert_eq!(
            everywhere_folder(Path::new("/h/Projects/-h-Projects-cli"), &[project], Path::new("/h")),
            None
        );
    }
}
