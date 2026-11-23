//! One launch's [`Policy`]: Piton's base policy, the profile's entries, the `[sandbox]` chain's lists, and the launch's
//! facts (workspace, agent directory, the daemon's state folder), as an ordered list of literal rules.
//!
//! Order, the last matching rule deciding:
//! 1. grants: writes to the workspace, the source's `.piton/.local`, temp, presets, `write`, `write_file`; DNS; and
//!    `connect` and `bind` sockets, before every deny so none of them reopens one (the base closes every socket);
//! 2. nono's credential denies; then `read`, which lifts them for reading (and reopens writes granted inside a read
//!    path); then `connect` socket entries at or inside a credential folder, for connecting only;
//! 3. the host agent directory is read-only; the launch's agent directory's runtime state is writable;
//! 4. the user's `deny`; the files fnox loads; each socket a `connect` entry names, and the folders on its way, kept
//!    in place;
//! 5. every configuration level, and what its entries link to, is read-only; protected files are read-only; so are
//!    Git's hooks and configuration in the launch's checkouts;
//! 6. the daemon's state folder is closed (reads, writes, connects) except its logs and the agent socket, and its
//!    control socket can't be connected to, wherever it is.
//!
//! Seatbelt compares real paths, so each rule also gets its real-path form. A deny's follows every symlink: it closes
//! what a link points to as well. An allow's follows only links no sandboxed session of the project can replace
//! (`/var` is `/private/var`): one it could replace would let it move the grant wherever it likes.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

use super::{
    SandboxError,
    policy::{self, AgentDirWrite, Provenance, Source},
    rules::{Access, Apps, Effect, Gate, Network, NetworkGrant, Policy, Rule, ServiceGrant, Services, Target},
};
use crate::{
    agent::{GitControl, canonical_target, lexical},
    config::{
        ClaudeCodeOptions, ClaudeLogin, Connect, LOCAL_DIR, LaunchConfig, Listed, Preset, Profile, ServiceEntry,
        ServiceName, expand_home,
    },
    launch::{
        Env,
        env::{Origin, SandboxEnv, ToolSource, Widening},
    },
};

/// What a launch is, beyond its configuration.
pub struct Facts<'a> {
    pub home: &'a Path,
    /// The daemon's base environment: `$VAR` grants and the host agent directory come from it, never from the
    /// workspace's (a repository's `mise.toml` can set variables).
    pub base_env: &'a Env,
    /// pi's environment for this launch, with where each value came from: the agent directory pi will use.
    pub launch_env: &'a SandboxEnv,
    /// The workspace pi runs in.
    pub workspace: &'a Path,
    /// The daemon's state folder (`agent.sock`, `piton.sock`, the database, `logs/`).
    pub state_dir: &'a Path,
    /// The daemon's control socket: clients only, never an agent.
    pub control_socket: &'a Path,
    pub agent_socket: &'a Path,
    pub logs: &'a Path,
    /// The temp and device folders every launch may write: [`policy::WRITE_ROOTS`] (tests pass none, since their
    /// folders are temp folders).
    pub write_roots: &'a [&'a str],
    /// The files fnox loads for this launch (`fnox config-files`), when its credential proxy applies: closed to writes.
    pub fnox_files: &'a [PathBuf],
    /// The session's own folder for the `pi-claude-code` preset's sockets, when the launch gives it one (a session's
    /// launch; not `piton sandbox show`). Used only when the user sets neither `XDG_RUNTIME_DIR` nor
    /// `CLAUDE_CODE_TMPDIR`.
    pub claude_sockets: Option<&'a Path>,
}

/// Why a configuration level is closed, and how it's changed: what `piton sandbox why` prints.
const CONFIG_LEVEL_DENY: &str = "a configuration level: changed only through a write the user approves (the edit or \
                                 write tool)";

/// Why pi's agent directory is closed, and how it's changed.
const AGENT_DIR_DENY: &str = "pi's agent directory: extensions, settings, and packages pi loads; changed only through \
                              a write the user approves (the edit or write tool)";

/// Why a folder on the way to a configuration level, an agent directory, or a link target can't be renamed or replaced.
const FIXED_FOLDER_DENY: &str = "a folder on the way to configuration: renaming or replacing it would move what the \
                                 user approves (what's in it follows the other rules)";

/// Why Git's hooks and configuration are closed, and how to open them: what `piton sandbox why` prints.
const GIT_CONTROL_DENY: &str = "Git's hooks and configuration: Git on the host runs what they name; the git preset \
                                opens them (global config.toml or a config.local.toml)";

/// Why a socket a `connect` entry names, or a folder on its way, can't be written.
const SOCKET_PATH_DENY: &str = "a socket a connect entry names, or a folder on its way: kept in place so no other \
                                socket can be moved there";

/// The socket macOS resolves names through.
const DNS_SOCKET: &str = "/var/run/mDNSResponder";

/// The most symlinks one path's resolution follows, as the kernel's limit; past it, the rest is kept as written.
const MAX_LINKS: usize = 40;

/// A launch's policy, and the grants it doesn't resolve as their paths suggest.
#[derive(Debug)]
pub struct Resolution {
    pub policy: Policy,
    pub links: Vec<UnfollowedLink>,
    /// The profile in effect, and the file that chose it (`None`: the default).
    pub profile: ProfileChoice,
    /// The `pi-claude-code` preset, when it's on: what became of its config folder, and how `claude` signs in.
    pub claude_code: Option<ClaudeCode>,
    /// `[sandbox] silence` and the presets' entries, expanded: the blocks they match are only logged.
    pub silence: Vec<Silenced>,
    /// pi's agent directories the policy closes, as named: the host's and the launch's (the same folder when the
    /// launch uses the host's). Protected files in them are changed through a write the user approves.
    pub agent_dirs: [PathBuf; 2],
}

/// One `silence` entry, expanded, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silenced {
    pub target: Silent,
    /// Only blocks of processes with this name.
    pub process: Option<String>,
    pub source: SilenceSource,
}

/// What a silence entry matches, expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Silent {
    /// A path and everything below it, absolute and canonical; `*` and `**` components are wildcards.
    Path(PathBuf),
    /// A Mach service, by name.
    Service(String),
    /// A network destination: a host (or `*.domain`) and a port.
    Host(crate::config::NetworkTarget),
}

/// Where a silence entry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SilenceSource {
    /// The user's `[sandbox] silence`, in this file.
    File(PathBuf),
    /// A preset's expected blocks.
    Preset(&'static str),
}

impl std::fmt::Display for SilenceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SilenceSource::File(path) => write!(f, "sandbox.silence, listed in {}", path.display()),
            SilenceSource::Preset(preset) => write!(f, "the {preset} preset"),
        }
    }
}

impl Silent {
    /// A non-path `silence` target as it matches (a path is expanded and canonicalized by the launch first); `None` for a
    /// path.
    pub fn named(target: &crate::config::SilenceTarget) -> Option<Self> {
        match target {
            crate::config::SilenceTarget::Path(_) => None,
            crate::config::SilenceTarget::Service(name) => Some(Silent::Service(name.clone())),
            crate::config::SilenceTarget::Host(text) => match crate::config::Connect::try_from(text.clone()) {
                Ok(crate::config::Connect::Network(target)) => Some(Silent::Host(target)),
                // Parsing accepted it as a host and port.
                Ok(crate::config::Connect::Socket(_)) | Err(_) => None,
            },
        }
    }
}

impl Silenced {
    /// Whether this entry matches a block: `process` refused `operation` on `target`, as the kernel names them.
    pub fn matches(&self, process: &str, operation: &str, target: &str) -> bool {
        if self.process.as_deref().is_some_and(|p| p != process) {
            return false;
        }
        match &self.target {
            Silent::Service(name) => operation == "mach-lookup" && target == name,
            Silent::Path(pattern) => {
                let path = Path::new(target);
                operation != "mach-lookup" && path.is_absolute() && below_pattern(path, pattern)
            }
            Silent::Host(host) => {
                operation == "network-outbound"
                    && !target.starts_with('/')
                    && super::Destination::parse(target).is_some_and(|to| host.allows(&to))
            }
        }
    }

    /// The entry as written in `[sandbox] silence`.
    pub fn entry(&self) -> crate::config::Silence {
        crate::config::Silence {
            target: match &self.target {
                Silent::Path(path) => crate::config::SilenceTarget::Path(path.display().to_string()),
                Silent::Service(name) => crate::config::SilenceTarget::Service(name.clone()),
                Silent::Host(host) => crate::config::SilenceTarget::Host(host.to_string()),
            },
            process: self.process.clone(),
        }
    }
}

/// Whether `path` is at or below `pattern`, comparing component by component: `*` matches one component, `**` any
/// number of them (none included).
fn below_pattern(path: &Path, pattern: &Path) -> bool {
    fn walk(path: &[Component], pattern: &[Component]) -> bool {
        let Some((first, rest)) = pattern.split_first() else {
            // The pattern is used up: the path is at or below it.
            return true;
        };
        match first.as_os_str().to_str() {
            Some("**") => (0..=path.len()).any(|skip| walk(&path[skip..], rest)),
            Some("*") => path.split_first().is_some_and(|(_, tail)| walk(tail, rest)),
            Some(_) | None => path
                .split_first()
                .is_some_and(|(head, tail)| head == first && walk(tail, rest)),
        }
    }
    let path: Vec<Component> = path.components().collect();
    let pattern: Vec<Component> = pattern.components().collect();
    walk(&path, &pattern)
}

/// The entry that silences a block of `process`, refused `operation` on `target` (as the kernel names it), if one does.
pub fn silenced<'a>(silence: &'a [Silenced], process: &str, operation: &str, target: &str) -> Option<&'a Silenced> {
    silence.iter().find(|s| s.matches(process, operation, target))
}

/// The `pi-claude-code` preset of a launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCode {
    pub folder: ClaudeCodeFolder,
    pub login: ClaudeLogin,
    /// `None` when the launch has no folder of its own to give it (`piton sandbox show`, the git sandbox): `claude`'s
    /// probes in the shared folder are then blocked as before.
    pub sockets: Option<ClaudeCodeSockets>,
}

/// The config folder `PI_CLAUDE_CODE_CONFIG_DIR` names, as the `pi-claude-code` preset found it. Only `Granted` is
/// writable; every other case starts the session with a warning, `claude`'s writes there meeting the block question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCodeFolder {
    /// Writable, as named.
    Granted(PathBuf),
    /// `PI_CLAUDE_CODE_CONFIG_DIR` isn't set: the extension's `claude` uses the interactive Claude Code's folder.
    Unset,
    /// It's the interactive Claude Code's folder (`$CLAUDE_CONFIG_DIR`, or `~/.claude`), whose hooks, plugins, and MCP
    /// servers run on the host.
    Interactive { path: PathBuf },
    /// It would open what the sandbox closes.
    Closed { path: PathBuf, reason: String },
    /// A file a sandboxed session can write set it, and it leads out of the checkout.
    AgentWritable {
        path: PathBuf,
        set_by: String,
        checkout: PathBuf,
    },
}

/// Where the preset's `claude` puts its messaging socket (`<folder>/cc-socks/<pid>.sock`): a folder of its own, so its
/// startup probe of a stale socket with its pid never reaches another process's. The folder is open for writing and
/// creating sockets, never for connecting: the agent can create entries there too, and a connect grant would reach
/// any host socket hard-linked into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeCodeSockets {
    /// The session's own folder, which the daemon creates, sets as `CLAUDE_CODE_TMPDIR`, and removes when the session
    /// ends.
    Own(PathBuf),
    /// The user's `XDG_RUNTIME_DIR` or `CLAUDE_CODE_TMPDIR`, as `claude` reads them (the first wins).
    Users { var: &'static str, path: PathBuf },
    /// The user's value, not opened: the session starts with a warning, and `claude`'s probes there are blocked.
    Refused {
        var: &'static str,
        path: PathBuf,
        reason: String,
    },
}

impl ClaudeCodeSockets {
    /// The folder the policy opens, if any.
    fn opened(&self) -> Option<&Path> {
        match self {
            ClaudeCodeSockets::Own(path) | ClaudeCodeSockets::Users { path, .. } => Some(path),
            ClaudeCodeSockets::Refused { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProfileChoice {
    pub name: Profile,
    pub set_in: Option<PathBuf>,
}

/// A grant whose path goes through a symlink a sandboxed session could have made, so the grant doesn't follow it: it
/// covers the path as written, which matches nothing while the link is there. A user who links a granted folder
/// elsewhere grants the real path instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfollowedLink {
    pub link: PathBuf,
    pub grant: PathBuf,
    pub source: Source,
}

impl std::fmt::Display for UnfollowedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.grant == self.link {
            true => "is a symlink".to_string(),
            false => format!("goes through the symlink {}", self.link.display()),
        };
        write!(
            f,
            "the sandbox grant {} {what}, which a sandboxed session could have made, so it isn't followed: grant the \
             real path instead",
            self.grant.display(),
        )
    }
}

/// The launch's policy, or why the launch is refused.
///
/// The `pi-claude-code` preset's folder is decided against the policy without it (whether the file that named it is
/// writable there), then added when it's accepted.
pub fn resolve(config: &LaunchConfig, facts: &Facts) -> Result<Resolution, SandboxError> {
    let grants = Grants::of(config, facts)?;
    let unopened = build(config, facts, &grants, None)?;
    let claude_code = grants.claude_code.as_ref().map(|(options, _)| ClaudeCode {
        folder: claude_code_folder(config, facts, &unopened),
        login: options.login(),
        sockets: claude_code_sockets(config, facts, &unopened),
    });
    let opened = match (&claude_code, grants.claude_code.as_ref()) {
        (Some(claude_code), Some((_, from))) => {
            let folder = match &claude_code.folder {
                ClaudeCodeFolder::Granted(folder) => Some(folder),
                ClaudeCodeFolder::Unset
                | ClaudeCodeFolder::Interactive { .. }
                | ClaudeCodeFolder::Closed { .. }
                | ClaudeCodeFolder::AgentWritable { .. } => None,
            };
            let sockets = claude_code.sockets.as_ref().and_then(ClaudeCodeSockets::opened);
            Some(ClaudeOpened { folder, sockets, from })
        }
        (Some(_), None) | (None, _) => None,
    };
    let built = match opened {
        Some(opened @ ClaudeOpened { folder: Some(_), .. }) | Some(opened @ ClaudeOpened { sockets: Some(_), .. }) => {
            build(config, facts, &grants, Some(opened))?
        }
        Some(ClaudeOpened {
            folder: None,
            sockets: None,
            ..
        })
        | None => unopened,
    };
    if let Some((value, origin)) = facts.launch_env.widening(Widening::AgentDir)
        && let Some(refusal) = widening_refusal(&built.agent, origin, &built.policy, config, None)
    {
        return Err(SandboxError::Widening {
            var: Widening::AgentDir.var(),
            value: value.to_string(),
            set_by: refusal.set_by,
            checkout: refusal.checkout,
        });
    }
    let profile = ProfileChoice {
        name: config.sandbox.profile(),
        set_in: config.sandbox.profile.as_ref().map(|p| p.file.clone()),
    };
    let mut silence = config
        .sandbox
        .silence
        .iter()
        .map(|l| {
            let target = match &l.value.target {
                crate::config::SilenceTarget::Path(path) => {
                    Silent::Path(canonical_pattern(&expand_grant(path, facts.home, facts.base_env)?))
                }
                other @ (crate::config::SilenceTarget::Service(_) | crate::config::SilenceTarget::Host(_)) => {
                    match Silent::named(other) {
                        Some(silent) => silent,
                        None => return Ok(None),
                    }
                }
            };
            Ok(Some(Silenced {
                target,
                process: l.value.process.clone(),
                source: SilenceSource::File(l.file.clone()),
            }))
        })
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, SandboxError>>()?;
    if let Some(claude_code) = &claude_code {
        silence.extend(claude_code_silence(claude_code));
    }
    Ok(Resolution {
        silence,
        policy: built.policy,
        links: built.links,
        profile,
        claude_code,
        agent_dirs: [built.host_agent, built.agent],
    })
}

/// A silence pattern's real-path form: the components before the first wildcard canonicalized, as Seatbelt reports
/// real paths; the rest as written.
fn canonical_pattern(path: &Path) -> PathBuf {
    let components: Vec<Component> = path.components().collect();
    let wild = components
        .iter()
        .position(|c| matches!(c.as_os_str().to_str(), Some("*" | "**")))
        .unwrap_or(components.len());
    let literal: PathBuf = components[..wild].iter().collect();
    components[wild..]
        .iter()
        .fold(canonical_target(&literal), |path, c| path.join(c))
}

/// The blocks the `pi-claude-code` preset expects, only logged. With the token login, `claude` looks for a stored login
/// in the keychain at every start, itself and through `security`, and signs in with the token when it finds none. In
/// its socket folder, `claude` probes a stale socket that has its pid (the extension kills it, so none is cleaned up)
/// and takes the name over when the probe fails.
fn claude_code_silence(claude_code: &ClaudeCode) -> Vec<Silenced> {
    let preset = |target, process: &str| Silenced {
        target,
        process: Some(process.to_string()),
        source: SilenceSource::Preset(CLAUDE_CODE_PRESET),
    };
    let mut silence: Vec<Silenced> = match claude_code.login {
        ClaudeLogin::Token => ["claude", "security"]
            .into_iter()
            .flat_map(|process| {
                policy::KEYCHAIN_TARGETS
                    .iter()
                    .map(move |service| preset(Silent::Service((*service).to_string()), process))
            })
            .collect(),
        // The keychain is open to it: nothing is blocked.
        ClaudeLogin::Keychain => Vec::new(),
    };
    if let Some(folder) = claude_code.sockets.as_ref().and_then(ClaudeCodeSockets::opened) {
        silence.push(preset(
            Silent::Path(canonical_target(&folder.join(CLAUDE_SOCKETS))),
            "claude",
        ));
    }
    silence
}

/// The folder `claude` puts its sockets in, inside its socket folder.
const CLAUDE_SOCKETS: &str = "cc-socks";

/// The longest socket path `claude` uses at its first choice; past it, it falls back to the shared
/// `/tmp/cc-socks-<uid>`.
const CLAUDE_SOCKET_MAX: usize = 103;

/// The longest socket name `claude` puts there: `<pid>.sock`, with macOS's largest pid.
const CLAUDE_SOCKET_NAME: &str = "99999.sock";

/// The `pi-claude-code` preset's socket folder: the user's `XDG_RUNTIME_DIR` or `CLAUDE_CODE_TMPDIR`, accepted like
/// `PI_CLAUDE_CODE_CONFIG_DIR`, else the session's own (`None` when the launch has none to give).
fn claude_code_sockets(config: &LaunchConfig, facts: &Facts, unopened: &Built) -> Option<ClaudeCodeSockets> {
    let users = [Widening::XdgRuntime, Widening::ClaudeCodeTmp]
        .into_iter()
        .find_map(|w| facts.launch_env.widening(w).map(|(value, origin)| (w, value, origin)));
    let Some((widening, value, origin)) = users else {
        return facts
            .claude_sockets
            .map(|own| ClaudeCodeSockets::Own(own.to_path_buf()));
    };
    let var = widening.var();
    let path = env_dir(value, facts.home, facts.workspace);
    let real = canonical_target(&path);
    let home = canonical_target(facts.home);
    let tmp = facts
        .base_env
        .get("TMPDIR")
        .filter(|t| !t.is_empty())
        .map(|t| canonical_target(Path::new(t)));
    let shared = policy::WRITE_ROOTS
        .iter()
        .map(|r| canonical_target(Path::new(r)))
        .chain(tmp)
        .find(|s| *s == real || s.starts_with(&real));
    let socket = real.join(CLAUDE_SOCKETS).join(CLAUDE_SOCKET_NAME);
    let reason = if real.parent().is_none() || real == home || real.parent() == Some(home.as_path()) {
        Some("it's the root, the home folder, or a folder directly in it".to_string())
    } else if let Some(shared) = shared {
        Some(format!(
            "it's {} or contains it, where other processes keep their sockets",
            shared.display()
        ))
    } else if socket.as_os_str().len() > CLAUDE_SOCKET_MAX {
        Some(format!(
            "claude's socket there would be longer than {CLAUDE_SOCKET_MAX} bytes, so it would use the shared \
             /tmp/cc-socks-<uid> instead"
        ))
    } else {
        closed_reason(&real, config, facts, &unopened.denied).or_else(|| {
            widening_refusal(&path, origin, &unopened.policy, config, Some(&path)).map(|r| {
                format!(
                    "it was set by {}, which a sandboxed session can write, and it leads out of the checkout {}",
                    r.set_by,
                    r.checkout.display()
                )
            })
        })
    };
    Some(match reason {
        Some(reason) => ClaudeCodeSockets::Refused { var, path, reason },
        None => ClaudeCodeSockets::Users { var, path },
    })
}

/// What the `pi-claude-code` preset opens, and where it came from.
#[derive(Clone, Copy)]
struct ClaudeOpened<'a> {
    /// Its config folder, when it's granted.
    folder: Option<&'a PathBuf>,
    /// Its socket folder, when it's opened.
    sockets: Option<&'a Path>,
    from: &'a Provenance,
}

/// One build of a launch's policy, and what the checks after it need.
struct Built {
    policy: Policy,
    links: Vec<UnfollowedLink>,
    /// The credential denies and the user's, expanded.
    denied: Vec<PathBuf>,
    /// pi's agent directories, as named: the host's and the launch's.
    host_agent: PathBuf,
    agent: PathBuf,
}

/// The policy, with the `pi-claude-code` preset's config and socket folders open when they're given.
fn build(
    config: &LaunchConfig,
    facts: &Facts,
    grants: &Grants,
    claude_code: Option<ClaudeOpened>,
) -> Result<Built, SandboxError> {
    let expand = |grant: &str| expand_grant(grant, facts.home, facts.base_env);
    let mut rules = Rules::default();

    // 1. Writes.
    rules.push(
        Effect::Allow,
        &[Access::Write],
        subpath(facts.workspace),
        Source::Workspace,
    );
    let artifacts = config.source.join(LOCAL_DIR);
    rules.push(
        Effect::Allow,
        &[Access::Write],
        subpath(&artifacts),
        Source::BuiltIn {
            what: "the source's workflow artifacts (.piton/.local)".into(),
        },
    );
    for root in facts.write_roots {
        rules.push(
            Effect::Allow,
            &[Access::Write],
            subpath(Path::new(root)),
            Source::BuiltIn {
                what: "temporary files and devices".into(),
            },
        );
    }
    let claude_source = claude_code.map(|c| Source::Preset {
        preset: CLAUDE_CODE_PRESET.into(),
        from: c.from.clone(),
    });
    if let (Some(folder), Some(source)) = (claude_code.and_then(|c| c.folder), &claude_source) {
        rules.push(Effect::Allow, &[Access::Write], subpath(folder), source.clone());
        // `claude` locks the folder itself with `proper-lockfile`: an empty `<folder>.lock` folder beside it.
        rules.push(
            Effect::Allow,
            &[Access::Write],
            subpath(&lock_of(folder)),
            source.clone(),
        );
    }
    let claude_sockets = claude_code.and_then(|c| c.sockets);
    if let (Some(sockets), Some(source)) = (claude_sockets, &claude_source) {
        // Its temp files and sockets; connecting stays closed (see `ClaudeCodeSockets`).
        rules.push(Effect::Allow, &[Access::Write], subpath(sockets), source.clone());
    }
    for (path, source) in &grants.write {
        rules.push(Effect::Allow, &[Access::Write], subpath(path), source.clone());
    }
    for (path, source) in &grants.write_file {
        rules.push(Effect::Allow, &[Access::Write], literal(path), source.clone());
    }
    // Sockets, before every deny: a `connect` or `bind` entry (yolo's `/` too) never reopens a credential, a user
    // deny, or the daemon's sockets. A connect entry inside a credential folder gets a second slot below.
    rules.push(
        Effect::Allow,
        &[Access::Connect],
        literal(Path::new(DNS_SOCKET)),
        Source::BuiltIn { what: "DNS".into() },
    );
    for (path, source) in &grants.sockets {
        rules.push(Effect::Allow, &[Access::Connect], subpath(path), source.clone());
    }
    for (path, source) in &grants.bind {
        rules.push(Effect::Allow, &[Access::Bind], subpath(path), source.clone());
    }
    if let (Some(sockets), Some(source)) = (claude_sockets, &claude_source) {
        rules.push(Effect::Allow, &[Access::Bind], subpath(sockets), source.clone());
    }

    // 2. Credentials, what `read` lifts of them (reading only), and the sockets named inside them.
    let all = [Access::Read, Access::Write, Access::Connect, Access::Bind];
    let mut denied = Vec::new();
    let mut credentials = Vec::new();
    for (path, source) in policy::nono_denies() {
        let path = expand(&path)?;
        rules.push(Effect::Deny, &all, subpath(&path), source);
        denied.push(path.clone());
        credentials.push(path);
    }
    for (path, source) in &grants.read {
        rules.push(Effect::Allow, &[Access::Read], subpath(path), source.clone());
    }
    // A `connect` entry at or inside a credential folder (Docker Desktop's socket, gpg-agent, 1Password's SSH agent)
    // opens just that path, for connecting: nothing there becomes readable. A broader entry (`/`, `~`) stays below the
    // credential denies.
    let inside = |path: &Path| {
        let real = canonical_target(path);
        credentials.iter().any(|c| {
            let root = subpath(c);
            let real_root = subpath(&canonical_target(c));
            root.covers(path) || real_root.covers(&real)
        })
    };
    for (path, source) in grants.sockets.iter().filter(|(p, _)| inside(p)) {
        rules.push(Effect::Allow, &[Access::Connect], subpath(path), source.clone());
    }
    let lifted = |p: &Path| grants.read.iter().any(|(r, _)| p.starts_with(r));
    for (path, source) in grants.write.iter().filter(|(p, _)| lifted(p)) {
        rules.push(Effect::Allow, &[Access::Write], subpath(path), source.clone());
    }
    for (path, source) in grants.write_file.iter().filter(|(p, _)| lifted(p)) {
        rules.push(Effect::Allow, &[Access::Write], literal(path), source.clone());
    }
    denied.extend(grants.deny.iter().map(|(p, _)| p.clone()));

    // 3. pi's agent directories: as named, so the runtime grants below resolve like every other grant.
    let host_agent = agent_dir(
        facts
            .base_env
            .get(Widening::AgentDir.var())
            .filter(|v| !v.is_empty())
            .map(String::as_str),
        facts.home,
        facts.workspace,
    );
    let launch_agent = facts.launch_env.widening(Widening::AgentDir);
    let agent = agent_dir(launch_agent.map(|(value, _)| value), facts.home, facts.workspace);
    check_agent_dir(
        &canonical_target(&agent),
        &canonical_target(&host_agent),
        config,
        facts,
        &denied,
    )?;
    rules.push(
        Effect::Deny,
        &[Access::Write],
        subpath(&host_agent),
        Source::Piton {
            what: AGENT_DIR_DENY.into(),
        },
    );
    if canonical_target(&agent) != canonical_target(&host_agent) {
        // A per-folder agent directory (even one inside the workspace) is loaded by host sessions in that folder too.
        rules.push(
            Effect::Deny,
            &[Access::Write],
            subpath(&agent),
            Source::Piton {
                what: AGENT_DIR_DENY.into(),
            },
        );
    }
    for write in policy::AGENT_DIR_WRITES {
        let target = match write {
            AgentDirWrite::Folder(name) => subpath(&agent.join(name)),
            AgentDirWrite::File(name) => literal(&agent.join(name)),
            AgentDirWrite::Lock(name) => subpath(&agent.join(format!("{name}.lock"))),
            AgentDirWrite::NamePrefix(name) => Target::Prefix { path: agent.join(name) },
        };
        rules.push(Effect::Allow, &[Access::Write], target, Source::AgentDir);
    }

    // 4. The user's denies.
    for (path, source) in &grants.deny {
        rules.push(Effect::Deny, &all, subpath(path), source.clone());
    }

    // 4b. What fnox loads: each file, and each folder entry on its path (renamed, removed, or replaced, a folder would
    // let the file be changed under another name). Denies follow links when finished, so the real path's are denied
    // too. What's in those folders stays as the other rules decide.
    for entry in crate::agent::fixed_entries(facts.fnox_files) {
        rules.push(Effect::Deny, &[Access::Write], literal(&entry), Source::FnoxConfig);
    }

    // 4c. Each socket a `connect` entry names, and each folder entry on its way, kept in place: otherwise a session
    // could move the real socket aside and another one (Neovim's) in its place. `$SSH_AUTH_SOCK` lives in `/private/tmp`,
    // which every launch writes. What's in a listed folder stays as the other rules decide.
    let named: Vec<PathBuf> = grants
        .sockets
        .iter()
        .map(|(p, _)| p.clone())
        .filter(|p| p.parent().is_some())
        .collect();
    for entry in crate::agent::fixed_entries(&named) {
        rules.push(
            Effect::Deny,
            &[Access::Write],
            literal(&entry),
            Source::Piton {
                what: SOCKET_PATH_DENY.into(),
            },
        );
    }

    // 5. Configuration and protected files: what the user edits by hand, changed only through a write they approve
    // (`agent/protected.rs`, the same definition).
    let config_level = Source::Piton {
        what: CONFIG_LEVEL_DENY.into(),
    };
    let own_piton = config.source.join(crate::config::PITON_DIR);
    let own_agents = config.checkout.join(crate::config::AGENTS_DIR);
    let mut areas: Vec<(PathBuf, Source)> = Vec::new();
    for level in &config.levels {
        // The source's `.piton/` is a protected root (below); the workspace's own `.agents/` is agent-editable.
        if *level == own_piton || *level == own_agents {
            continue;
        }
        areas.push((level.clone(), config_level.clone()));
    }
    let agent_dirs = [host_agent.clone(), agent.clone()];
    for (link, target) in crate::agent::link_entries(&config.levels, &agent_dirs, &config.source, &config.checkout) {
        let what = match agent_dirs.iter().any(|d| link.starts_with(d)) {
            true => AGENT_DIR_DENY,
            false => CONFIG_LEVEL_DENY,
        };
        areas.push((target, Source::Piton { what: what.into() }));
    }
    // Every folder on the way to each of them (and to the agent directories), fixed in place, as for what fnox loads: a
    // folder moved aside and replaced would let what's inside be changed under the same name. The areas' own denies
    // come after, so each names why it's closed.
    let roots: Vec<PathBuf> = agent_dirs
        .iter()
        .cloned()
        .chain(areas.iter().map(|(p, _)| p.clone()))
        .collect();
    for entry in crate::agent::fixed_entries(&roots) {
        rules.push(
            Effect::Deny,
            &[Access::Write],
            literal(&entry),
            Source::Piton {
                what: FIXED_FOLDER_DENY.into(),
            },
        );
    }
    for (path, source) in areas {
        rules.push(Effect::Deny, &[Access::Write], subpath(&path), source);
    }
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    rules.push(
        Effect::Deny,
        &[Access::Write],
        Target::Protected {
            source: canonical(&config.source),
            checkout: canonical(&config.checkout),
        },
        Source::Piton {
            what: "a protected file (.piton/ or .pi/): changed only with the user's approval".into(),
        },
    );
    // The `git` preset (the user's own choice, or yolo's) leaves this rule out.
    if !grants.presets.iter().any(|(p, _)| matches!(p, Preset::Git)) {
        rules.push(
            Effect::Deny,
            &[Access::Write],
            Target::GitControl {
                control: GitControl::of(&[&canonical(&config.checkout), &canonical(&config.source)]),
            },
            Source::Piton {
                what: GIT_CONTROL_DENY.into(),
            },
        );
    }

    // 6. The daemon.
    rules.push(
        Effect::Deny,
        &all,
        subpath(facts.state_dir),
        Source::Piton {
            what: "the daemon's state folder".into(),
        },
    );
    rules.push(
        Effect::Allow,
        &[Access::Read],
        subpath(facts.logs),
        Source::Piton {
            what: "Piton's logs".into(),
        },
    );
    rules.push(
        Effect::Allow,
        &[Access::Read],
        subpath(&crate::gate::ssh::folder(facts.state_dir)),
        Source::Piton {
            what: "the ssh wrapper that tunnels SSH through the network gate".into(),
        },
    );
    rules.push(
        Effect::Allow,
        &[Access::Connect],
        literal(facts.agent_socket),
        Source::Piton {
            what: "the agent socket (protected writes, commands the daemon runs)".into(),
        },
    );
    rules.push(
        Effect::Deny,
        &[Access::Connect],
        literal(facts.control_socket),
        Source::Piton {
            what: "the daemon's control socket (clients only)".into(),
        },
    );

    let keychains = facts.home.join("Library/Keychains");
    let keychain = grants
        .read
        .iter()
        .any(|(r, _)| keychains.starts_with(r) || r.starts_with(&keychains));
    let (rules, links) = rules.finish(&[&config.checkout, &config.source]);
    let policy = Policy {
        rules,
        keychain,
        services: grants.services.clone(),
        apps: grants.apps.clone(),
        network: grants.network.clone(),
        tag: String::new(),
    };
    Ok(Built {
        policy,
        links,
        denied,
        host_agent,
        agent,
    })
}

/// The `pi-claude-code` preset's name, as its rules name it.
const CLAUDE_CODE_PRESET: &str = "pi-claude-code";

/// What becomes of the folder `PI_CLAUDE_CODE_CONFIG_DIR` names: read like pi's agent directory, and never the
/// interactive Claude Code's folder or one that would open what the sandbox closes. `unopened` is the launch's policy
/// without it.
fn claude_code_folder(config: &LaunchConfig, facts: &Facts, unopened: &Built) -> ClaudeCodeFolder {
    let Some((value, origin)) = facts.launch_env.widening(Widening::ClaudeCodeConfig) else {
        return ClaudeCodeFolder::Unset;
    };
    let path = env_dir(value, facts.home, facts.workspace);
    let real = canonical_target(&path);
    let overlaps = |other: &Path| real.starts_with(other) || other.starts_with(&real);
    let interactive = [
        facts.base_env.get(CLAUDE_CONFIG_DIR).map(String::as_str),
        facts.launch_env.value(CLAUDE_CONFIG_DIR),
    ]
    .into_iter()
    .flatten()
    .filter(|v| !v.is_empty())
    .map(|v| env_dir(v, facts.home, facts.workspace))
    .chain([facts.home.join(".claude")]);
    let agent_dirs = [&unopened.host_agent, &unopened.agent];
    let reason = match closed_reason(&real, config, facts, &unopened.denied) {
        Some(reason) => Some(reason),
        None => agent_dirs
            .iter()
            .map(|d| canonical_target(d))
            .find(|d| overlaps(d))
            .map(|agent| format!("it overlaps pi's agent directory {}", agent.display())),
    };
    if let Some(reason) = reason {
        return ClaudeCodeFolder::Closed { path, reason };
    }
    for folder in interactive {
        if overlaps(&canonical_target(&folder)) {
            return ClaudeCodeFolder::Interactive { path };
        }
    }
    match widening_refusal(&path, origin, &unopened.policy, config, Some(&path)) {
        Some(refusal) => ClaudeCodeFolder::AgentWritable {
            path,
            set_by: refusal.set_by,
            checkout: refusal.checkout,
        },
        None => ClaudeCodeFolder::Granted(path),
    }
}

/// The `proper-lockfile` lock of a folder: `<folder>.lock` beside it.
fn lock_of(folder: &Path) -> PathBuf {
    let mut lock = folder.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// The interactive Claude Code's config folder, when it isn't `~/.claude`.
const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

/// The `[sandbox]` lists in effect: the profile's entries, then the user's, expanded to absolute paths, each with the
/// rule source that names where it came from.
struct Grants {
    write: Vec<(PathBuf, Source)>,
    write_file: Vec<(PathBuf, Source)>,
    read: Vec<(PathBuf, Source)>,
    sockets: Vec<(PathBuf, Source)>,
    bind: Vec<(PathBuf, Source)>,
    network: Network,
    services: Services,
    apps: Apps,
    presets: Vec<(Preset, Provenance)>,
    deny: Vec<(PathBuf, Source)>,
    /// The `pi-claude-code` preset, when it's on: its options and where they came from. Its config folder is decided
    /// from the launch's environment ([`claude_code_folder`]).
    claude_code: Option<(ClaudeCodeOptions, Provenance)>,
}

impl Grants {
    /// A profile's entry whose variable isn't set (no `$SSH_AUTH_SOCK`) is left out; the user's own refuses the launch.
    fn of(config: &LaunchConfig, facts: &Facts) -> Result<Self, SandboxError> {
        let settings = &config.sandbox;
        let profile = settings.profile();
        let entries = policy::profile_entries(profile);
        let from_profile = Provenance::Profile { profile };
        let from_file = |file: &Path| Provenance::File {
            path: file.to_path_buf(),
        };
        let source = |key: &str, from: Provenance| Source::Config {
            key: format!("sandbox.{key}"),
            from,
        };
        let expand = |grant: &str| expand_grant(grant, facts.home, facts.base_env);
        let from_profile_entry = |grant: &str| match expand(grant) {
            Ok(path) => Ok(Some(path)),
            Err(SandboxError::UnsetVariable { .. }) => Ok(None),
            Err(other) => Err(other),
        };
        let user = |key: &str, list: &[Listed<String>]| -> Result<Vec<(PathBuf, Source)>, SandboxError> {
            list.iter()
                .map(|l| Ok((expand(&l.value)?, source(key, from_file(&l.file)))))
                .collect()
        };

        // A preset both list keeps the profile as its source.
        let mut presets: Vec<(Preset, Provenance)> =
            entries.presets.into_iter().map(|p| (p, from_profile.clone())).collect();
        for listed in &settings.presets {
            if !presets.iter().any(|(p, _)| p.name() == listed.value.name()) {
                presets.push((listed.value, from_file(&listed.file)));
            }
        }

        // Each preset's entries come first in their lists, named after the preset.
        let mut write = Vec::new();
        let mut write_file = Vec::new();
        let mut read = Vec::new();
        let mut preset_connects = Vec::new();
        for (preset, from) in &presets {
            let granted = policy::preset_grants(preset);
            let source = Source::Preset {
                preset: preset.name().into(),
                from: from.clone(),
            };
            for path in granted.write {
                write.push((expand(path)?, source.clone()));
            }
            for path in granted.write_file {
                write_file.push((expand(path)?, source.clone()));
            }
            for path in granted.read {
                read.push((expand(path)?, source.clone()));
            }
            preset_connects.extend(granted.connect.into_iter().map(|c| (c, source.clone())));
        }
        write.extend(user("write", &settings.write)?);
        write_file.extend(user("write_file", &settings.write_file)?);

        for entry in entries.read {
            let paths = match entry {
                policy::ReadEntry::Path(grant) => from_profile_entry(grant)?.into_iter().collect(),
                policy::ReadEntry::SshPublicKeys => ssh_public_keys(&facts.home.join(".ssh")),
            };
            for path in paths {
                read.push((path, source("read", from_profile.clone())));
            }
        }
        read.extend(user("read", &settings.read)?);

        let mut bind = Vec::new();
        for grant in entries.bind {
            if let Some(path) = from_profile_entry(grant)? {
                bind.push((path, source("bind", from_profile.clone())));
            }
        }
        bind.extend(user("bind", &settings.bind)?);

        let mut sockets = Vec::new();
        let mut network = Vec::new();
        let connects = entries
            .connect
            .iter()
            .map(|c| (c.clone(), from_profile.clone()))
            .chain(settings.connect.iter().map(|l| (l.value.clone(), from_file(&l.file))))
            .map(|(c, from)| {
                let is_profile = matches!(from, Provenance::Profile { .. });
                (c, is_profile, source("connect", from))
            })
            .chain(preset_connects.into_iter().map(|(c, source)| (c, true, source)));
        for (connect, optional, source) in connects {
            match connect {
                Connect::Socket(grant) => {
                    let path = match optional {
                        true => from_profile_entry(&grant)?,
                        false => Some(expand(&grant)?),
                    };
                    if let Some(path) = path {
                        sockets.push((path, source));
                    }
                }
                Connect::Network(target) => network.push(NetworkGrant { target, source }),
            }
        }
        // `*` anywhere opens the network, named after the first entry that did; otherwise only what's listed, and the
        // gate the launch gives it later (`Gate::None` until then).
        let network = match network
            .iter()
            .find(|g| g.target == crate::config::NetworkTarget::Anywhere)
        {
            Some(open) => Network::Open {
                source: open.source.clone(),
            },
            None => Network::Filtered {
                grants: network,
                gate: Gate::None,
            },
        };

        let claude_code = presets.iter().find_map(|(preset, from)| match preset {
            Preset::PiClaudeCode(options) => Some((*options, from.clone())),
            Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift | Preset::Git | Preset::Apps => None,
        });

        // Mach services: the built-in list, then the presets', the profile's, and the user's; `*` anywhere opens every
        // service, named after the first entry that did.
        let mut grants: Vec<ServiceGrant> = policy::BASE_SERVICES
            .iter()
            .map(|(name, why)| ServiceGrant {
                name: ServiceName::Exact((*name).to_string()),
                source: Source::BuiltIn {
                    what: format!("a service every launch may look up: {why}"),
                },
            })
            .collect();
        let mut every = None;
        for (preset, from) in &presets {
            for name in policy::preset_grants(preset).services {
                grants.push(ServiceGrant {
                    name: ServiceName::Exact(name.to_string()),
                    source: Source::Preset {
                        preset: preset.name().into(),
                        from: from.clone(),
                    },
                });
            }
        }
        let entries_listed = entries
            .services
            .into_iter()
            .map(|e| (e, from_profile.clone()))
            .chain(settings.services.iter().map(|l| (l.value.clone(), from_file(&l.file))));
        for (entry, from) in entries_listed {
            let source = source("services", from);
            match entry {
                ServiceEntry::Every => {
                    every.get_or_insert(source);
                }
                ServiceEntry::Name(name) => grants.push(ServiceGrant { name, source }),
            }
        }
        let services = match every {
            Some(source) => Services::Every { source },
            None => Services::Listed { grants },
        };
        let apps = presets
            .iter()
            .find_map(|(preset, from)| match preset {
                Preset::Apps => Some(Apps::Open {
                    source: Source::Preset {
                        preset: preset.name().into(),
                        from: from.clone(),
                    },
                }),
                Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift | Preset::Git | Preset::PiClaudeCode(_) => {
                    None
                }
            })
            .unwrap_or(Apps::Closed);

        Ok(Self {
            write,
            write_file,
            read,
            sockets,
            bind,
            network,
            services,
            apps,
            presets,
            deny: user("deny", &settings.deny)?,
            claude_code,
        })
    }
}

/// Every regular file whose name ends in `.pub` in `folder` and its subfolders (a few levels, symlinks not followed):
/// SSH's public keys. Read when the launch is resolved; the sandbox can't write there, so it can't add one.
fn ssh_public_keys(folder: &Path) -> Vec<PathBuf> {
    const DEPTH: usize = 4;
    let mut keys = Vec::new();
    let mut pending = vec![(folder.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() && depth + 1 < DEPTH {
                pending.push((path, depth + 1));
            } else if kind.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pub")) {
                keys.push(path);
            }
        }
    }
    keys.sort();
    keys
}

/// Why a widening value isn't accepted: who set it, and the checkout it leads out of.
struct WideningRefusal {
    /// The file that set it, or what the tool said when it named none.
    set_by: String,
    checkout: PathBuf,
}

/// A value that widens the sandbox is accepted from the daemon's environment, or from a file the agent can't write.
/// One the agent could have written (or a tool that names no file) is accepted only while it leads into the
/// checkout. `path` is the value as the policy uses it; a file inside `opens` (the folder the value would open) counts
/// as writable.
fn widening_refusal(
    path: &Path,
    origin: &Origin,
    policy: &Policy,
    config: &LaunchConfig,
    opens: Option<&Path>,
) -> Option<WideningRefusal> {
    let set_by = match origin {
        Origin::Daemon => return None,
        Origin::Tool { tool, source } => match source {
            ToolSource::File(file) => {
                let inside = opens.is_some_and(|folder| {
                    let real = canonical_target(folder);
                    lexical(file).starts_with(folder) || canonical_target(file).starts_with(&real)
                });
                match inside || agent_writable(file, policy, config) {
                    true => file.display().to_string(),
                    false => return None,
                }
            }
            ToolSource::Unknown => format!("{tool}, which named no file"),
        },
    };
    let checkout = canonical_target(&config.checkout);
    match canonical_target(path).starts_with(&checkout) {
        true => None,
        false => Some(WideningRefusal { set_by, checkout }),
    }
}

/// Whether a sandboxed session of this project could have written `file`: this launch's policy lets it, as written or
/// at its real path, or it's in the checkout or source checkout, where a session in the whole checkout writes even
/// when this launch's workspace is a subfolder.
fn agent_writable(file: &Path, policy: &Policy, config: &LaunchConfig) -> bool {
    let written = lexical(file);
    let real = canonical_target(file);
    let roots = [canonical_target(&config.checkout), canonical_target(&config.source)];
    [&written, &real].into_iter().any(|path| {
        policy.decide(path, Access::Write).allowed
            || roots.iter().any(|root| path.starts_with(root))
            || [&config.checkout, &config.source]
                .iter()
                .any(|root| path.starts_with(root))
    })
}

/// The rules as written, in order; [`Rules::finish`] adds their real-path forms.
#[derive(Default)]
struct Rules {
    rules: Vec<Rule>,
}

impl Rules {
    fn push(&mut self, effect: Effect, access: &[Access], target: Target, source: Source) {
        self.rules.push(Rule {
            effect,
            access: access.to_vec(),
            target,
            source,
        });
    }

    /// Every rule, followed by the same rule for its real path when that differs: Seatbelt compares real paths (`/var`
    /// is `/private/var`), a connect may compare the path as given.
    ///
    /// What a sandboxed session of this project can replace: the `checkouts` (a session whose workspace is the whole
    /// checkout writes there, even when this launch's is a subfolder) and what this launch's write grants cover, as
    /// written or resolved. A resolved grant can make more replaceable (`~/l/b` with `~/l -> ~/x` covers `~/x/b`), which
    /// can stop another grant's resolution, so it's computed to a fixed point. The set only grows, which only follows
    /// fewer links, and it's bounded (each grant resolves to one of finitely many stopping points), so it ends.
    fn finish(self, checkouts: &[&Path]) -> (Vec<Rule>, Vec<UnfollowedLink>) {
        let mut replaceable: Vec<Target> = checkouts
            .iter()
            .map(|c| subpath(&canonical_target(c)))
            .chain(self.rules.iter().filter(|r| grants_write(r)).map(|r| r.target.clone()))
            .collect();
        loop {
            let resolved: Vec<RealForm> = self.rules.iter().map(|r| real_form(r, &replaceable)).collect();
            let mut grew = false;
            for (rule, real) in self.rules.iter().zip(&resolved) {
                if grants_write(rule)
                    && let Some(target) = &real.target
                    && !replaceable.contains(target)
                {
                    replaceable.push(target.clone());
                    grew = true;
                }
            }
            if grew {
                continue;
            }
            let mut rules = Vec::with_capacity(self.rules.len() * 2);
            let mut links: Vec<UnfollowedLink> = Vec::new();
            for (rule, real) in self.rules.into_iter().zip(resolved) {
                if let (Some(link), Some(grant)) = (real.unfollowed, rule_path(&rule.target)) {
                    let report = UnfollowedLink {
                        link,
                        grant: grant.to_path_buf(),
                        source: rule.source.clone(),
                    };
                    if !links.contains(&report) {
                        links.push(report);
                    }
                }
                let companion = real
                    .target
                    .filter(|t| *t != rule.target)
                    .map(|target| Rule { target, ..rule.clone() });
                rules.push(rule);
                rules.extend(companion);
            }
            return (rules, links);
        }
    }
}

fn grants_write(rule: &Rule) -> bool {
    match rule.effect {
        Effect::Allow => rule.access.contains(&Access::Write),
        Effect::Deny => false,
    }
}

fn rule_path(target: &Target) -> Option<&Path> {
    match target {
        Target::Subpath { path } | Target::Literal { path } | Target::Prefix { path } => Some(path),
        Target::Protected { .. } | Target::GitControl { .. } => None,
    }
}

/// A rule's real-path target, and the link its resolution stopped at because a sandboxed session could have made it.
struct RealForm {
    target: Option<Target>,
    unfollowed: Option<PathBuf>,
}

/// A deny follows every link: it closes what a link points to too. An allow follows only links outside `replaceable`,
/// and a write grant never its own last component, which it can always replace.
fn real_form(rule: &Rule, replaceable: &[Target]) -> RealForm {
    let resolve = |path: &Path, keep_last: Last| match rule.effect {
        Effect::Deny => Walk {
            path: canonical_target(path),
            unfollowed: None,
        },
        Effect::Allow => walk(path, keep_last, replaceable),
    };
    let last = match rule.access.contains(&Access::Write) {
        true => Last::Kept,
        false => Last::Followed,
    };
    let (target, walked) = match &rule.target {
        Target::Subpath { path } => {
            let walked = resolve(path, last);
            (subpath(&walked.path), walked.unfollowed)
        }
        Target::Literal { path } => {
            let walked = resolve(path, last);
            (literal(&walked.path), walked.unfollowed)
        }
        // A name prefix isn't an entry: its folder is resolved, the prefix kept.
        Target::Prefix { path } => match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let walked = resolve(parent, Last::Followed);
                (
                    Target::Prefix {
                        path: walked.path.join(name),
                    },
                    walked.unfollowed,
                )
            }
            (None, _) | (_, None) => (rule.target.clone(), None),
        },
        Target::Protected { .. } | Target::GitControl { .. } => {
            return RealForm {
                target: None,
                unfollowed: None,
            };
        }
    };
    RealForm {
        target: Some(target),
        unfollowed: walked,
    }
}

/// Whether a walk follows a link at the path's last component.
#[derive(Debug, Clone, Copy)]
enum Last {
    /// A write grant's own entry: always replaceable by the sandbox, never followed.
    Kept,
    /// Followed like any other component.
    Followed,
}

struct Walk {
    path: PathBuf,
    unfollowed: Option<PathBuf>,
}

/// `path` resolved from `/`, following each symlink unless `replaceable` covers it (a sandboxed session could have made
/// it), or it's the last component and `last` keeps it. Below a link that isn't followed, the agent decides what each
/// name resolves to, so the rest is kept as written. Past [`MAX_LINKS`] links, the rest is kept too.
fn walk(path: &Path, last: Last, replaceable: &[Target]) -> Walk {
    let mut pending: std::collections::VecDeque<OsString> = names(&lexical(path)).collect();
    // A relative path (an agent directory named relative) resolves against the working folder, as before.
    let mut current = match path.is_absolute() {
        true => PathBuf::from("/"),
        false => PathBuf::new(),
    };
    let mut followed = 0;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            current.pop();
            continue;
        }
        let candidate = current.join(&name);
        let is_link = candidate.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            current = candidate;
            continue;
        }
        let kept = match (last, pending.is_empty()) {
            (Last::Kept, true) => true,
            (Last::Kept, false) | (Last::Followed, _) => replaceable.iter().any(|t| t.covers(&candidate)),
        };
        let target = match kept || followed >= MAX_LINKS {
            true => None,
            false => std::fs::read_link(&candidate).ok(),
        };
        let Some(target) = target else {
            let mut rest = candidate.clone();
            rest.extend(pending);
            return Walk {
                path: rest,
                unfollowed: kept.then_some(candidate),
            };
        };
        followed += 1;
        if target.is_absolute() {
            current = PathBuf::from("/");
        }
        for name in names(&target).collect::<Vec<_>>().into_iter().rev() {
            pending.push_front(name);
        }
    }
    Walk {
        path: current,
        unfollowed: None,
    }
}

/// A path's names, with `..` kept as `..` (a link's target may climb).
fn names(path: &Path) -> impl Iterator<Item = OsString> + '_ {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name.to_os_string()),
        Component::ParentDir => Some(OsString::from("..")),
        Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
    })
}

fn subpath(path: &Path) -> Target {
    Target::Subpath {
        path: path.to_path_buf(),
    }
}

fn literal(path: &Path) -> Target {
    Target::Literal {
        path: path.to_path_buf(),
    }
}

/// pi's agent directory from `PI_CODING_AGENT_DIR` (with `~`; a relative one from the workspace, pi's working
/// folder), else `~/.pi/agent`. As named, with no symlink followed: its rules get their real paths like every other
/// rule.
pub fn agent_dir(value: Option<&str>, home: &Path, workspace: &Path) -> PathBuf {
    match value {
        Some(value) => env_dir(value, home, workspace),
        None => lexical(&home.join(".pi/agent")),
    }
}

/// A folder an environment variable names (with `~`; a relative one from the workspace, pi's working folder), as
/// named.
fn env_dir(value: &str, home: &Path, workspace: &Path) -> PathBuf {
    lexical(&workspace.join(expand_home(value, home)))
}

/// The launch's agent directory comes from the workspace's environment, so it can't be where it would open what the
/// sandbox otherwise closes.
fn check_agent_dir(
    agent: &Path,
    host_agent: &Path,
    config: &LaunchConfig,
    facts: &Facts,
    denied: &[PathBuf],
) -> Result<(), SandboxError> {
    let reason = match agent != host_agent && agent.starts_with(host_agent) {
        true => Some(format!(
            "it's inside the host's agent directory {}",
            host_agent.display()
        )),
        false => closed_reason(agent, config, facts, denied),
    };
    match reason {
        Some(reason) => Err(SandboxError::AgentDir {
            path: agent.to_path_buf(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Why a writable folder the launch's environment names (`dir`, canonical) would open what the sandbox otherwise
/// closes, or `None` when it wouldn't.
fn closed_reason(dir: &Path, config: &LaunchConfig, facts: &Facts, denied: &[PathBuf]) -> Option<String> {
    let overlaps = |other: &Path| dir.starts_with(other) || other.starts_with(dir);
    let home = canonical_target(facts.home);
    if home.starts_with(dir) {
        return Some("it's the home folder or contains it".into());
    }
    // A project's `.pi/` (settings, extensions) anywhere in the checkout: pi loads it in that folder.
    let pi = std::ffi::OsStr::new(".pi");
    if let Ok(inside) = dir.strip_prefix(canonical_target(&config.checkout))
        && inside
            .components()
            .any(|c| matches!(c, Component::Normal(name) if name.eq_ignore_ascii_case(pi)))
    {
        return Some("it's inside a .pi folder of the checkout, whose files pi loads".into());
    }
    if let Some(level) = config.levels.iter().map(|l| canonical_target(l)).find(|l| overlaps(l)) {
        return Some(format!("it overlaps the configuration level {}", level.display()));
    }
    if overlaps(&canonical_target(facts.state_dir)) {
        return Some("it overlaps the daemon's state folder".into());
    }
    if let Some(deny) = denied.iter().map(|d| canonical_target(d)).find(|d| overlaps(d)) {
        return Some(format!("it overlaps the denied path {}", deny.display()));
    }
    None
}

/// What's never configuration, for any launch in `config`'s workspace: the credential denies and the user's `[sandbox]
/// deny`, expanded (an entry naming an unset variable is left out: the sandbox refuses that launch anyway). No protected
/// area outside the checkout reaches them (`agent/protected.rs`).
pub fn never_configuration(config: &LaunchConfig, home: &Path, base_env: &Env) -> Vec<PathBuf> {
    policy::nono_denies()
        .into_iter()
        .map(|(path, _)| path)
        .chain(config.sandbox.deny.iter().map(|l| l.value.clone()))
        .filter_map(|path| expand_grant(&path, home, base_env).ok())
        .collect()
}

/// A grant as a literal absolute path: `~` and `$HOME` from the user database, other `$VAR`s (a whole leading
/// segment, `$NAME` or `${NAME}`) from the daemon's base environment.
pub(super) fn expand_grant(grant: &str, home: &Path, env: &Env) -> Result<PathBuf, SandboxError> {
    let path = expand_home(grant, home).to_string_lossy().into_owned();
    let expanded = match path.strip_prefix('$') {
        None => path,
        Some(rest) => {
            let (name, tail) = match rest.strip_prefix('{') {
                Some(braced) => braced.split_once('}').unwrap_or((braced, "")),
                None => rest.split_at(rest.find('/').unwrap_or(rest.len())),
            };
            let value = env
                .get(name)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| SandboxError::UnsetVariable {
                    grant: grant.to_string(),
                    var: name.to_string(),
                })?;
            format!("{value}{tail}")
        }
    };
    if !expanded.starts_with('/') || expanded.contains('$') {
        return Err(SandboxError::NotAbsolute {
            grant: grant.to_string(),
            expanded,
        });
    }
    Ok(PathBuf::from(expanded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Paths, Preset},
        project::{Relation, Workspace},
    };

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        repo: PathBuf,
        state: PathBuf,
    }

    fn fixture(global: &str, project: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let repo = home.join("p/repo");
        let state = home.join(".local/state/piton");
        for d in [
            home.join(".config/piton"),
            repo.join(".piton/.local"),
            state.join("logs"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".config/piton/config.toml"), global).unwrap();
        std::fs::write(repo.join(".piton/config.toml"), project).unwrap();
        Fixture {
            _dir: dir,
            home,
            repo,
            state,
        }
    }

    fn launch_config(f: &Fixture) -> LaunchConfig {
        let paths = Paths {
            home: f.home.clone(),
            global: f.home.join(".config/piton"),
        };
        let ws = Workspace {
            path: f.repo.clone(),
            checkout: f.repo.clone(),
            relation: Relation::Source,
        };
        crate::config::resolve_launch(&ws, &paths).unwrap()
    }

    fn policy_with(f: &Fixture, base: &Env, launch: &Env) -> Result<Policy, SandboxError> {
        resolve_in(f, &f.repo, base, launch).map(|r| r.policy)
    }

    /// `launch` as values of the daemon's environment: the tests of what an agent directory may be.
    fn resolve_in(f: &Fixture, workspace: &Path, base: &Env, launch: &Env) -> Result<Resolution, SandboxError> {
        resolve_env(f, workspace, base, &SandboxEnv::daemon(launch))
    }

    fn resolve_env(f: &Fixture, workspace: &Path, base: &Env, launch: &SandboxEnv) -> Result<Resolution, SandboxError> {
        resolve_full(f, &launch_config(f), workspace, base, launch, &[])
    }

    fn resolve_full(
        f: &Fixture,
        config: &LaunchConfig,
        workspace: &Path,
        base: &Env,
        launch: &SandboxEnv,
        fnox_files: &[PathBuf],
    ) -> Result<Resolution, SandboxError> {
        resolve_with_sockets(f, config, workspace, base, launch, fnox_files, None)
    }

    fn resolve_with_sockets(
        f: &Fixture,
        config: &LaunchConfig,
        workspace: &Path,
        base: &Env,
        launch: &SandboxEnv,
        fnox_files: &[PathBuf],
        claude_sockets: Option<&Path>,
    ) -> Result<Resolution, SandboxError> {
        let control = f.state.join("piton.sock");
        let agent = f.state.join("agent.sock");
        let logs = f.state.join("logs");
        resolve(
            config,
            &Facts {
                home: &f.home,
                base_env: base,
                launch_env: launch,
                workspace,
                state_dir: &f.state,
                control_socket: &control,
                agent_socket: &agent,
                logs: &logs,
                write_roots: &[],
                fnox_files,
                claude_sockets,
            },
        )
    }

    fn policy(f: &Fixture) -> Policy {
        policy_with(f, &Env::new(), &Env::new()).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn allowed(p: &Policy, path: &Path, access: Access) -> bool {
        p.decide(path, access).allowed
    }

    /// With no configuration: read everything but credentials, write the workspace and temp, nothing else.
    #[test]
    fn the_built_in_default() {
        let f = fixture("", "");
        let p = policy(&f);
        let h = &f.home;
        assert!(allowed(&p, Path::new("/etc/hosts"), Access::Read));
        assert!(allowed(&p, &h.join(".gitconfig"), Access::Read));
        assert!(allowed(&p, &f.repo.join("src/main.rs"), Access::Write));
        assert!(allowed(&p, &f.repo.join(".piton/.local/gigs/x"), Access::Write));
        assert!(!allowed(&p, &f.home.join("elsewhere"), Access::Write));
        for denied in [
            ".ssh/id_ed25519",
            ".aws/credentials",
            "Library/Keychains/login.keychain-db",
            ".zsh_history",
        ] {
            assert!(!allowed(&p, &h.join(denied), Access::Read), "{denied}");
        }
        // No user caches: host tools reuse them.
        for closed in [".cache/x", "Library/Caches/go-build/x", ".cargo/registry/x", ".zshrc"] {
            assert!(!allowed(&p, &h.join(closed), Access::Write), "{closed}");
        }
        // pi's runtime state, not its code.
        let agent = h.join(".pi/agent");
        assert!(allowed(&p, &agent.join("sessions/--w--/s.jsonl"), Access::Write));
        assert!(allowed(&p, &agent.join("auth.json"), Access::Write));
        assert!(allowed(&p, &agent.join("auth.json.lock/x"), Access::Write));
        for state in [
            "models-store.json",
            "models-store.json.lock",
            "mcp-auth.json",
            "mcp-auth.json.lock",
            "mcp-auth-refresh-0123abcd.lock",
            "mcp-auth-refresh-0123abcd.lock/x",
            "crashes.json",
            "mcp.log",
            "mcp.log.1",
            // pi locks `settings.json` and `trust.json` to read them; only the locks are writable.
            "settings.json.lock",
            "settings.json.lock/x",
            "trust.json.lock",
            "trust.json.lock/x",
        ] {
            assert!(allowed(&p, &agent.join(state), Access::Write), "{state}");
        }
        for code in [
            "settings.json",
            // Which folders' `.pi/` extensions and settings host sessions load.
            "trust.json",
            "mcp.json",
            "keybindings.json",
            "models.json",
            "bin/rg",
            "tmp/extensions/x",
            "git/x",
            // The name prefix covers only its own names.
            "mcp-auth-refreshx",
            "extensions/x.ts",
            "extensions/sessions/index.ts",
            "npm/x",
            "skills/s/SKILL.md",
        ] {
            assert!(!allowed(&p, &agent.join(code), Access::Write), "{code}");
        }
        // Configuration and protected files.
        assert!(!allowed(&p, &h.join(".config/piton/config.toml"), Access::Write));
        assert!(!allowed(&p, &f.repo.join(".piton/config.toml"), Access::Write));
        assert!(!allowed(&p, &f.repo.join("sub/.pi/settings.json"), Access::Write));
        assert!(allowed(&p, &f.repo.join(".agents/rules/r.md"), Access::Write));
        // The daemon.
        assert!(!allowed(&p, &f.state.join("piton.sock"), Access::Connect));
        assert!(!allowed(&p, &f.state.join("piton.db"), Access::Read));
        assert!(allowed(&p, &f.state.join("agent.sock"), Access::Connect));
        assert!(allowed(&p, &f.state.join("logs/daemon/current.log"), Access::Read));
        assert!(!allowed(&p, &f.state.join("logs/daemon/current.log"), Access::Write));
        // Unix sockets: closed but DNS (standard, the default, binds anywhere and has no SSH agent here).
        assert!(!allowed(&p, Path::new("/private/tmp/ssh-agent.sock"), Access::Connect));
        assert!(!allowed(
            &p,
            Path::new("/private/var/folders/x/T/nvim.u/0/nvim.1.0"),
            Access::Connect
        ));
        assert!(allowed(&p, Path::new("/var/run/mDNSResponder"), Access::Connect));
        assert!(allowed(
            &p,
            Path::new("/private/var/run/mDNSResponder"),
            Access::Connect
        ));
        assert!(allowed(&p, &f.repo.join("test.sock"), Access::Bind));
        assert!(
            !allowed(&p, &f.state.join("x.sock"), Access::Bind),
            "the daemon's state"
        );
        assert!(!allowed(&p, &h.join(".ssh/agent.sock"), Access::Bind), "a credential");
        assert!(!p.keychain);
    }

    #[test]
    fn presets_writes_and_denies() {
        let f = fixture(
            "",
            "[sandbox]\npresets = [\"rust\"]\nwrite = [\"~/out\"]\nwrite_file = [\"~/f\"]\ndeny = [\"~/p/repo/secret\"]\n",
        );
        assert_eq!(launch_config(&f).sandbox.presets[0].value, Preset::Rust);
        let p = policy(&f);
        let h = &f.home;
        assert!(allowed(&p, &h.join(".cargo/registry/index/x"), Access::Write));
        assert!(allowed(&p, &h.join(".cargo/.package-cache"), Access::Write));
        assert!(!allowed(&p, &h.join(".cargo/bin/cargo"), Access::Write));
        assert!(allowed(&p, &h.join("out/x"), Access::Write));
        assert!(allowed(&p, &h.join("f"), Access::Write));
        assert!(!allowed(&p, &h.join("f2"), Access::Write));
        // A user deny beats the workspace grant.
        assert!(!allowed(&p, &f.repo.join("secret/k"), Access::Read));
        assert!(!allowed(&p, &f.repo.join("secret/k"), Access::Write));
        assert_eq!(
            p.decide(&f.repo.join("secret/k"), Access::Read).source,
            Source::Config {
                key: "sandbox.deny".into(),
                from: Provenance::File {
                    path: f.repo.join(".piton/config.toml")
                },
            }
        );
        assert_eq!(
            p.decide(&f.home.join(".cargo/registry/x"), Access::Write).source,
            Source::Preset {
                preset: "rust".into(),
                from: Provenance::File {
                    path: f.repo.join(".piton/config.toml")
                },
            }
        );
    }

    /// `write` alone doesn't reopen a credential; `read` (global only) does, and makes it writable with `write`.
    #[test]
    fn read_lifts_a_built_in_deny() {
        let f = fixture(
            "[sandbox]\nprofile = \"strict\"\nread = [\"~/.ssh/config\", \"~/Library/Keychains\"]\nwrite = [\"~/Library/Keychains\", \"~/.aws\"]\n",
            "",
        );
        let p = policy(&f);
        let h = &f.home;
        assert!(allowed(&p, &h.join(".ssh/config"), Access::Read));
        assert!(!allowed(&p, &h.join(".ssh/config"), Access::Write));
        assert!(!allowed(&p, &h.join(".ssh/id_ed25519"), Access::Read));
        assert!(allowed(
            &p,
            &h.join("Library/Keychains/login.keychain-db"),
            Access::Write
        ));
        assert!(
            !allowed(&p, &h.join(".aws/credentials"), Access::Read),
            "write doesn't lift a deny"
        );
        assert!(!allowed(&p, &h.join(".aws/credentials"), Access::Write));
        assert!(p.keychain);
    }

    /// `strict` is the base: no profile entry. `standard` (the default) reads SSH's host files, connects to the SSH agent
    /// and the network; `yolo` adds every socket and the `git` preset. Each entry names its profile.
    #[test]
    fn profiles_only_add() {
        let agent = env(&[("SSH_AUTH_SOCK", "/private/tmp/agent.sock")]);
        let resolve = |global: &str| {
            let f = fixture(global, "");
            let r = resolve_in(&f, &f.repo, &agent, &Env::new()).unwrap();
            (f, r)
        };
        let profile_rules = |r: &Resolution| {
            let network = match &r.policy.network {
                Network::Open { source } => usize::from(from_profile(source)),
                Network::Filtered { grants, .. } => grants.iter().filter(|g| from_profile(&g.source)).count(),
            };
            r.policy.rules.iter().filter(|rule| from_profile(&rule.source)).count() + network
        };

        let ssh_keys = |f: &Fixture| {
            std::fs::create_dir_all(f.home.join(".ssh/keys")).unwrap();
            for name in ["keys/id_ed25519", "keys/id_ed25519.pub", "id_rsa.pub"] {
                std::fs::write(f.home.join(".ssh").join(name), "k").unwrap();
            }
        };
        let (f, _) = resolve("[sandbox]\nprofile = \"strict\"\n");
        ssh_keys(&f);
        let strict = resolve_in(&f, &f.repo, &agent, &Env::new()).unwrap();
        assert_eq!(profile_rules(&strict), 0, "strict adds nothing");
        assert_eq!(strict.profile.name, Profile::Strict);
        assert_eq!(strict.profile.set_in, Some(f.home.join(".config/piton/config.toml")));
        let p = &strict.policy;
        assert!(!allowed(p, &f.home.join(".ssh/config"), Access::Read));
        assert!(!allowed(p, &f.home.join(".ssh/id_rsa.pub"), Access::Read));
        assert!(!allowed(p, Path::new("/private/tmp/agent.sock"), Access::Connect));
        assert!(
            !allowed(p, &f.repo.join("test.sock"), Access::Bind),
            "strict binds nowhere"
        );
        assert_eq!(
            p.network,
            Network::Filtered {
                grants: Vec::new(),
                gate: Gate::None
            },
            "strict's network is filtered, with nothing listed"
        );

        let (f, _) = resolve("");
        ssh_keys(&f);
        let standard = resolve_in(&f, &f.repo, &agent, &Env::new()).unwrap();
        assert_eq!(standard.profile.name, Profile::Standard);
        assert_eq!(standard.profile.set_in, None, "the default");
        let p = &standard.policy;
        assert!(allowed(p, &f.home.join(".ssh/config"), Access::Read));
        assert!(allowed(p, &f.home.join(".ssh/known_hosts"), Access::Read));
        assert!(
            !allowed(p, &f.home.join(".ssh/id_ed25519"), Access::Read),
            "keys stay closed"
        );
        // Public keys, nested ones too, as exact paths; their private halves stay closed.
        assert!(allowed(p, &f.home.join(".ssh/id_rsa.pub"), Access::Read));
        assert!(allowed(p, &f.home.join(".ssh/keys/id_ed25519.pub"), Access::Read));
        assert!(!allowed(p, &f.home.join(".ssh/keys/id_ed25519"), Access::Read));
        assert!(
            !allowed(p, &f.home.join(".ssh/keys/other.pub"), Access::Read),
            "found at launch"
        );
        assert!(allowed(p, Path::new("/private/tmp/agent.sock"), Access::Connect));
        assert!(allowed(p, &f.repo.join("test.sock"), Access::Bind));
        assert!(allowed(p, Path::new("/private/tmp/x.sock"), Access::Bind));
        assert!(!allowed(p, Path::new("/private/tmp/x.sock"), Access::Connect));
        assert_eq!(
            p.decide(&f.home.join(".ssh/config"), Access::Read).source,
            Source::Config {
                key: "sandbox.read".into(),
                from: Provenance::Profile {
                    profile: Profile::Standard
                },
            }
        );
        assert!(
            p.rules
                .iter()
                .any(|r| r.target == subpath(Path::new("/private/tmp/agent.sock")) && r.access == [Access::Connect])
        );
        assert_eq!(
            p.network,
            Network::Open {
                source: Source::Config {
                    key: "sandbox.connect".into(),
                    from: Provenance::Profile {
                        profile: Profile::Standard
                    },
                }
            },
            "standard's `*` opens the network"
        );
        assert!(!allowed(p, &f.repo.join(".git/config"), Access::Write));

        let (f, yolo) = resolve("[sandbox]\nprofile = \"yolo\"\n");
        let p = &yolo.policy;
        assert!(
            allowed(p, &f.home.join(".ssh/config"), Access::Read),
            "yolo holds standard"
        );
        assert!(allowed(p, &f.repo.join(".git/config"), Access::Write), "the git preset");
        assert!(
            p.rules
                .iter()
                .any(|r| r.target == subpath(Path::new("/")) && r.access == [Access::Connect])
        );
        assert!(
            allowed(p, Path::new("/private/tmp/x.sock"), Access::Connect),
            "every socket"
        );
        assert!(allowed(p, &f.repo.join("test.sock"), Access::Bind));
    }

    /// `standard`'s `$SSH_AUTH_SOCK` pointing at an agent inside a credential folder (1Password's) opens that socket for
    /// connecting: nothing else there.
    #[test]
    fn an_ssh_agent_in_a_credential_folder_connects() {
        let f = fixture("", "");
        let folder = f.home.join("Library/Group Containers/2BUA8C4S2C.com.1password/t");
        let agent = folder.join("agent.sock");
        let r = resolve_in(
            &f,
            &f.repo,
            &env(&[("SSH_AUTH_SOCK", agent.to_str().unwrap())]),
            &Env::new(),
        )
        .unwrap();
        assert!(allowed(&r.policy, &agent, Access::Connect));
        assert!(!allowed(&r.policy, &agent, Access::Read));
        assert!(!allowed(&r.policy, &folder.join("other.sock"), Access::Connect));
        assert!(!allowed(&r.policy, &folder.join("data.sqlite"), Access::Read));
    }

    fn from_profile(source: &Source) -> bool {
        match source {
            Source::Config { from, .. } | Source::Preset { from, .. } => matches!(from, Provenance::Profile { .. }),
            Source::Workspace
            | Source::BuiltIn { .. }
            | Source::NonoGroup { .. }
            | Source::Piton { .. }
            | Source::FnoxConfig
            | Source::AgentDir => false,
        }
    }

    /// A broad `connect` entry (yolo's `/`) never reopens a credential folder, and no entry reopens the user's `deny`
    /// or the daemon's sockets. An entry at or inside a credential folder opens just that path, for connecting.
    #[test]
    fn connect_never_reopens_a_deny() {
        let f = fixture(
            "[sandbox]\nprofile = \"yolo\"\nconnect = [\"~/secret\"]\ndeny = [\"~/secret\"]\n",
            "",
        );
        let p = policy(&f);
        assert!(allowed(&p, Path::new("/private/tmp/any.sock"), Access::Connect));
        for closed in [
            f.home.join(".gnupg/S.gpg-agent"),
            f.home.join(".docker/run/docker.sock"),
            f.home.join("secret/s.sock"),
            f.state.join("piton.sock"),
        ] {
            assert!(!allowed(&p, &closed, Access::Connect), "{}", closed.display());
        }
        assert!(allowed(&p, &f.state.join("agent.sock"), Access::Connect));

        // Named: just that socket opens, for connecting; the folder's files and other sockets stay closed.
        let g = fixture(
            "[sandbox]\nprofile = \"yolo\"\nconnect = [\"~/.docker/run/docker.sock\", \"~/.gnupg/S.gpg-agent\", \
             \"~/secret/s.sock\"]\ndeny = [\"~/secret\"]\n",
            "",
        );
        let p = policy(&g);
        let docker = g.home.join(".docker/run/docker.sock");
        assert!(allowed(&p, &docker, Access::Connect));
        assert!(allowed(&p, &g.home.join(".gnupg/S.gpg-agent"), Access::Connect));
        assert!(!allowed(&p, &docker, Access::Read));
        assert!(!allowed(&p, &docker, Access::Bind));
        assert!(!allowed(&p, &g.home.join(".docker/config.json"), Access::Read));
        assert!(!allowed(&p, &g.home.join(".docker/run/other.sock"), Access::Connect));
        assert!(!allowed(&p, &g.home.join(".gnupg/S.gpg-agent.ssh"), Access::Connect));
        assert!(
            !allowed(&p, &g.home.join("secret/s.sock"), Access::Connect),
            "the user's deny"
        );
        assert_eq!(
            p.decide(&docker, Access::Connect).source,
            Source::Config {
                key: "sandbox.connect".into(),
                from: Provenance::File {
                    path: g.home.join(".config/piton/config.toml")
                },
            }
        );

        // `read` reads only: a socket it covers stays closed.
        let h = fixture("[sandbox]\nread = [\"~/.gnupg/S.gpg-agent\"]\n", "");
        let p = policy(&h);
        assert!(allowed(&p, &h.home.join(".gnupg/S.gpg-agent"), Access::Read));
        assert!(!allowed(&p, &h.home.join(".gnupg/S.gpg-agent"), Access::Connect));
    }

    /// A socket a `connect` entry names, and every folder on its way, can't be written, renamed, or replaced: otherwise
    /// a session could move the real `$SSH_AUTH_SOCK` (under temp, which it writes) aside and link another socket in its
    /// place. A sibling in its folder stays writable.
    #[test]
    fn a_named_socket_stays_in_place() {
        let tmp = "/private/tmp/piton-test-launchd";
        let f = fixture(&format!("[sandbox]\nwrite = [\"{tmp}\"]\n"), "");
        let launchd = Path::new(tmp).join("com.apple.launchd.x");
        let agent = launchd.join("Listeners");
        let r = resolve_in(
            &f,
            &f.repo,
            &env(&[("SSH_AUTH_SOCK", agent.to_str().unwrap())]),
            &Env::new(),
        )
        .unwrap();
        let p = &r.policy;
        assert!(allowed(p, &launchd.join("other"), Access::Write));
        assert!(allowed(p, &agent, Access::Connect));
        for fixed in [&agent, &launchd, &Path::new(tmp).to_path_buf()] {
            assert!(!allowed(p, fixed, Access::Write), "{}", fixed.display());
        }
    }

    /// A profile's entry whose variable isn't set is left out; the user's own refuses the launch.
    #[test]
    fn unset_variables_skip_profile_entries_only() {
        let f = fixture("", "");
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(!r.policy.rules.iter().any(|rule| matches!(
            &rule.source,
            Source::Config { key, .. } if key == "sandbox.connect"
        ) && rule.access == [Access::Connect]));
        let g = fixture("[sandbox]\nconnect = [\"$NOPE/s.sock\"]\n", "");
        assert!(matches!(
            resolve_in(&g, &g.repo, &Env::new(), &Env::new()),
            Err(SandboxError::UnsetVariable { .. })
        ));
    }

    #[test]
    fn grants_expand_from_the_daemons_environment() {
        let f = fixture("[sandbox]\nwrite = [\"$CACHE/x\"]\n", "");
        let p = policy_with(
            &f,
            &env(&[("CACHE", "/private/tmp/c")]),
            &env(&[("CACHE", "/elsewhere")]),
        )
        .unwrap();
        assert!(
            p.rules
                .iter()
                .any(|r| r.target == subpath(Path::new("/private/tmp/c/x")))
        );
        assert!(matches!(
            policy_with(&f, &Env::new(), &Env::new()),
            Err(SandboxError::UnsetVariable { .. })
        ));
        let home = Path::new("/Users/a");
        assert_eq!(
            expand_grant("${HOME}x", home, &env(&[("HOME", "/h")])).unwrap(),
            Path::new("/hx")
        );
        assert!(matches!(
            expand_grant("$REL", home, &env(&[("REL", "rel")])),
            Err(SandboxError::NotAbsolute { .. })
        ));
    }

    /// The workspace sets `PI_CODING_AGENT_DIR` (its `mise.toml`): a per-folder one works, one that would open what
    /// the sandbox closes refuses the launch.
    #[test]
    fn the_agent_directory_comes_from_the_launch_but_cant_open_anything() {
        let f = fixture("", "");
        let h = f.home.to_string_lossy().into_owned();
        let launch = |dir: &str| policy_with(&f, &Env::new(), &env(&[(Widening::AgentDir.var(), dir)]));

        let p = launch("~/p/agents/one").unwrap();
        assert!(allowed(
            &p,
            &f.home.join("p/agents/one/sessions/s.jsonl"),
            Access::Write
        ));
        assert!(!allowed(
            &p,
            &f.home.join("p/agents/one/extensions/x.ts"),
            Access::Write
        ));
        // Inside the workspace: its code is closed even there.
        let p = launch("~/p/repo/agent").unwrap();
        assert!(!allowed(&p, &f.repo.join("agent/extensions/x.ts"), Access::Write));
        assert!(allowed(&p, &f.repo.join("agent/auth.json"), Access::Write));
        // The host's agent directory stays closed, runtime state included.
        assert!(!allowed(&p, &f.home.join(".pi/agent/extensions/x.ts"), Access::Write));
        assert!(!allowed(&p, &f.home.join(".pi/agent/sessions/s.jsonl"), Access::Write));

        for refused in [
            h.clone(),
            "/".into(),
            "~/.pi/agent/extensions".into(),
            "~/p/repo/.pi".into(),
            "~/p/repo/sub/.PI/agent".into(),
            "~/.config/piton/agent".into(),
            "~/.config".into(),
            "~/.local/state/piton/x".into(),
            "~/.ssh/agent".into(),
            "~/p/repo/.piton".into(),
        ] {
            assert!(
                matches!(launch(&refused), Err(SandboxError::AgentDir { .. })),
                "{refused}"
            );
        }
        // The host's own, from the daemon's environment.
        let base = env(&[(Widening::AgentDir.var(), &format!("{h}/.config/pi/agent"))]);
        let p = policy_with(&f, &base, &base).unwrap();
        assert!(allowed(&p, &f.home.join(".config/pi/agent/auth.json"), Access::Write));
        assert!(!allowed(
            &p,
            &f.home.join(".config/pi/agent/settings.json"),
            Access::Write
        ));
    }

    #[test]
    fn configuration_links_are_closed() {
        let f = fixture("", "");
        let dotfiles = f.home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("config.toml"), "").unwrap();
        let global = f.home.join(".config/piton/config.toml");
        std::fs::remove_file(&global).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("config.toml"), &global).unwrap();
        let p = policy_with(&f, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&p, &dotfiles.join("config.toml"), Access::Write));
    }

    /// A dotfiles repository is the workspace, and pi's agent directory and the global tier link into it: the link
    /// targets are closed, and every folder on the way to them (and to the levels and agent directory) can't be renamed
    /// or replaced, so a folder moved aside and rebuilt can't change them under the same name. What's beside them stays
    /// writable; pi's runtime state too.
    #[test]
    fn agent_directory_links_are_closed_with_their_folders() {
        let f = fixture("", "");
        let agent = f.home.join(".pi/agent");
        let pi = f.repo.join("home/pi");
        mkdir(&pi.join("extensions"));
        std::fs::write(pi.join("settings.json"), "{}").unwrap();
        link(&pi.join("settings.json"), &agent.join("settings.json"));
        link(&pi.join("extensions"), &agent.join("extensions"));
        let piton = f.repo.join("home/piton");
        mkdir(&piton);
        std::fs::write(piton.join("config.toml"), "").unwrap();
        let global = f.home.join(".config/piton/config.toml");
        std::fs::remove_file(&global).unwrap();
        link(&piton.join("config.toml"), &global);
        let p = policy(&f);
        let denied = |path: &Path| p.decide(path, Access::Write);
        for closed in [
            pi.join("settings.json"),
            pi.join("extensions/evil.ts"),
            piton.join("config.toml"),
        ] {
            assert!(!denied(&closed).allowed, "{}", closed.display());
        }
        for folder in [
            pi.clone(),
            f.repo.join("home"),
            piton.clone(),
            agent.clone(),
            f.home.join(".pi"),
        ] {
            let decision = denied(&folder);
            assert!(!decision.allowed, "{} can't be renamed", folder.display());
        }
        assert_eq!(
            denied(&pi).source,
            Source::Piton {
                what: FIXED_FOLDER_DENY.into()
            }
        );
        assert!(denied(&pi.join("keybindings.json")).allowed, "a sibling");
        assert!(denied(&f.repo.join("home/README.md")).allowed, "beside the folders");
        assert!(denied(&f.repo.join("src/x")).allowed, "the rest of the workspace");
        assert!(denied(&agent.join("sessions/s.jsonl")).allowed, "pi's runtime state");
        assert!(allowed(&p, &pi.join("settings.json"), Access::Read));
    }

    fn link(target: &Path, at: &Path) {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    fn reported(r: &Resolution) -> Vec<PathBuf> {
        r.links.iter().map(|l| l.link.clone()).collect()
    }

    /// A runtime entry of pi's agent directory the agent replaced with a link doesn't move the grant to the link's
    /// target: the next launch can't write the shell config through `crashes.json`.
    #[test]
    fn a_runtime_entry_replaced_with_a_link_grants_nothing() {
        let f = fixture("", "");
        let agent = f.home.join(".pi/agent");
        std::fs::write(f.home.join(".zshrc"), "").unwrap();
        std::fs::write(f.home.join("victim"), "").unwrap();
        link(&f.home.join(".zshrc"), &agent.join("crashes.json"));
        link(&f.home.join("victim"), &agent.join("mcp.log"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&r.policy, &f.home.join(".zshrc"), Access::Write));
        assert!(!allowed(&r.policy, &f.home.join("victim"), Access::Write));
        assert!(allowed(&r.policy, &agent.join("auth.json"), Access::Write));
        let links = reported(&r);
        assert!(links.contains(&agent.join("crashes.json")), "{links:?}");
        assert!(links.contains(&agent.join("mcp.log")), "{links:?}");
    }

    #[test]
    fn an_allow_replaced_with_a_link_grants_nothing() {
        let f = fixture("[sandbox]\nwrite = [\"~/d/cache\"]\n", "");
        mkdir(&f.home.join("victim"));
        link(&f.home.join("victim"), &f.home.join("d/cache"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&r.policy, &f.home.join("victim/x"), Access::Write));
        assert_eq!(reported(&r), [f.home.join("d/cache")]);
        assert_eq!(
            r.links[0].source,
            Source::Config {
                key: "sandbox.write".into(),
                from: Provenance::File {
                    path: f.home.join(".config/piton/config.toml")
                },
            }
        );
    }

    /// A link the sandbox can't replace is followed: the user's own (a folder moved to another disk), like `/var`.
    #[test]
    fn a_link_above_every_grant_is_followed() {
        let f = fixture("[sandbox]\nwrite = [\"~/ln/cache\"]\n", "");
        mkdir(&f.home.join("real/cache"));
        link(&f.home.join("real"), &f.home.join("ln"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(allowed(&r.policy, &f.home.join("real/cache/x"), Access::Write));
        assert!(!allowed(&r.policy, &f.home.join("real/other"), Access::Write));
        assert!(r.links.is_empty(), "{:?}", r.links);
    }

    /// An agent directory inside the workspace, below a folder the agent swapped for a link out of it.
    #[test]
    fn an_agent_directory_below_a_planted_link_stays_in_the_checkout() {
        let f = fixture("", "");
        let outside = f.home.join("outside");
        mkdir(&outside.join("agent"));
        link(&outside, &f.repo.join("tools"));
        let launch = env(&[(Widening::AgentDir.var(), "~/p/repo/tools/agent")]);
        let r = resolve_in(&f, &f.repo, &Env::new(), &launch).unwrap();
        assert!(!allowed(&r.policy, &outside.join("agent/crashes.json"), Access::Write));
        assert!(!allowed(
            &r.policy,
            &outside.join("agent/sessions/s.jsonl"),
            Access::Write
        ));
        assert!(!allowed(
            &r.policy,
            &outside.join("agent/mcp-auth-refresh-x.lock"),
            Access::Write
        ));
        assert!(reported(&r).contains(&f.repo.join("tools")));
    }

    /// A session in the whole checkout could have planted the link; a later one in a subfolder must not follow it.
    #[test]
    fn links_anywhere_in_the_checkout_are_never_followed() {
        let f = fixture("", "");
        let launch_agents = f.home.join("Library/LaunchAgents");
        mkdir(&launch_agents);
        mkdir(&f.repo.join("packages/web"));
        link(&launch_agents, &f.repo.join(".pi-agent"));
        let launch = env(&[(Widening::AgentDir.var(), "~/p/repo/.pi-agent")]);
        let r = resolve_in(&f, &f.repo.join("packages/web"), &Env::new(), &launch).unwrap();
        for entry in ["mcp-auth-refresh-x.plist", "crashes.json", "sessions/x.plist"] {
            assert!(
                !allowed(&r.policy, &launch_agents.join(entry), Access::Write),
                "{entry}"
            );
        }
    }

    /// A resolved grant can make an entry replaceable that another grant's path goes through.
    #[test]
    fn what_resolved_grants_cover_is_replaceable_too() {
        let f = fixture("[sandbox]\nwrite = [\"~/l/b\", \"~/x/b/c/d\"]\n", "");
        mkdir(&f.home.join("x/b"));
        mkdir(&f.home.join("victim/d"));
        link(&f.home.join("x"), &f.home.join("l"));
        link(&f.home.join("victim"), &f.home.join("x/b/c"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(allowed(&r.policy, &f.home.join("x/b/y"), Access::Write));
        assert!(!allowed(&r.policy, &f.home.join("victim/d/z"), Access::Write));
        assert_eq!(reported(&r), [f.home.join("x/b/c")]);
    }

    /// Denies follow every link: a deny named by a link closes its target.
    #[test]
    fn a_deny_closes_what_its_link_points_to() {
        let f = fixture("[sandbox]\ndeny = [\"~/p/repo/secret\"]\n", "");
        let secret = f.home.join("secret");
        mkdir(&secret);
        link(&secret, &f.repo.join("secret"));
        let p = policy(&f);
        assert!(!allowed(&p, &secret.join("k"), Access::Read));
    }

    /// Git's hooks and configuration are closed in the checkout, the rest of `.git` and a new worktree's `.git` file
    /// aren't, and `piton sandbox why` names the rule.
    #[test]
    fn gits_control_files_are_closed() {
        let f = fixture("", "");
        let p = policy(&f);
        for denied in [
            ".git",
            ".git/hooks/post-merge",
            ".git/config",
            ".GIT/Config",
            "vendor/lib/.git/hooks/x",
        ] {
            assert!(!allowed(&p, &f.repo.join(denied), Access::Write), "{denied}");
        }
        // `piton sandbox why` names the opt-in.
        let source = p.decide(&f.repo.join(".git/config"), Access::Write).source;
        assert!(
            matches!(&source, Source::Piton { what } if what.contains("git preset")),
            "{source:?}"
        );
        for open in [
            ".git/index",
            ".git/refs/heads/x",
            ".git/worktrees/w/commondir",
            ".piton/.local/pull-requests/1/rebase/.git",
        ] {
            assert!(allowed(&p, &f.repo.join(open), Access::Write), "{open}");
        }
        assert!(allowed(&p, &f.repo.join(".git/config"), Access::Read));

        // The `git` preset in the global configuration lifts them.
        let g = fixture("[sandbox]\npresets = [\"git\"]\n", "");
        let p = policy(&g);
        for open in [".git/config", ".git/hooks/post-merge", ".git/commondir"] {
            assert!(allowed(&p, &g.repo.join(open), Access::Write), "{open}");
        }
    }

    /// Every rule is valid Seatbelt the library accepts.
    #[test]
    fn the_rules_are_accepted_by_the_library() {
        let agent = env(&[("SSH_AUTH_SOCK", "/private/tmp/agent.sock")]);
        for profile in Profile::ALL {
            let f = fixture(
                &format!(
                    "[sandbox]\nprofile = \"{}\"\nread = [\"~/Library/Keychains\"]\npresets = [\"mise\", \"rust\", \"node\", \"swift\"]\nconnect = [\"~/s.sock\", \"localhost:5432\"]\n",
                    profile.name()
                ),
                "",
            );
            let p = policy_with(&f, &agent, &Env::new()).unwrap();
            let caps = super::super::apply::capabilities(&p).unwrap();
            let rules = caps.platform_rules();
            assert!(rules.iter().any(|r| r.contains("piton.sock")), "{}", profile.name());
            let every_socket = rules.iter().any(|r| r == r#"(allow network-outbound (subpath "/"))"#);
            let ssh_agent = rules.iter().any(|r| r.contains("/private/tmp/agent.sock"));
            let services_listed = rules.iter().any(|r| r == "(deny mach-lookup)");
            let apps = rules.iter().any(|r| r == "(allow lsopen)");
            match profile {
                Profile::Strict => assert!(!every_socket && !ssh_agent && services_listed && !apps),
                Profile::Standard => assert!(!every_socket && ssh_agent && !services_listed && !apps),
                Profile::Yolo => assert!(every_socket && ssh_agent && !services_listed && apps),
            }
        }
    }

    /// Mach services: `strict` allows only the built-in list, `standard` and `yolo` every service but the keychain's;
    /// the user's own `services` and the `apps` preset add to `strict`, each named where it came from.
    #[test]
    fn services_and_apps_follow_the_profile() {
        let f = fixture("[sandbox]\nprofile = \"strict\"\n", "");
        let p = policy(&f);
        assert!(p.decide_service("com.apple.logd").allowed, "the built-in list");
        assert!(p.decide_service("com.apple.trustd.agent").allowed, "TLS");
        assert!(!p.decide_service("com.apple.pasteboard.1").allowed);
        assert!(!p.decide_service("Hammerspoon").allowed);
        assert!(!p.decide_service("com.apple.SecurityServer").allowed, "the keychain");
        assert_eq!(p.apps, Apps::Closed);

        let standard = policy(&fixture("", ""));
        assert!(standard.decide_service("Hammerspoon").allowed);
        assert!(
            matches!(&standard.decide_service("x").source, Source::Config { key, from: Provenance::Profile { profile: Profile::Standard } } if key == "sandbox.services")
        );
        assert!(
            !standard.decide_service("com.apple.secd").allowed,
            "the keychain stays closed"
        );
        assert_eq!(standard.apps, Apps::Closed);

        let yolo = policy(&fixture("[sandbox]\nprofile = \"yolo\"\n", ""));
        assert!(matches!(&yolo.apps, Apps::Open { source: Source::Preset { preset, .. } } if preset == "apps"));
        assert!(yolo.decide_service("com.apple.coreservices.appleevents").allowed);

        let listed = fixture(
            "[sandbox]\nprofile = \"strict\"\nservices = [\"com.example.*\"]\npresets = [\"apps\"]\n",
            "",
        );
        let p = policy(&listed);
        let from = p.decide_service("com.example.agent");
        assert!(from.allowed);
        assert!(matches!(
            &from.source,
            Source::Config {
                from: Provenance::File { .. },
                ..
            }
        ));
        assert!(
            p.decide_service("com.apple.coreservices.appleevents").allowed,
            "the apps preset"
        );
        assert!(matches!(p.apps, Apps::Open { .. }));
        assert!(!p.decide_service("com.apple.pasteboard.1").allowed);
        super::super::apply::capabilities(&p).expect("nono accepts it");

        // A user `*` opens every service in `strict` too, named after that file.
        let every = policy(&fixture("[sandbox]\nprofile = \"strict\"\nservices = [\"*\"]\n", ""));
        assert!(matches!(
            every.services,
            Services::Every {
                source: Source::Config {
                    from: Provenance::File { .. },
                    ..
                }
            }
        ));
    }

    fn from_file(file: &Path) -> Origin {
        Origin::Tool {
            tool: crate::launch::env::EnvTool::Mise,
            source: ToolSource::File(file.to_path_buf()),
        }
    }

    fn agent_from(value: &str, origin: Origin) -> SandboxEnv {
        SandboxEnv::for_tests(&[(Widening::AgentDir.var(), value, origin)])
    }

    /// A workspace's `mise.toml` the agent can write may set the agent directory only inside the checkout; a file it
    /// can't write (a client folder's, above the checkout) may set it anywhere the existing checks allow.
    #[test]
    fn the_agent_directory_follows_the_file_that_set_it() {
        let f = fixture("", "");
        let own = f.repo.join("mise.toml");
        let sub = f.repo.join("packages/web");
        mkdir(&sub);
        let refused = |workspace: &Path, env: &SandboxEnv| {
            matches!(
                resolve_env(&f, workspace, &Env::new(), env),
                Err(SandboxError::Widening { .. })
            )
        };

        // The workspace's own `mise.toml`: inside the checkout, also from a subfolder session.
        let inside = agent_from("~/p/repo/.pi-agent", from_file(&own));
        let r = resolve_env(&f, &sub, &Env::new(), &inside).unwrap();
        assert!(allowed(
            &r.policy,
            &f.repo.join(".pi-agent/crashes.json"),
            Access::Write
        ));
        assert!(refused(&f.repo, &agent_from("~/Library/LaunchAgents", from_file(&own))));
        assert!(refused(&sub, &agent_from("~/Library/LaunchAgents", from_file(&own))));
        // A subfolder's file, outside this launch's workspace but in the checkout.
        assert!(refused(
            &sub,
            &agent_from("~/elsewhere", from_file(&f.repo.join("other/mise.toml")))
        ));
        // No file named: written by the agent.
        let unknown = Origin::Tool {
            tool: crate::launch::env::EnvTool::Mise,
            source: ToolSource::Unknown,
        };
        assert!(refused(&f.repo, &agent_from("~/elsewhere", unknown)));

        // A client folder's file above the checkout: the agent can't write it.
        let client = f.home.join("p/mise.toml");
        let shared = agent_from("~/p/shared-agent", from_file(&client));
        let r = resolve_env(&f, &f.repo, &Env::new(), &shared).unwrap();
        assert!(allowed(
            &r.policy,
            &f.home.join("p/shared-agent/crashes.json"),
            Access::Write
        ));
        // The daemon's environment: trusted.
        let daemon = SandboxEnv::daemon(&env(&[(Widening::AgentDir.var(), "~/elsewhere")]));
        assert!(resolve_env(&f, &f.repo, &Env::new(), &daemon).is_ok());

        // A file the launch's grants cover is writable wherever it is.
        let g = fixture("[sandbox]\nwrite = [\"~/shared\"]\n", "");
        let granted = agent_from("~/elsewhere", from_file(&g.home.join("shared/mise.toml")));
        assert!(matches!(
            resolve_env(&g, &g.repo, &Env::new(), &granted),
            Err(SandboxError::Widening { .. })
        ));
    }

    /// mise names a file as written: a link in the checkout to a file the agent can't write is still the agent's.
    #[test]
    fn a_file_reached_through_a_checkout_link_is_the_agents() {
        let f = fixture("", "");
        let outside = f.home.join("dotfiles/mise.toml");
        mkdir(outside.parent().unwrap());
        std::fs::write(&outside, "").unwrap();
        link(&outside, &f.repo.join("mise.toml"));
        let env = agent_from("~/elsewhere", from_file(&f.repo.join("mise.toml")));
        assert!(matches!(
            resolve_env(&f, &f.repo, &Env::new(), &env),
            Err(SandboxError::Widening { .. })
        ));
    }

    /// pi resolves a relative agent directory from its working folder: the workspace.
    #[test]
    fn a_relative_agent_directory_is_the_workspaces() {
        let f = fixture("", "");
        let sub = f.repo.join("web");
        mkdir(&sub);
        let env = agent_from("./agent", from_file(&sub.join("mise.toml")));
        let r = resolve_env(&f, &sub, &Env::new(), &env).unwrap();
        assert!(allowed(&r.policy, &sub.join("agent/auth.json"), Access::Write));
    }

    /// The process question's entries, checked against the real resolution: one that lifts is offered; one a later deny
    /// beats (Git's control files, the agent directory) isn't, and the deny that decides is named. An entry is resolved
    /// as listed in the file its scope saves to.
    #[test]
    fn entries_are_offered_only_when_the_resolution_lets_them_through() {
        use super::super::grant::{Bounds, Credentials, DeniedBy, Lift, NoLift, Refused, Scope, lift_for};
        let f = fixture("", "");
        let config = launch_config(&f);
        let launch = policy(&f);
        let file = f.home.join(".config/piton/config.toml");
        let widened = |lift: &Lift, _: Scope| {
            let config = super::super::grant::widened_config(&config, &lift.lists(), &file);
            resolve_full(&f, &config, &f.repo, &Env::new(), &SandboxEnv::daemon(&Env::new()), &[])
                .map(|r| r.policy)
                .map_err(|e| e.to_string())
        };
        let lift = |op: &str, target: &Path| {
            let target = target.to_string_lossy();
            let refused = Refused {
                operation: op,
                target: &target,
                pid: 4242,
            };
            let projects = [f.repo.as_path()];
            let bounds = Bounds {
                projects: &projects,
                home: &f.home,
                shared: &[],
                credentials: Credentials::Liftable,
            };
            lift_for(&launch, refused, &bounds, widened)
        };
        let cache = f.home.join("Library/Caches/tool");
        assert_eq!(
            lift("file-write-create", &cache).map(|o| o.project),
            Ok(Lift::Write(cache.clone()))
        );
        assert_eq!(
            lift("file-write-unlink", &f.repo.join(".git/config")),
            Err(NoLift::Decided(DeniedBy::GitControl))
        );
        assert_eq!(
            lift("file-read-data", &f.home.join(".ssh/keys/id_ed25519")),
            Err(NoLift::Decided(DeniedBy::Credential(Access::Read)))
        );
        assert_eq!(
            lift("file-write-create", &f.home.join(".config/piton/x.toml")),
            Err(NoLift::Decided(DeniedBy::Floor))
        );
    }

    /// What fnox loads is closed in the policy, not refused: each file at its real path, and each folder on the way, so
    /// none can be renamed or replaced to change the file under another name. What's beside them stays writable.
    #[test]
    fn fnox_files_are_closed_with_their_folders() {
        let f = fixture("", "");
        // A dotfiles repo is the workspace; fnox's global config links into it.
        let real = f.repo.join("fnox/config.toml");
        mkdir(real.parent().unwrap());
        std::fs::write(&real, "").unwrap();
        let link = f.home.join(".config/fnox/config.toml");
        mkdir(link.parent().unwrap());
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let r = resolve_full(
            &f,
            &launch_config(&f),
            &f.repo,
            &Env::new(),
            &SandboxEnv::daemon(&Env::new()),
            std::slice::from_ref(&link),
        )
        .unwrap();
        let p = &r.policy;
        assert!(!allowed(p, &real, Access::Write), "the real file");
        assert!(
            !allowed(p, &f.repo.join("fnox"), Access::Write),
            "its folder can't be renamed"
        );
        assert!(!allowed(p, &link, Access::Write), "the link itself");
        assert!(allowed(p, &f.repo.join("fnox/other.toml"), Access::Write), "a sibling");
        assert!(
            allowed(p, &f.repo.join("src/x"), Access::Write),
            "the rest of the workspace"
        );
        assert_eq!(p.decide(&real, Access::Write).source, Source::FnoxConfig);
        assert!(allowed(p, &real, Access::Read));
    }

    /// A launch behind fnox: the built-in policy passes its checks; a sandbox that can read the keychain doesn't
    /// (`launch/fnox.rs`).
    #[test]
    fn fnox_checks_against_resolved_policies() {
        use crate::{
            launch::fnox::{FnoxError, KeychainOpener, Proxy},
            tools::Program,
        };
        let proxy = |files: Vec<PathBuf>| Proxy {
            program: Program {
                program: "fnox".into(),
                args: vec![],
            },
            files,
            env: Env::new(),
            variables: vec![],
        };
        let f = fixture("", "");
        let global = f.home.join(".config/fnox/config.toml");
        let built_in = policy(&f);
        assert!(proxy(vec![global.clone()]).check_sandbox(&built_in, &f.home).is_ok());
        // The workspace's own fnox file is protected.
        assert!(
            proxy(vec![f.repo.join("fnox.toml")])
                .check_sandbox(&built_in, &f.home)
                .is_ok()
        );
        let exposed = fixture("[sandbox]\nread = [\"~/Library/Keychains\"]\n", "");
        assert!(matches!(
            proxy(vec![]).check_sandbox(&policy(&exposed), &exposed.home),
            Err(FnoxError::Keychain {
                opened_by: KeychainOpener::Read {
                    from: Some(Provenance::File { .. })
                }
            })
        ));
        // Exposing the whole Library opens the keychains too.
        let library = fixture("[sandbox]\nread = [\"~/Library\"]\n", "");
        assert!(matches!(
            proxy(vec![]).check_sandbox(&policy(&library), &library.home),
            Err(FnoxError::Keychain { .. })
        ));
        // So does the pi-claude-code preset's keychain login, named with the file that set it; the token login
        // doesn't.
        let token = fixture("[sandbox]\npresets = [\"pi-claude-code\"]\n", "");
        assert!(proxy(vec![]).check_sandbox(&policy(&token), &token.home).is_ok());
        let keychain = fixture(
            "[sandbox]\npresets = [{ name = \"pi-claude-code\", login = \"keychain\" }]\n",
            "",
        );
        let global_file = keychain.home.join(".config/piton/config.toml");
        assert!(matches!(
            proxy(vec![]).check_sandbox(&policy(&keychain), &keychain.home),
            Err(FnoxError::Keychain {
                opened_by: KeychainOpener::Preset { preset, from: Provenance::File { path } }
            }) if preset == "pi-claude-code" && path == global_file
        ));
    }

    const CLAUDE_PRESET: &str = "[sandbox]\npresets = [\"pi-claude-code\"]\n";

    fn claude_from(value: &str, origin: Origin) -> SandboxEnv {
        SandboxEnv::for_tests(&[(Widening::ClaudeCodeConfig.var(), value, origin)])
    }

    fn claude_folder(r: &Resolution) -> &ClaudeCodeFolder {
        &r.claude_code.as_ref().expect("the preset is on").folder
    }

    /// The `pi-claude-code` preset opens the folder `PI_CLAUDE_CODE_CONFIG_DIR` names and its lock beside it, the log
    /// cache, and the extension's relay on any local port; never the keychain with the token login.
    #[test]
    fn the_claude_code_preset_opens_its_own_folder() {
        let f = fixture(CLAUDE_PRESET, "");
        let folder = f.home.join(".config/pi/claude");
        let env = claude_from("~/.config/pi/claude", Origin::Daemon);
        let r = resolve_env(&f, &f.repo, &Env::new(), &env).unwrap();
        assert_eq!(claude_folder(&r), &ClaudeCodeFolder::Granted(folder.clone()));
        assert_eq!(r.claude_code.as_ref().unwrap().login, ClaudeLogin::Token);
        let p = &r.policy;
        for path in [
            folder.join(".claude.json"),
            folder.join("sessions/x"),
            f.home.join(".config/pi/claude.lock"),
            f.home.join(".config/pi/claude.lock/x"),
            f.home.join("Library/Caches/claude-cli-nodejs/-w/mcp-logs-x/log.jsonl"),
        ] {
            assert!(allowed(p, &path, Access::Write), "{}", path.display());
        }
        assert!(!allowed(p, &f.home.join(".config/pi/other"), Access::Write));
        assert!(!allowed(p, &f.home.join(".claude/settings.json"), Access::Write));
        let keychain = f.home.join("Library/Keychains/login.keychain-db");
        assert!(!allowed(p, &keychain, Access::Read));
        assert!(!p.keychain);
        // The relay's `localhost:*`: under strict, the preset's entry decides a local port; another host stays closed.
        let strict = fixture(&format!("{CLAUDE_PRESET}profile = \"strict\"\n"), "");
        let r = resolve_env(&strict, &strict.repo, &Env::new(), &env).unwrap();
        let relay = r
            .policy
            .decide_network(&crate::launch::sandbox::Destination::parse("127.0.0.1:52011").unwrap());
        assert!(relay.allowed);
        assert!(matches!(&relay.source, Source::Preset { preset, .. } if preset == "pi-claude-code"));
        assert!(
            !r.policy
                .decide_network(&crate::launch::sandbox::Destination::parse("example.com:443").unwrap())
                .allowed
        );
        assert!(matches!(
            p.decide(&folder.join("x"), Access::Write).source,
            Source::Preset { preset, .. } if preset == "pi-claude-code"
        ));

        // Without the preset: nothing of it.
        let none = fixture("", "");
        let r = resolve_env(&none, &none.repo, &Env::new(), &env).unwrap();
        assert!(r.claude_code.is_none());
        assert!(!allowed(
            &r.policy,
            &none.home.join(".config/pi/claude/x"),
            Access::Write
        ));
    }

    /// The preset's socket folder: the session's own when the user sets neither variable; the user's when it's a private
    /// folder of its own, from a file the agent can't write; otherwise refused with a warning. Opened for writing and
    /// creating sockets, never connecting; `claude`'s own probes there are silenced.
    #[test]
    fn the_claude_code_preset_gives_claude_a_socket_folder_of_its_own() {
        let f = fixture(CLAUDE_PRESET, "");
        let config = launch_config(&f);
        let own = f.home.join("t/piton-cc-x");
        let run = |env: &SandboxEnv, own: Option<&Path>| {
            resolve_with_sockets(&f, &config, &f.repo, &Env::new(), env, &[], own).unwrap()
        };
        let sockets = |r: &Resolution| r.claude_code.as_ref().unwrap().sockets.clone();
        let r = run(&SandboxEnv::daemon(&Env::new()), Some(&own));
        assert_eq!(sockets(&r), Some(ClaudeCodeSockets::Own(own.clone())));
        let socket = own.join("cc-socks/4242.sock");
        assert!(allowed(&r.policy, &socket, Access::Bind));
        assert!(allowed(&r.policy, &own.join("claude-501/x"), Access::Write));
        assert!(!allowed(&r.policy, &socket, Access::Connect), "never connecting");
        let probe = socket.to_string_lossy();
        assert!(silenced(&r.silence, "claude", "network-outbound", &probe).is_some());
        assert!(silenced(&r.silence, "node", "network-outbound", &probe).is_none());
        assert!(silenced(&r.silence, "claude", "network-outbound", "/private/tmp/cc-socks/1.sock").is_none());
        // No folder to give (`piton sandbox show`): nothing opened.
        assert_eq!(sockets(&run(&SandboxEnv::daemon(&Env::new()), None)), None);

        // The user's own, from the daemon's environment: used, and wins over Piton's.
        let users = f.home.join("run/claude");
        let from = |var: Widening, value: &str, origin: Origin| SandboxEnv::for_tests(&[(var.var(), value, origin)]);
        let r = run(
            &from(Widening::ClaudeCodeTmp, "~/run/claude", Origin::Daemon),
            Some(&own),
        );
        assert_eq!(
            sockets(&r),
            Some(ClaudeCodeSockets::Users {
                var: "CLAUDE_CODE_TMPDIR",
                path: users.clone()
            })
        );
        assert!(allowed(&r.policy, &users.join("cc-socks/1.sock"), Access::Bind));
        assert!(!allowed(&r.policy, &own.join("x"), Access::Write));
        // `XDG_RUNTIME_DIR` first, as claude reads it.
        let both = SandboxEnv::for_tests(&[
            ("XDG_RUNTIME_DIR", "~/run/xdg", Origin::Daemon),
            ("CLAUDE_CODE_TMPDIR", "~/run/claude", Origin::Daemon),
        ]);
        assert!(matches!(
            sockets(&run(&both, Some(&own))),
            Some(ClaudeCodeSockets::Users {
                var: "XDG_RUNTIME_DIR",
                ..
            })
        ));

        // Refused: a shared folder, the home folder, a path too long for the socket, a file the agent can write.
        let refused = |env: &SandboxEnv| match sockets(&run(env, Some(&own))) {
            Some(ClaudeCodeSockets::Refused { reason, .. }) => reason,
            other => panic!("not refused: {other:?}"),
        };
        assert!(refused(&from(Widening::ClaudeCodeTmp, "/private/tmp", Origin::Daemon)).contains("other processes"));
        assert!(refused(&from(Widening::ClaudeCodeTmp, "~", Origin::Daemon)).contains("home folder"));
        let deep = format!("~/run/{}", "d".repeat(100));
        assert!(refused(&from(Widening::ClaudeCodeTmp, &deep, Origin::Daemon)).contains("103 bytes"));
        let workspace_file = Origin::Tool {
            tool: crate::launch::env::EnvTool::Mise,
            source: crate::launch::env::ToolSource::File(f.repo.join("mise.toml")),
        };
        assert!(refused(&from(Widening::ClaudeCodeTmp, "~/run/claude", workspace_file)).contains("leads out"));
    }

    /// The token login expects `claude`'s keychain lookups, its own and through `security`: they're silenced, and no
    /// one else's is. The keychain login has none to silence. The user's entries match by target and process.
    #[test]
    fn silence_entries_match_target_and_process() {
        let user = "[sandbox]\npresets = [\"pi-claude-code\"]\nsilence = [\
                    { process = \"git\", target = \"~/p/**/.git/config\" }, { target = \"~/Library/Caches/tool\" }]\n";
        let f = fixture(user, "");
        let r = resolve_env(&f, &f.repo, &Env::new(), &SandboxEnv::daemon(&Env::new())).unwrap();
        let hit = |process: &str, operation: &str, target: &str| silenced(&r.silence, process, operation, target);
        for process in ["claude", "security"] {
            let entry = hit(process, "mach-lookup", "com.apple.SecurityServer").expect(process);
            assert_eq!(entry.source, SilenceSource::Preset("pi-claude-code"));
        }
        assert!(hit("node", "mach-lookup", "com.apple.SecurityServer").is_none());
        assert!(hit("claude", "mach-lookup", "com.apple.diagnosticd").is_none());
        let home = f.home.to_string_lossy();
        for worker in ["p/Piton/.git/config", "p/Piton--1-x/.git/config", "p/a/b/.git/config"] {
            assert!(
                hit("git", "file-write-create", &format!("{home}/{worker}")).is_some(),
                "{worker}"
            );
        }
        assert!(hit("git", "file-write-create", &format!("{home}/p/Piton/.git/config.bak")).is_none());
        assert!(hit("node", "file-write-create", &format!("{home}/p/Piton/.git/config")).is_none());
        assert!(hit("any", "file-write-create", &format!("{home}/Library/Caches/tool/x")).is_some());
        // A path entry never matches a service lookup of the same text.
        assert!(hit("any", "mach-lookup", &format!("{home}/Library/Caches/tool")).is_none());

        let keychain = fixture(
            "[sandbox]\npresets = [{ name = \"pi-claude-code\", login = \"keychain\" }]\n",
            "",
        );
        let r = resolve_env(&keychain, &keychain.repo, &Env::new(), &SandboxEnv::daemon(&Env::new())).unwrap();
        assert!(silenced(&r.silence, "security", "mach-lookup", "com.apple.SecurityServer").is_none());
    }

    /// `login = "keychain"` reads the keychain (which reopens its services); its files stay unwritable.
    #[test]
    fn the_keychain_login_reads_the_keychain() {
        let f = fixture(
            "[sandbox]\npresets = [{ name = \"pi-claude-code\", login = \"keychain\" }]\n",
            "",
        );
        let r = resolve_env(&f, &f.repo, &Env::new(), &SandboxEnv::daemon(&Env::new())).unwrap();
        assert_eq!(r.claude_code.as_ref().unwrap().login, ClaudeLogin::Keychain);
        let keychain = f.home.join("Library/Keychains/login.keychain-db");
        assert!(allowed(&r.policy, &keychain, Access::Read));
        assert!(!allowed(&r.policy, &keychain, Access::Write));
        assert!(r.policy.keychain);
    }

    /// Every folder the preset won't open, and why: the session starts anyway, `claude`'s writes there blocked.
    #[test]
    fn the_claude_code_folder_is_never_what_the_sandbox_closes() {
        let f = fixture(CLAUDE_PRESET, "");
        let resolve_with = |base: &Env, env: &SandboxEnv| resolve_env(&f, &f.repo, base, env).unwrap();
        let folder_of = |base: &Env, env: &SandboxEnv| claude_folder(&resolve_with(base, env)).clone();
        let none = Env::new();

        assert_eq!(folder_of(&none, &SandboxEnv::daemon(&none)), ClaudeCodeFolder::Unset);
        // The interactive Claude Code's folder: `~/.claude`, or `CLAUDE_CONFIG_DIR` from the daemon or pi's env.
        assert!(matches!(
            folder_of(&none, &claude_from("~/.claude", Origin::Daemon)),
            ClaudeCodeFolder::Interactive { .. }
        ));
        let interactive = env(&[("CLAUDE_CONFIG_DIR", "~/.config/claude")]);
        let same = SandboxEnv::for_tests(&[
            (Widening::ClaudeCodeConfig.var(), "~/.config/claude", Origin::Daemon),
            ("CLAUDE_CONFIG_DIR", "~/.config/claude", Origin::Daemon),
        ]);
        assert!(matches!(
            folder_of(&interactive, &same),
            ClaudeCodeFolder::Interactive { .. }
        ));
        assert!(matches!(folder_of(&none, &same), ClaudeCodeFolder::Interactive { .. }));
        assert!(matches!(
            folder_of(&interactive, &claude_from("~/.config/claude/inner", Origin::Daemon)),
            ClaudeCodeFolder::Interactive { .. }
        ));
        // What the sandbox closes: home, a configuration level, the daemon's state, a credential, pi's agent dir.
        for (value, why) in [
            ("~", "home folder"),
            ("~/.config/piton", "configuration level"),
            ("~/.local/state/piton", "state folder"),
            ("~/.ssh/claude", "denied path"),
            ("~/.pi/agent/claude", "agent directory"),
        ] {
            let r = resolve_with(&none, &claude_from(value, Origin::Daemon));
            match claude_folder(&r) {
                ClaudeCodeFolder::Closed { reason, .. } => assert!(reason.contains(why), "{value}: {reason}"),
                other @ (ClaudeCodeFolder::Granted(_)
                | ClaudeCodeFolder::Unset
                | ClaudeCodeFolder::Interactive { .. }
                | ClaudeCodeFolder::AgentWritable { .. }) => panic!("{value}: {other:?}"),
            }
            assert!(!allowed(
                &r.policy,
                &env_dir(value, &f.home, &f.repo).join("x"),
                Access::Write
            ));
        }
    }

    /// Like pi's agent directory, the folder follows the file that set it: one a sandboxed session can write (the
    /// workspace's `mise.toml`, a file inside the folder itself) may name it only inside the checkout.
    #[test]
    fn the_claude_code_folder_follows_the_file_that_set_it() {
        let f = fixture(CLAUDE_PRESET, "");
        let folder_of = |env: &SandboxEnv| claude_folder(&resolve_env(&f, &f.repo, &Env::new(), env).unwrap()).clone();
        let own = from_file(&f.repo.join("mise.toml"));
        assert!(matches!(
            folder_of(&claude_from("~/.config/pi/claude", own.clone())),
            ClaudeCodeFolder::AgentWritable { .. }
        ));
        assert_eq!(
            folder_of(&claude_from(".claude-pi", own)),
            ClaudeCodeFolder::Granted(f.repo.join(".claude-pi"))
        );
        // A file above the checkout no session can write.
        let above = from_file(&f.home.join("p/mise.toml"));
        assert_eq!(
            folder_of(&claude_from("~/.config/pi/claude", above)),
            ClaudeCodeFolder::Granted(f.home.join(".config/pi/claude"))
        );
        // A file inside the folder it names would be writable once the folder opens.
        let inside = from_file(&f.home.join(".config/pi/claude/mise.toml"));
        assert!(matches!(
            folder_of(&claude_from("~/.config/pi/claude", inside)),
            ClaudeCodeFolder::AgentWritable { .. }
        ));
        let unknown = Origin::Tool {
            tool: crate::launch::env::EnvTool::Mise,
            source: ToolSource::Unknown,
        };
        assert!(matches!(
            folder_of(&claude_from("~/.config/pi/claude", unknown)),
            ClaudeCodeFolder::AgentWritable { .. }
        ));
    }

    /// A folder that's a symlink is granted as named, not followed: the agent could have swapped it.
    #[test]
    fn a_linked_claude_code_folder_isnt_followed() {
        let f = fixture(CLAUDE_PRESET, "");
        let elsewhere = f.home.join("elsewhere");
        mkdir(&elsewhere);
        link(&elsewhere, &f.home.join(".config/pi/claude"));
        let r = resolve_env(
            &f,
            &f.repo,
            &Env::new(),
            &claude_from("~/.config/pi/claude", Origin::Daemon),
        )
        .unwrap();
        assert!(!allowed(&r.policy, &elsewhere.join("x"), Access::Write));
        assert_eq!(reported(&r), [f.home.join(".config/pi/claude")]);
    }
}
