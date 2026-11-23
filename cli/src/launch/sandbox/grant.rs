//! What a command the user approved runs with: the launch's policy widened by a per-command grant (`all`, or the
//! `[sandbox]` lists of a narrowed one), never past the floor.
//!
//! The floor is what no approval lifts: protected files, every configuration level and pi's agent directory (what a
//! later pi or Piton loads, so a lifted command could grant the agent more), the daemon's state and control socket, the
//! user's own `deny`, and the credential denies while fnox is on (the sandbox could run `fnox get`). Git's hooks and
//! configuration and the credential denies otherwise are liftable: `husky install` and Git's keychain sign-in work once
//! approved. So are Mach services and launching apps: an approved `open https://…` opens the page (what `open` may
//! name is checked before the question, `agent/open.rs`).

use std::path::{Path, PathBuf};

use super::{
    policy::{self, Source},
    rules::{Access, Apps, Effect, Policy, Rule, Services, Target},
};
use crate::config::{
    AllGrant, CommandGrant, CommandRuntime, Grant, GrantLists, LaunchConfig, Listed, ServiceEntry, ServiceName,
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
    resolve: std::sync::Arc<ResolveFn>,
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

    /// The launch's policy with `lists` added as if `file` listed them.
    pub fn with(&self, lists: &GrantLists, file: &Path) -> Result<Policy, String> {
        (self.resolve)(&widened_config(&self.config, lists, file))
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
    Sandbox(Result<Policy, String>),
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
            bind: lists.bind.clone(),
            services: lists.services.clone(),
            presets: lists.presets.clone(),
            ..crate::config::SandboxTable::default()
        },
        file,
    );
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
                CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => EntryRuntime::Sandbox(Ok(widened.clone())),
                CommandRuntime::Sandbox(Grant::Lists(lists)) => {
                    EntryRuntime::Sandbox(resolver.with(lists, &entry.file))
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    // nono validates file rules only: a socket allow on `/` is accepted (yolo's `connect = ["/"]` emits it too).
    rules.push(Rule {
        effect: Effect::Allow,
        access: vec![Access::Connect, Access::Bind],
        target: Target::Subpath { path: "/".into() },
        source: source(),
    });
    rules.extend(policy.rules.iter().filter(|r| kept(r, credentials)).cloned());
    Policy {
        rules,
        keychain: match credentials {
            Credentials::Liftable => true,
            Credentials::Floor => policy.keychain,
        },
        services: Services::Every { source: source() },
        apps: Apps::Open { source: source() },
        network: policy.network.clone(),
        tag: policy.tag.clone(),
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
            // What fnox loads: one edit could send secrets anywhere.
            Source::FnoxConfig => true,
            // The user's own `deny`.
            Source::Config { .. } => true,
            // No such deny exists; kept, so a new one is closed until it's classified here.
            Source::Workspace | Source::BuiltIn { .. } | Source::AgentDir | Source::Preset { .. } => true,
        },
    }
}

/// The `[sandbox]` entry that lifts a refusal of a process that isn't a bash call (an MCP server, an extension, pi).
/// Persisted as the user's own entry, so it applies to the whole session after a restart. Reads are never offered:
/// the base reads everything, so a process's read is refused only by a credential, the user's `deny`, or the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lift {
    /// `write = [path]`: a folder and everything below it.
    Write(PathBuf),
    /// `write_file = [path]`: exactly the path.
    WriteFile(PathBuf),
    /// `connect = [path]`: a Unix socket.
    Connect(PathBuf),
    /// `bind = [path]`: where a Unix socket is created.
    Bind(PathBuf),
    /// `services = [name]`: one Mach service.
    Service(ServiceName),
}

impl Lift {
    /// The key and value as written in `[sandbox]`.
    pub fn entry(&self) -> (&'static str, String) {
        match self {
            Lift::Write(path) => ("write", path.display().to_string()),
            Lift::WriteFile(path) => ("write_file", path.display().to_string()),
            Lift::Connect(path) => ("connect", path.display().to_string()),
            Lift::Bind(path) => ("bind", path.display().to_string()),
            Lift::Service(name) => ("services", name.to_string()),
        }
    }

    /// What a `silence` entry for the same block names: the path, or the service.
    pub fn silence_target(&self) -> crate::config::SilenceTarget {
        match self {
            Lift::Write(path) | Lift::WriteFile(path) | Lift::Connect(path) | Lift::Bind(path) => {
                crate::config::SilenceTarget::Path(path.display().to_string())
            }
            Lift::Service(name) => crate::config::SilenceTarget::Service(name.to_string()),
        }
    }

    /// The `[sandbox]` lists with only this entry: what [`widened_config`] adds.
    pub fn lists(&self) -> GrantLists {
        let mut lists = GrantLists::default();
        match self {
            Lift::Write(path) => lists.write.push(path.display().to_string()),
            Lift::WriteFile(path) => lists.write_file.push(path.display().to_string()),
            Lift::Connect(path) => lists
                .connect
                .push(crate::config::Connect::Socket(path.display().to_string())),
            Lift::Bind(path) => lists.bind.push(path.display().to_string()),
            Lift::Service(name) => lists.services.push(ServiceEntry::Name(name.clone())),
        }
        lists
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
    /// Git's hooks and configuration: the `git` preset opens them.
    GitControl,
    /// The user's own `deny`.
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
        Effect::Deny => Some(match &rule.target {
            Target::GitControl { .. } => DeniedBy::GitControl,
            Target::Protected { .. } => DeniedBy::Floor,
            Target::Subpath { .. } | Target::Literal { .. } | Target::Prefix { .. } => match &rule.source {
                Source::NonoGroup { .. } => DeniedBy::Credential(access),
                Source::Config { .. } => DeniedBy::UserDeny,
                Source::FnoxConfig => DeniedBy::FnoxConfig,
                Source::Piton { .. }
                | Source::Workspace
                | Source::BuiltIn { .. }
                | Source::AgentDir
                | Source::Preset { .. } => DeniedBy::Floor,
            },
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
    let (access, project) = match operation {
        "mach-lookup" if policy::KEYCHAIN_TARGETS.contains(&refused.target) => {
            return Err(keychain(bounds.credentials));
        }
        // One service, the same entry in both scopes; offered only when the widened policy opens it.
        "mach-lookup" => {
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
        "file-write-create" | "file-write-mkdir" if absolute && !target.is_file() => {
            (Access::Write, Lift::Write(target.to_path_buf()))
        }
        op if op.starts_with("file-write") && absolute => (Access::Write, Lift::WriteFile(target.to_path_buf())),
        "network-outbound" if absolute => (Access::Connect, Lift::Connect(target.to_path_buf())),
        "network-bind" if absolute => (Access::Bind, Lift::Bind(target.to_path_buf())),
        _ => return Err(NoLift::Unsupported),
    };
    // A deny rule decides it: every user entry comes before the denies, so none would beat it. Except a connect inside
    // a credential folder, which a `connect` entry naming it opens: the widened check below decides that one.
    if let Some(denied) = denied_by(policy, target, access) {
        match (access, &denied) {
            (Access::Connect, DeniedBy::Credential(_)) => {}
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
    /// An operation or target the policy model doesn't know.
    Unsupported,
}

/// Classifies the kernel's refusal of `operation` on `target` in a call that ran under `launch`, against `all` (the
/// launch widened to everything but the floor).
pub fn command_block(launch: &Policy, all: &Policy, operation: &str, target: &str) -> CommandBlock {
    let lifted = |allowed: bool| match allowed {
        true => CommandBlock::Liftable,
        false => CommandBlock::Floor,
    };
    match operation {
        "lsopen" | "appleevent-send" => {
            return match all.apps {
                Apps::Open { .. } => CommandBlock::Liftable,
                Apps::Closed => CommandBlock::Floor,
            };
        }
        "mach-lookup" => return lifted(all.decide_service(target).allowed),
        _ => {}
    }
    let path = Path::new(target);
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

    fn piton(what: &str) -> Source {
        Source::Piton { what: what.into() }
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
            services: Services::Listed { grants: Vec::new() },
            apps: Apps::Closed,
            network: Vec::new(),
            tag: String::new(),
        }
    }

    /// `all` opens everything the launch closes except the floor, which still decides wherever it applies.
    /// Mach services and launching apps open too; the keychain's services stay closed behind fnox.
    #[test]
    fn all_opens_services_and_apps() {
        let widened = all_below(&launch(), Credentials::Liftable, &roots());
        assert!(widened.decide_service("com.example.agent").allowed);
        assert!(widened.decide_service("com.apple.SecurityServer").allowed, "fnox off");
        assert!(matches!(widened.apps, Apps::Open { .. }));
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
            Lift::WriteFile(p) => (Access::Write, Target::Literal { path: p.clone() }),
            Lift::Connect(p) => (Access::Connect, Target::Subpath { path: p.clone() }),
            Lift::Bind(p) => (Access::Bind, Target::Subpath { path: p.clone() }),
            Lift::Service(name) => {
                let mut policy = launch.clone();
                match &mut policy.services {
                    Services::Every { .. } => {}
                    Services::Listed { grants } => grants.push(crate::launch::sandbox::ServiceGrant {
                        name: name.clone(),
                        source: Source::Workspace,
                    }),
                }
                return policy;
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
            Lift::Write(_) | Lift::WriteFile(_) | Lift::Bind(_) | Lift::Service(_) => {}
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
            Ok(only(Lift::WriteFile("/h/.cache/x".into())))
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
        // A socket in a credential folder: its `connect` entry opens it (for connecting only), so it's offered.
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
        // Git's control files, after every user entry: reported, with the `git` preset.
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
