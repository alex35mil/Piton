//! One launch's [`Policy`]: Piton's base policy, the profile's and presets' entries, the `[sandbox]` chain's maps, and
//! the launch's facts (workspace, agent directory, the daemon's state folder), as an ordered list of literal rules.
//!
//! Order, the last matching rule deciding:
//! 1. the base write roots (the workspace, the source's `.piton/.local`, temp) and DNS;
//! 2. the entry layer, merged with 1 by path depth, least specific first, so the most specific entry decides (a deny
//!    after an allow at the same depth): nono's credential denies, the profile's and presets' entries, the files'
//!    filesystem, socket, and binding entries, and the `pi-claude-code` preset's folders. A committed file's opening
//!    entry inside a credential or the user's own deny is left out;
//! 3. the host agent directory is read-only; the launch's agent directory's runtime state is writable;
//! 4. the files fnox loads; each socket an entry allows, and the folders on its way, kept in place;
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
    rules::{Access, Apps, Effect, Gate, Listen, Network, NetworkGrant, Policy, Rule, ServiceGrant, Services, Target},
};
use crate::{
    agent::{GitControl, canonical_target, lexical},
    config::{
        ClaudeCodeOptions, ClaudeLogin, FileAccess, LOCAL_DIR, LaunchConfig, Listed, NetworkTarget,
        Origin as ConfigOrigin, Preset, Profile, ServiceKey, ServiceName, expand_home,
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
const GIT_CONTROL_DENY: &str = "Git's hooks and configuration: Git on the host runs what they name; \
                                `allow_git_control_files = true` opens them (global config.toml or a config.local.toml)";

/// Why a socket a `connect` entry names, or a folder on its way, can't be written.
const SOCKET_PATH_DENY: &str = "a socket an entry allows, or a folder on its way: kept in place so no other socket can \
                                be moved there";

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
    /// Entries a committed file wrote that would reopen what a credential or the user's own deny closes, left out.
    pub ignored: Vec<String>,
}

/// One `silence` entry, expanded, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silenced {
    pub target: Silent,
    /// Only blocks of processes with this name.
    pub process: Option<String>,
    pub source: SilenceSource,
}

/// What a silence entry matches, expanded. A path is a preset's own (its socket folder); the user's paths are decided
/// by `"deny"` entries instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Silent {
    /// A path and everything below it, absolute and canonical; `*` and `**` components are wildcards.
    Path(PathBuf),
    /// A Mach service, by name.
    Service(String),
    /// A TCP listen, on any port.
    TcpListen,
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
    /// A `silence` target as it matches.
    pub fn named(target: &crate::config::SilenceTarget) -> Self {
        match target {
            crate::config::SilenceTarget::TcpListen => Silent::TcpListen,
            crate::config::SilenceTarget::Service(name) => Silent::Service(name.clone()),
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
            Silent::TcpListen => super::rules::is_listen(operation, target),
        }
    }

    /// The entry as `[sandbox] silence` writes it (a preset's path as its inline table, for display only).
    pub fn describe(&self) -> String {
        let target = match &self.target {
            Silent::Path(path) => path.display().to_string(),
            Silent::Service(name) => name.clone(),
            Silent::TcpListen => crate::config::SilenceTarget::TcpListen.into(),
        };
        let target = toml::Value::String(target).to_string();
        match &self.process {
            Some(process) => format!(
                "{{ process = {}, target = {target} }}",
                toml::Value::String(process.clone())
            ),
            None => format!("{{ target = {target} }}"),
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
    let mut silence: Vec<Silenced> = config
        .sandbox
        .silence
        .iter()
        .map(|l| Silenced {
            target: Silent::named(&l.value.target),
            process: l.value.process.clone(),
            source: SilenceSource::File(l.file.clone()),
        })
        .collect();
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
        ignored: grants.ignored,
    })
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
    // DNS, before the entries: an entry can close it.
    rules.push(
        Effect::Allow,
        &[Access::Connect],
        literal(Path::new(DNS_SOCKET)),
        Source::BuiltIn { what: "DNS".into() },
    );

    // 2. The entry layer: credential denies, the profile's and presets' entries, the user's, and the preset's folders,
    // emitted least specific first so the most specific decides (Seatbelt's last match), a deny after an allow at the
    // same depth. The base roots above take part by depth too: an entry inside the workspace applies, a broader one
    // above it doesn't close it.
    // At the same depth, in order: the base roots, built-in denies, base-layer entries, the user's (an allow, then a
    // deny): the user's own entry for a path decides over a built-in one for the same path.
    const ROOT: u8 = 0;
    const BUILT_IN: u8 = 1;
    const BASE: u8 = 2;
    const USER_ALLOW: u8 = 3;
    const USER_DENY: u8 = 4;
    let all = [Access::Read, Access::Write, Access::Connect, Access::Bind];
    let mut layer: Vec<(PathBuf, u8, Effect, Vec<Access>, Source)> = Vec::new();
    let mut denied = Vec::new();
    for (path, source) in policy::nono_denies() {
        let path = expand(&path)?;
        layer.push((path.clone(), BUILT_IN, Effect::Deny, all.to_vec(), source));
        denied.push(path);
    }
    let rank = |source: &Source, effect: Effect| match (source, effect) {
        (
            Source::Config {
                from: Provenance::File { .. },
                ..
            },
            Effect::Allow,
        ) => USER_ALLOW,
        (
            Source::Config {
                from: Provenance::File { .. },
                ..
            },
            Effect::Deny,
        ) => USER_DENY,
        (
            Source::Config { .. }
            | Source::Preset { .. }
            | Source::Workspace
            | Source::BuiltIn { .. }
            | Source::NonoGroup { .. }
            | Source::Piton { .. }
            | Source::FnoxConfig
            | Source::AgentDir,
            _,
        ) => BASE,
    };
    for entry in &grants.filesystem {
        let (effect, access) = match entry.value {
            FileAccess::Read => (Effect::Allow, vec![Access::Read]),
            FileAccess::Write => (Effect::Allow, vec![Access::Read, Access::Write]),
            FileAccess::Deny => (Effect::Deny, all.to_vec()),
        };
        if entry.value == FileAccess::Deny {
            denied.push(entry.path.clone());
        }
        let at = rank(&entry.source, effect);
        layer.push((entry.path.clone(), at, effect, access, entry.source.clone()));
        // The user's own read entry also closes writes inside it (a read-only folder in the workspace): their choice, so
        // only logged and floor. A profile's or preset's read is inside a credential, whose own deny keeps deciding
        // writes there as a built-in block (a notice, liftable by an approved command while fnox is off).
        let users_own = matches!(
            entry.source,
            Source::Config {
                from: Provenance::File { .. },
                ..
            }
        );
        if entry.value == FileAccess::Read && users_own {
            layer.push((
                entry.path.clone(),
                at,
                Effect::Deny,
                vec![Access::Write],
                entry.source.clone(),
            ));
        }
    }
    for entry in &grants.sockets {
        let at = rank(&entry.source, entry.value);
        layer.push((
            entry.path.clone(),
            at,
            entry.value,
            vec![Access::Connect],
            entry.source.clone(),
        ));
    }
    for entry in &grants.bindings {
        let at = rank(&entry.source, entry.value);
        layer.push((
            entry.path.clone(),
            at,
            entry.value,
            vec![Access::Bind],
            entry.source.clone(),
        ));
    }
    if let (Some(folder), Some(source)) = (claude_code.and_then(|c| c.folder), &claude_source) {
        layer.push((folder.clone(), BASE, Effect::Allow, vec![Access::Write], source.clone()));
        // `claude` locks the folder itself with `proper-lockfile`: an empty `<folder>.lock` folder beside it.
        layer.push((
            lock_of(folder),
            BASE,
            Effect::Allow,
            vec![Access::Write],
            source.clone(),
        ));
    }
    let claude_sockets = claude_code.and_then(|c| c.sockets);
    if let (Some(sockets), Some(source)) = (claude_sockets, &claude_source) {
        // Its temp files and sockets; connecting stays closed (see `ClaudeCodeSockets`).
        layer.push((
            sockets.to_path_buf(),
            BASE,
            Effect::Allow,
            vec![Access::Write, Access::Bind],
            source.clone(),
        ));
    }
    // The rules so far are the base roots and DNS. A write root reopens reading too, so a broader deny above it doesn't
    // close it; each sits under any entry of the same depth.
    let mut ordered: Vec<(usize, u8, usize, Rule)> = Vec::new();
    for (index, mut rule) in std::mem::take(&mut rules.rules).into_iter().enumerate() {
        let depth = rule_path(&rule.target).map_or(0, |p| p.components().count());
        if rule.access == [Access::Write] {
            rule.access = vec![Access::Read, Access::Write];
        }
        ordered.push((depth, ROOT, index, rule));
    }
    let offset = ordered.len();
    for (index, (path, rank, effect, access, source)) in layer.into_iter().enumerate() {
        ordered.push((
            path.components().count(),
            rank,
            offset + index,
            Rule {
                effect,
                access,
                target: subpath(&path),
                source,
            },
        ));
    }
    ordered.sort_by_key(|(depth, rank, index, _)| (*depth, *rank, *index));
    rules.rules = ordered.into_iter().map(|(_, _, _, rule)| rule).collect();

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
        .filter(|e| e.value == Effect::Allow)
        .map(|e| e.path.clone())
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
    // `allow_git_control_files = true` (the user's own choice, or yolo's) leaves this rule out.
    if grants.git_control.is_none() {
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
        subpath(&crate::launch::sandbox::network::ssh::folder(facts.state_dir)),
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
        .filesystem
        .iter()
        .any(|e| e.value != FileAccess::Deny && (keychains.starts_with(&e.path) || e.path.starts_with(&keychains)));
    let (rules, links) = rules.finish(&[&config.checkout, &config.source]);
    let policy = Policy {
        rules,
        keychain,
        services: Services::Listed {
            grants: grants.services.clone(),
        },
        apps: grants.apps.clone(),
        network: grants.network(),
        listen: grants.listen.clone(),
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

/// One entry of the merged maps, expanded, with where it came from.
#[derive(Debug, Clone)]
struct PathEntry<V> {
    path: PathBuf,
    value: V,
    source: Source,
    /// A committed `config.toml` wrote it: it never reopens what a credential or the user's own deny closes.
    committed: bool,
}

/// The `[sandbox]` maps in effect, the base layer (profile, presets) under the files, expanded to absolute paths, each
/// entry with the rule source that names where it came from.
struct Grants {
    filesystem: Vec<PathEntry<FileAccess>>,
    sockets: Vec<PathEntry<Effect>>,
    bindings: Vec<PathEntry<Effect>>,
    network: Vec<NetworkGrant>,
    listen: Listen,
    services: Vec<ServiceGrant>,
    apps: Apps,
    git_control: Option<Source>,
    /// What a committed file left out: a write inside a credential or the user's own deny.
    ignored: Vec<String>,
    /// The `pi-claude-code` preset, when it's on: its options and where they came from. Its config folder is decided
    /// from the launch's environment ([`claude_code_folder`]).
    claude_code: Option<(ClaudeCodeOptions, Provenance)>,
}

/// A boolean's value in effect: the nearest file's, else `true` when the base layer opens it.
fn boolean(file: &Option<Listed<bool>>, key: &str, base: Option<Source>) -> Option<(bool, Source)> {
    match file {
        Some(listed) => Some((
            listed.value,
            Source::Config {
                key: format!("sandbox.{key}"),
                from: Provenance::File {
                    path: listed.file.clone(),
                },
            },
        )),
        None => base.map(|source| (true, source)),
    }
}

impl Grants {
    /// A base entry whose variable isn't set (no `$SSH_AUTH_SOCK`) is left out; the user's own refuses the launch.
    fn of(config: &LaunchConfig, facts: &Facts) -> Result<Self, SandboxError> {
        let settings = &config.sandbox;
        let profile = settings.profile();
        let entries = policy::profile_entries(profile);
        let from_profile = Provenance::Profile { profile };
        let expand = |grant: &str| expand_grant(grant, facts.home, facts.base_env);
        let optional = |grant: &str| match expand(grant) {
            Ok(path) => Ok(Some(path)),
            Err(SandboxError::UnsetVariable { .. }) => Ok(None),
            Err(other) => Err(other),
        };
        let config_source = |key: String, from: Provenance| Source::Config { key, from };
        let profile_source = |key: &str| config_source(format!("sandbox.{key}"), from_profile.clone());

        let presets: Vec<(Preset, Provenance)> = settings
            .presets
            .iter()
            .map(|e| (e.value, Provenance::File { path: e.file.clone() }))
            .collect();
        let preset_source = |preset: &Preset, from: &Provenance| Source::Preset {
            preset: preset.name().into(),
            from: from.clone(),
        };

        // Filesystem: the profile's reads, the presets' entries, then the files (nearest wins per key, as merged).
        let mut filesystem: Vec<PathEntry<FileAccess>> = Vec::new();
        let base = |filesystem: &mut Vec<PathEntry<FileAccess>>, path: PathBuf, value, source| {
            filesystem.retain(|e| e.path != path);
            filesystem.push(PathEntry {
                path,
                value,
                source,
                committed: false,
            });
        };
        for entry in entries.read {
            let paths = match entry {
                policy::ReadEntry::Path(grant) => optional(grant)?.into_iter().collect(),
                policy::ReadEntry::SshPublicKeys => ssh_public_keys(&facts.home.join(".ssh")),
            };
            for path in paths {
                base(&mut filesystem, path, FileAccess::Read, profile_source("filesystem"));
            }
        }
        for (preset, from) in &presets {
            let granted = policy::preset_grants(preset);
            for path in granted.write {
                base(
                    &mut filesystem,
                    expand(path)?,
                    FileAccess::Write,
                    preset_source(preset, from),
                );
            }
            for path in granted.read {
                base(
                    &mut filesystem,
                    expand(path)?,
                    FileAccess::Read,
                    preset_source(preset, from),
                );
            }
        }
        let user_source = |table: &str, key: &dyn std::fmt::Display, file: &Path| {
            config_source(
                format!("sandbox.{table}.\"{key}\""),
                Provenance::File {
                    path: file.to_path_buf(),
                },
            )
        };
        // The files' entries by expanded path: keys spelled differently (`~/x`, `$HOME/x`) are one entry. The user's own
        // deny for a path is kept over anything a committed file says for it; otherwise deny over write over read.
        let mut ignored: Vec<String> = settings
            .ignored
            .iter()
            .map(|l| format!("{} in {}: it would reopen your own deny", l.value, l.file.display()))
            .collect();
        let rank = |value: FileAccess| match value {
            FileAccess::Read => 0,
            FileAccess::Write => 1,
            FileAccess::Deny => 2,
        };
        let mut users: Vec<PathEntry<FileAccess>> = Vec::new();
        for (key, entry) in &settings.filesystem {
            let new = PathEntry {
                path: expand(key.as_str())?,
                value: entry.value,
                source: user_source("filesystem", key, &entry.file),
                committed: entry.origin == ConfigOrigin::Committed,
            };
            match users.iter().position(|e| e.path == new.path) {
                None => users.push(new),
                Some(at) => {
                    let old = &users[at];
                    let users_deny = |e: &PathEntry<FileAccess>| !e.committed && e.value == FileAccess::Deny;
                    let reopens = |e: &PathEntry<FileAccess>| e.committed && e.value != FileAccess::Deny;
                    let keep_old = match (users_deny(old) && reopens(&new), users_deny(&new) && reopens(old)) {
                        (true, _) => true,
                        (false, true) => false,
                        (false, false) => rank(old.value) >= rank(new.value),
                    };
                    let dropped = match keep_old {
                        true => new,
                        false => std::mem::replace(&mut users[at], new),
                    };
                    if reopens(&dropped) {
                        ignored.push(format!(
                            "{}: it would reopen {}, which your own deny closes",
                            explain_source(&dropped.source),
                            dropped.path.display()
                        ));
                    }
                }
            }
        }
        for entry in users {
            filesystem.retain(|e| e.path != entry.path);
            filesystem.push(entry);
        }

        // A committed opening entry at or below a credential deny, or below the user's own deny, is left out.
        let credentials: Vec<PathBuf> = policy::nono_denies()
            .into_iter()
            .filter_map(|(path, _)| expand(&path).ok())
            .collect();
        let users_denies: Vec<PathBuf> = filesystem
            .iter()
            .filter(|e| !e.committed && e.value == FileAccess::Deny)
            .map(|e| e.path.clone())
            .collect();
        filesystem.retain(|e| {
            let opens = e.committed && e.value != FileAccess::Deny;
            let closed = credentials
                .iter()
                .chain(&users_denies)
                .find(|d| e.path.starts_with(d) && **d != e.path || credentials.contains(&e.path));
            match (opens, closed) {
                (true, Some(by)) => {
                    ignored.push(format!(
                        "{}: it would reopen {}, which a credential or your own deny closes",
                        explain_source(&e.source),
                        by.display()
                    ));
                    false
                }
                (true, None) | (false, _) => true,
            }
        });

        // Sockets and bindings.
        let mut sockets: Vec<PathEntry<Effect>> = Vec::new();
        for grant in entries.unix_socket_connections {
            if let Some(path) = optional(grant)? {
                sockets.retain(|e| e.path != path);
                sockets.push(PathEntry {
                    path,
                    value: Effect::Allow,
                    source: profile_source("network.unix_socket_connections"),
                    committed: false,
                });
            }
        }
        for (key, entry) in &settings.unix_socket_connections {
            let path = expand(key.as_str())?;
            sockets.retain(|e| e.path != path);
            sockets.push(PathEntry {
                path,
                value: entry.value,
                source: user_source("network.unix_socket_connections", key, &entry.file),
                committed: entry.origin == ConfigOrigin::Committed,
            });
        }
        let mut bindings: Vec<PathEntry<Effect>> = Vec::new();
        for grant in entries.unix_socket_bindings {
            if let Some(path) = optional(grant)? {
                bindings.push(PathEntry {
                    path,
                    value: Effect::Allow,
                    source: profile_source("network.unix_socket_bindings"),
                    committed: false,
                });
            }
        }
        for (key, entry) in &settings.unix_socket_bindings {
            let path = expand(key.as_str())?;
            bindings.retain(|e| e.path != path);
            bindings.push(PathEntry {
                path,
                value: entry.value,
                source: user_source("network.unix_socket_bindings", key, &entry.file),
                committed: entry.origin == ConfigOrigin::Committed,
            });
        }

        // Connections: the profile's, the presets', then the files'.
        let mut network: Vec<NetworkGrant> = Vec::new();
        let put = |network: &mut Vec<NetworkGrant>, target: NetworkTarget, effect, source| {
            network.retain(|g| g.target != target);
            network.push(NetworkGrant { target, effect, source });
        };
        for target in entries.connections {
            put(
                &mut network,
                target,
                Effect::Allow,
                profile_source("network.connections"),
            );
        }
        for (preset, from) in &presets {
            for target in policy::preset_grants(preset).connections {
                put(&mut network, target, Effect::Allow, preset_source(preset, from));
            }
        }
        for (target, entry) in &settings.connections {
            put(
                &mut network,
                target.clone(),
                entry.value,
                user_source("network.connections", target, &entry.file),
            );
        }

        // Booleans: the nearest file's value, else the base layer's `true`.
        let preset_listen = presets.iter().find_map(|(preset, from)| {
            policy::preset_grants(preset)
                .allow_tcp_listening
                .then(|| preset_source(preset, from))
        });
        let base_listen = entries
            .allow_tcp_listening
            .then(|| profile_source("allow_tcp_listening"))
            .or(preset_listen);
        let listen = match boolean(&settings.allow_tcp_listening, "allow_tcp_listening", base_listen) {
            Some((true, source)) => Listen::Open { source },
            Some((false, source)) => Listen::Denied { source },
            None => Listen::Closed,
        };
        let base_apps = entries
            .allow_launching_apps
            .then(|| profile_source("allow_launching_apps"));
        let (apps, apps_open) = match boolean(&settings.allow_launching_apps, "allow_launching_apps", base_apps) {
            Some((true, source)) => (Apps::Open { source: source.clone() }, Some(source)),
            Some((false, source)) => (Apps::Denied { source }, None),
            None => (Apps::Closed, None),
        };
        let base_git = entries
            .allow_git_control_files
            .then(|| profile_source("allow_git_control_files"));
        let git_control = match boolean(&settings.allow_git_control_files, "allow_git_control_files", base_git) {
            Some((true, source)) => Some(source),
            Some((false, _)) | None => None,
        };

        // Mach services: the built-in list, the services launching apps needs, the profile's, then the files'.
        let mut services: Vec<ServiceGrant> = Vec::new();
        let put_service = |services: &mut Vec<ServiceGrant>, key: ServiceKey, effect, source| {
            services.retain(|g| g.key != key);
            services.push(ServiceGrant { key, effect, source });
        };
        for (name, why) in policy::BASE_SERVICES {
            put_service(
                &mut services,
                ServiceKey::Name(ServiceName::Exact((*name).to_string())),
                Effect::Allow,
                Source::BuiltIn {
                    what: format!("a service every launch may look up: {why}"),
                },
            );
        }
        if let Some(source) = apps_open {
            for name in policy::APP_SERVICES {
                put_service(
                    &mut services,
                    ServiceKey::Name(ServiceName::Exact((*name).to_string())),
                    Effect::Allow,
                    source.clone(),
                );
            }
        }
        for key in entries.services {
            put_service(&mut services, key, Effect::Allow, profile_source("macos.services"));
        }
        for (key, entry) in &settings.services {
            put_service(
                &mut services,
                key.clone(),
                entry.value,
                user_source("macos.services", key, &entry.file),
            );
        }

        let claude_code = presets.iter().find_map(|(preset, from)| match preset {
            Preset::PiClaudeCode(options) => Some((*options, from.clone())),
            Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift => None,
        });

        Ok(Self {
            filesystem,
            sockets,
            bindings,
            network,
            listen,
            services,
            apps,
            git_control,
            ignored,
            claude_code,
        })
    }

    /// The network's mode: open with no gate when `*` decides allow and the sandbox's port rules can enforce every
    /// entry (no named host decides differently from the entry that decides for it in Seatbelt); otherwise filtered,
    /// keeping this Mac direct when `*` allows.
    fn network(&self) -> Network {
        let star = self
            .network
            .iter()
            .find(|g| g.target == NetworkTarget::Anywhere)
            .map(|g| g.effect);
        let named_differs = self.network.iter().any(|g| match (&g.target, g.effect) {
            (NetworkTarget::Host { .. }, Effect::Deny) => true,
            // A host allow is enforceable directly only when every port it covers is allowed there anyway.
            (NetworkTarget::Host { port, .. }, Effect::Allow) => self.network.iter().any(|p| {
                p.effect == Effect::Deny
                    && match (&p.target, port) {
                        (NetworkTarget::AnyHostPort { port: denied }, Some(port)) => denied == port,
                        (NetworkTarget::AnyHostPort { .. }, None) => true,
                        (
                            NetworkTarget::Anywhere
                            | NetworkTarget::LocalAnyPort
                            | NetworkTarget::LocalPort { .. }
                            | NetworkTarget::Host { .. },
                            _,
                        ) => false,
                    }
            }),
            (
                NetworkTarget::Anywhere
                | NetworkTarget::AnyHostPort { .. }
                | NetworkTarget::LocalAnyPort
                | NetworkTarget::LocalPort { .. },
                _,
            ) => false,
        });
        match (star, named_differs) {
            (Some(Effect::Allow), false) => Network::Direct {
                grants: self.network.clone(),
            },
            (Some(Effect::Allow), true) => {
                // This Mac stays direct, unless an entry says otherwise.
                let mut grants = self.network.clone();
                if !grants.iter().any(|g| g.target == NetworkTarget::LocalAnyPort) {
                    let source = self
                        .network
                        .iter()
                        .find(|g| g.target == NetworkTarget::Anywhere)
                        .map(|g| g.source.clone())
                        .unwrap_or(Source::BuiltIn {
                            what: "the network".into(),
                        });
                    grants.push(NetworkGrant {
                        target: NetworkTarget::LocalAnyPort,
                        effect: Effect::Allow,
                        source,
                    });
                }
                Network::Filtered {
                    grants,
                    gate: Gate::None,
                }
            }
            (Some(Effect::Deny) | None, _) => Network::Filtered {
                grants: self.network.clone(),
                gate: Gate::None,
            },
        }
    }
}

/// How a rule source reads in a warning.
fn explain_source(source: &Source) -> String {
    match source {
        Source::Config { key, from } => format!("{key}, {from}"),
        Source::Preset { preset, from } => format!("the {preset} preset, {from}"),
        Source::Workspace
        | Source::BuiltIn { .. }
        | Source::NonoGroup { .. }
        | Source::Piton { .. }
        | Source::FnoxConfig
        | Source::AgentDir => format!("{source:?}"),
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

/// What's never configuration, for any launch in `config`'s workspace: the credential denies and every filesystem
/// `"deny"` entry, expanded (an entry naming an unset variable is left out: the sandbox refuses that launch anyway). No
/// protected area outside the checkout reaches them (`agent/protected.rs`).
pub fn never_configuration(config: &LaunchConfig, home: &Path, base_env: &Env) -> Vec<PathBuf> {
    let denies = config
        .sandbox
        .filesystem
        .iter()
        .filter(|(_, e)| e.value == FileAccess::Deny)
        .map(|(key, _)| key.as_str().to_string());
    policy::nono_denies()
        .into_iter()
        .map(|(path, _)| path)
        .chain(denies)
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

    /// Presets' writes, the user's writes (a folder or a file alike), and a user deny inside the workspace, which wins
    /// over the workspace's write because it's more specific.
    #[test]
    fn presets_writes_and_denies() {
        let f = fixture(
            "[sandbox.filesystem]\n\"~/p/repo/docs\" = \"read\"\n",
            "[sandbox]\npresets = [\"rust\"]\n[sandbox.filesystem]\n\"~/out\" = \"write\"\n\"~/f\" = \"write\"\n\"~/p/repo/secret\" = \"deny\"\n",
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
        // A user deny beats the workspace grant; a `"read"` inside it closes writing there.
        assert!(!allowed(&p, &f.repo.join("secret/k"), Access::Read));
        assert!(!allowed(&p, &f.repo.join("secret/k"), Access::Write));
        assert!(allowed(&p, &f.repo.join("docs/a.md"), Access::Read));
        assert!(!allowed(&p, &f.repo.join("docs/a.md"), Access::Write));
        assert!(allowed(&p, &f.repo.join("src/main.rs"), Access::Write));
        assert_eq!(
            p.decide(&f.repo.join("secret/k"), Access::Read).source,
            Source::Config {
                key: "sandbox.filesystem.\"~/p/repo/secret\"".into(),
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

    /// The user's own `"read"` or `"write"` inside a credential reopens it, the most specific entry deciding; the same
    /// entry from a committed file is left out, and so is a committed write inside the user's own deny.
    #[test]
    fn entries_inside_a_credential() {
        let f = fixture(
            "[sandbox]\nprofile = \"strict\"\n[sandbox.filesystem]\n\"~/.ssh/config\" = \"read\"\n\"~/Library/Keychains\" = \"write\"\n\"~/secrets\" = \"deny\"\n",
            "[sandbox.filesystem]\n\"~/.aws\" = \"write\"\n\"~/secrets/keys\" = \"write\"\n\"~/.gnupg\" = \"deny\"\n",
        );
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        let p = &r.policy;
        let h = &f.home;
        assert!(allowed(p, &h.join(".ssh/config"), Access::Read));
        assert!(!allowed(p, &h.join(".ssh/config"), Access::Write));
        assert!(!allowed(p, &h.join(".ssh/id_ed25519"), Access::Read));
        assert!(allowed(
            p,
            &h.join("Library/Keychains/login.keychain-db"),
            Access::Write
        ));
        assert!(p.keychain);
        // Committed: left out, each with a warning.
        assert!(!allowed(p, &h.join(".aws/credentials"), Access::Read));
        assert!(!allowed(p, &h.join(".aws/credentials"), Access::Write));
        assert!(!allowed(p, &h.join("secrets/keys/k"), Access::Read));
        assert!(!allowed(p, &h.join("secrets/keys/k"), Access::Write));
        assert_eq!(r.ignored.len(), 2, "{:?}", r.ignored);
        // A committed deny applies.
        assert!(!allowed(p, &h.join(".gnupg/x"), Access::Read));
    }

    /// Keys spelled differently for one path are one entry: a committed write never replaces the user's own deny, in
    /// either sort order, and is listed as left out; between the user's own, deny wins over write.
    #[test]
    fn the_same_path_spelled_differently() {
        for (global, project) in [("$HOME/secrets", "~/secrets"), ("~/secrets", "$HOME/secrets")] {
            let f = fixture(
                &format!("[sandbox.filesystem]\n\"{global}\" = \"deny\"\n"),
                &format!("[sandbox.filesystem]\n\"{project}\" = \"write\"\n"),
            );
            let base = env(&[("HOME", f.home.to_str().unwrap())]);
            let r = resolve_in(&f, &f.repo, &base, &Env::new()).unwrap();
            let secret = f.home.join("secrets/k");
            assert!(!allowed(&r.policy, &secret, Access::Read), "{global} / {project}");
            assert!(!allowed(&r.policy, &secret, Access::Write), "{global} / {project}");
            assert_eq!(r.ignored.len(), 1, "{:?}", r.ignored);
        }
        let f = fixture(
            "[sandbox.filesystem]\n\"$HOME/x\" = \"write\"\n\"~/x\" = \"deny\"\n",
            "",
        );
        let base = env(&[("HOME", f.home.to_str().unwrap())]);
        let p = policy_with(&f, &base, &Env::new()).unwrap();
        assert!(!allowed(&p, &f.home.join("x/y"), Access::Write), "deny over write");
    }

    /// A profile's read inside a credential (standard's `~/.ssh/known_hosts`) stays a built-in block for writes: the
    /// credential deny decides, so a notice explains it and an approved command lifts it while fnox is off. The user's
    /// own read entry closes writes as their choice: only logged, and floor.
    #[test]
    fn a_profiles_read_leaves_writes_to_the_credential() {
        use super::super::grant::{Credentials, all};
        let f = fixture("[sandbox.filesystem]\n\"~/docs\" = \"read\"\n", "");
        let p = policy(&f);
        let known = f.home.join(".ssh/known_hosts");
        assert!(allowed(&p, &known, Access::Read));
        assert!(!allowed(&p, &known, Access::Write));
        assert!(matches!(
            p.decide(&known, Access::Write).source,
            Source::NonoGroup { .. }
        ));
        let widened = all(&p, Credentials::Liftable);
        assert!(allowed(&widened, &known, Access::Write), "an approved command lifts it");
        assert!(
            !allowed(&all(&p, Credentials::Floor), &known, Access::Write),
            "not behind fnox"
        );
        let docs = f.home.join("docs/a");
        assert!(matches!(
            p.decide(&docs, Access::Write).source,
            Source::Config {
                from: Provenance::File { .. },
                ..
            }
        ));
        assert!(
            !allowed(&widened, &docs, Access::Write),
            "the user's own read stays floor"
        );
    }

    /// A broader deny above the workspace leaves the workspace below it as it was: the most specific decides.
    #[test]
    fn a_broader_deny_doesnt_close_the_workspace() {
        let f = fixture("[sandbox.filesystem]\n\"~/p\" = \"deny\"\n", "");
        let p = policy(&f);
        assert!(allowed(&p, &f.repo.join("src/main.rs"), Access::Write));
        assert!(allowed(&p, &f.repo.join("src/main.rs"), Access::Read));
        assert!(!allowed(&p, &f.home.join("p/other/x"), Access::Read));
    }

    /// `strict` is the base: no profile entry. `standard` (the default) reads SSH's host files, connects to the SSH agent
    /// and the network; `yolo` adds every socket and Git's control files. Each entry names its profile.
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
                Network::Direct { grants } | Network::Filtered { grants, .. } => {
                    grants.iter().filter(|g| from_profile(&g.source)).count()
                }
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
        assert_eq!(p.listen, Listen::Closed, "strict listens on no address");

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
        let from_standard = |key: &str| Source::Config {
            key: key.into(),
            from: Provenance::Profile {
                profile: Profile::Standard,
            },
        };
        assert_eq!(
            p.decide(&f.home.join(".ssh/config"), Access::Read).source,
            from_standard("sandbox.filesystem")
        );
        assert!(
            p.rules
                .iter()
                .any(|r| r.target == subpath(Path::new("/private/tmp/agent.sock")) && r.access == [Access::Connect])
        );
        assert!(
            matches!(&p.network, Network::Direct { grants } if grants.iter().any(|g| g.target == crate::config::NetworkTarget::Anywhere)),
            "standard's `*` keeps the network open: {:?}",
            p.network
        );
        assert!(p.network.proxy().is_none(), "no gate");
        assert_eq!(
            p.listen,
            Listen::Open {
                source: from_standard("sandbox.allow_tcp_listening")
            },
            "standard listens"
        );
        assert!(!allowed(p, &f.repo.join(".git/config"), Access::Write));

        let (f, yolo) = resolve("[sandbox]\nprofile = \"yolo\"\n");
        let p = &yolo.policy;
        assert!(
            allowed(p, &f.home.join(".ssh/config"), Access::Read),
            "yolo holds standard"
        );
        assert!(
            allowed(p, &f.repo.join(".git/config"), Access::Write),
            "Git's control files"
        );
        assert!(matches!(p.apps, Apps::Open { .. }), "launching apps");
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
        assert!(p.decide_listen().allowed, "yolo holds standard's listening");
    }

    /// In `strict`, TCP listening opens from the `pi-claude-code` preset (its relay) or the user's `true`, each named;
    /// the profile names itself first when it opens it too; the user's `false` closes it over both, only logged.
    #[test]
    fn tcp_listening_follows_presets_and_the_users_entry() {
        let preset = policy(&fixture(&format!("{CLAUDE_PRESET}profile = \"strict\"\n"), ""));
        assert!(matches!(
            &preset.listen,
            Listen::Open { source: Source::Preset { preset, .. } } if preset == "pi-claude-code"
        ));
        let f = fixture("[sandbox]\nprofile = \"strict\"\nallow_tcp_listening = true\n", "");
        let user = policy(&f);
        assert_eq!(
            user.decide_listen().source,
            Source::Config {
                key: "sandbox.allow_tcp_listening".into(),
                from: Provenance::File {
                    path: f.home.join(".config/piton/config.toml")
                },
            }
        );
        let closed = policy(&fixture(&format!("{CLAUDE_PRESET}allow_tcp_listening = false\n"), ""));
        assert!(
            matches!(closed.listen, Listen::Denied { .. }),
            "false wins over profile and preset"
        );
        assert!(!closed.decide_listen().allowed);
        let both = policy(&fixture(CLAUDE_PRESET, ""));
        assert!(matches!(
            &both.listen,
            Listen::Open {
                source: Source::Config {
                    from: Provenance::Profile { .. },
                    ..
                }
            }
        ));
        let strict = policy(&fixture("[sandbox]\nprofile = \"strict\"\n", ""));
        assert!(!strict.decide_listen().allowed);
        assert!(
            strict
                .seatbelt()
                .unwrap()
                .contains(&r#"(deny network-inbound (local tcp "*:*"))"#.to_string())
        );
        super::super::apply::capabilities(&strict).expect("nono accepts it");
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

    /// A broad socket entry (yolo's `/`) never reopens a credential folder or the user's deny: the more specific deny
    /// decides. An entry at or inside one opens just that path, for connecting; the daemon's sockets stay floor.
    #[test]
    fn socket_entries_and_denies_by_specificity() {
        let f = fixture(
            "[sandbox]\nprofile = \"yolo\"\n[sandbox.filesystem]\n\"~/secret\" = \"deny\"\n",
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
            "[sandbox]\nprofile = \"yolo\"\n[sandbox.filesystem]\n\"~/secret\" = \"deny\"\n\"~/other\" = \"deny\"\n\
             [sandbox.network.unix_socket_connections]\n\"~/.docker/run/docker.sock\" = \"allow\"\n\
             \"~/.gnupg/S.gpg-agent\" = \"allow\"\n\"~/secret/s.sock\" = \"allow\"\n\"/private/tmp/x\" = \"deny\"\n",
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
            allowed(&p, &g.home.join("secret/s.sock"), Access::Connect),
            "a socket allow inside the user's own deny opens that socket"
        );
        assert!(!allowed(&p, &g.home.join("secret/s.sock"), Access::Read));
        assert!(
            !allowed(&p, &g.home.join("other/o.sock"), Access::Connect),
            "the deny, over yolo's `/`"
        );
        assert!(
            !allowed(&p, Path::new("/private/tmp/x/y.sock"), Access::Connect),
            "a socket deny"
        );
        assert_eq!(
            p.decide(&docker, Access::Connect).source,
            Source::Config {
                key: "sandbox.network.unix_socket_connections.\"~/.docker/run/docker.sock\"".into(),
                from: Provenance::File {
                    path: g.home.join(".config/piton/config.toml")
                },
            }
        );

        // `"read"` reads only: a socket it covers stays closed.
        let h = fixture("[sandbox.filesystem]\n\"~/.gnupg/S.gpg-agent\" = \"read\"\n", "");
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
        let f = fixture(&format!("[sandbox.filesystem]\n\"{tmp}\" = \"write\"\n"), "");
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
            Source::Config { key, .. } if key == "sandbox.network.unix_socket_connections"
        ) && rule.access == [Access::Connect]));
        let g = fixture(
            "[sandbox.network.unix_socket_connections]\n\"$NOPE/s.sock\" = \"allow\"\n",
            "",
        );
        assert!(matches!(
            resolve_in(&g, &g.repo, &Env::new(), &Env::new()),
            Err(SandboxError::UnsetVariable { .. })
        ));
    }

    #[test]
    fn grants_expand_from_the_daemons_environment() {
        let f = fixture("[sandbox.filesystem]\n\"$CACHE/x\" = \"write\"\n", "");
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
        let f = fixture("[sandbox.filesystem]\n\"~/d/cache\" = \"write\"\n", "");
        mkdir(&f.home.join("victim"));
        link(&f.home.join("victim"), &f.home.join("d/cache"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&r.policy, &f.home.join("victim/x"), Access::Write));
        assert_eq!(reported(&r), [f.home.join("d/cache")]);
        assert_eq!(
            r.links[0].source,
            Source::Config {
                key: "sandbox.filesystem.\"~/d/cache\"".into(),
                from: Provenance::File {
                    path: f.home.join(".config/piton/config.toml")
                },
            }
        );
    }

    /// A link the sandbox can't replace is followed: the user's own (a folder moved to another disk), like `/var`.
    #[test]
    fn a_link_above_every_grant_is_followed() {
        let f = fixture("[sandbox.filesystem]\n\"~/ln/cache\" = \"write\"\n", "");
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
        let f = fixture(
            "[sandbox.filesystem]\n\"~/l/b\" = \"write\"\n\"~/x/b/c/d\" = \"write\"\n",
            "",
        );
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
        let f = fixture("[sandbox.filesystem]\n\"~/p/repo/secret\" = \"deny\"\n", "");
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
            matches!(&source, Source::Piton { what } if what.contains("allow_git_control_files")),
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

        // `allow_git_control_files = true` in the global configuration lifts them; a `false` closes yolo's.
        let g = fixture("[sandbox]\nallow_git_control_files = true\n", "");
        let p = policy(&g);
        for open in [".git/config", ".git/hooks/post-merge", ".git/commondir"] {
            assert!(allowed(&p, &g.repo.join(open), Access::Write), "{open}");
        }
        let y = fixture("[sandbox]\nprofile = \"yolo\"\nallow_git_control_files = false\n", "");
        assert!(!allowed(&policy(&y), &y.repo.join(".git/config"), Access::Write));
    }

    /// Every rule is valid Seatbelt the library accepts.
    #[test]
    fn the_rules_are_accepted_by_the_library() {
        let agent = env(&[("SSH_AUTH_SOCK", "/private/tmp/agent.sock")]);
        for profile in Profile::ALL {
            let f = fixture(
                &format!(
                    "[sandbox]\nprofile = \"{}\"\npresets = [\"mise\", \"rust\", \"node\", \"swift\"]\n\
                     [sandbox.filesystem]\n\"~/Library/Keychains\" = \"read\"\n\"~/p/repo/secret\" = \"deny\"\n\
                     [sandbox.network.unix_socket_connections]\n\"~/s.sock\" = \"allow\"\n\
                     [sandbox.network.connections]\n\"localhost:5432\" = \"allow\"\n\"*:22\" = \"deny\"\n\"evil.example\" = \"deny\"\n\
                     [sandbox.macos.services]\n\"com.example.*\" = \"deny\"\n",
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
            let every_service = rules.iter().any(|r| r == "(allow mach-lookup)");
            let apps = rules.iter().any(|r| r == "(allow lsopen)");
            match profile {
                Profile::Strict => assert!(!every_socket && !ssh_agent && !every_service && !apps),
                Profile::Standard => assert!(!every_socket && ssh_agent && every_service && !apps),
                Profile::Yolo => assert!(every_socket && ssh_agent && every_service && apps),
            }
            // A named host denied: filtered through the gate in every profile.
            assert!(matches!(p.network, Network::Filtered { .. }), "{}", profile.name());
        }
    }

    /// Mach services: `strict` allows only the built-in list, `standard` and `yolo` every service but the keychain's;
    /// the user's own entries and `allow_launching_apps` add to `strict`, each named where it came from; a deny closes
    /// what `*` opens, the most specific entry deciding.
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
            matches!(&standard.decide_service("x").source, Source::Config { key, from: Provenance::Profile { profile: Profile::Standard } } if key == "sandbox.macos.services")
        );
        assert!(
            !standard.decide_service("com.apple.secd").allowed,
            "the keychain stays closed"
        );
        assert_eq!(standard.apps, Apps::Closed);

        let yolo = policy(&fixture("[sandbox]\nprofile = \"yolo\"\n", ""));
        assert!(
            matches!(&yolo.apps, Apps::Open { source: Source::Config { key, .. } } if key == "sandbox.allow_launching_apps")
        );
        assert!(yolo.decide_service("com.apple.coreservices.appleevents").allowed);

        let closed = policy(&fixture("[sandbox.macos.services]\n\"com.example.*\" = \"deny\"\n", ""));
        assert!(
            !closed.decide_service("com.example.agent").allowed,
            "a deny closes what `*` opens"
        );
        assert!(closed.decide_service("Hammerspoon").allowed);

        let listed = fixture(
            "[sandbox]\nprofile = \"strict\"\nallow_launching_apps = true\n[sandbox.macos.services]\n\"com.example.*\" = \"allow\"\n",
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
            "launching apps"
        );
        assert!(matches!(p.apps, Apps::Open { .. }));
        assert!(!p.decide_service("com.apple.pasteboard.1").allowed);
        super::super::apply::capabilities(&p).expect("nono accepts it");

        // A user `*` opens every service in `strict` too, named after that file.
        let every = policy(&fixture(
            "[sandbox]\nprofile = \"strict\"\n[sandbox.macos.services]\n\"*\" = \"allow\"\n",
            "",
        ));
        assert!(matches!(
            every.decide_service("org.other").source,
            Source::Config {
                from: Provenance::File { .. },
                ..
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
        let g = fixture("[sandbox.filesystem]\n\"~/shared\" = \"write\"\n", "");
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
            let config = super::super::grant::widened_config(&config, &lift.entries(), &file);
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
        let exposed = fixture("[sandbox.filesystem]\n\"~/Library/Keychains\" = \"read\"\n", "");
        assert!(matches!(
            proxy(vec![]).check_sandbox(&policy(&exposed), &exposed.home),
            Err(FnoxError::Keychain {
                opened_by: KeychainOpener::Read {
                    from: Some(Provenance::File { .. })
                }
            })
        ));
        // Exposing the whole Library opens the keychains too.
        let library = fixture("[sandbox.filesystem]\n\"~/Library\" = \"read\"\n", "");
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
                    { process = \"node\", target = \"tcp-listen\" }, { target = \"com.example.agent\" }]\n";
        let f = fixture(user, "");
        let r = resolve_env(&f, &f.repo, &Env::new(), &SandboxEnv::daemon(&Env::new())).unwrap();
        let hit = |process: &str, operation: &str, target: &str| silenced(&r.silence, process, operation, target);
        for process in ["claude", "security"] {
            let entry = hit(process, "mach-lookup", "com.apple.SecurityServer").expect(process);
            assert_eq!(entry.source, SilenceSource::Preset("pi-claude-code"));
        }
        assert!(hit("node", "mach-lookup", "com.apple.SecurityServer").is_none());
        assert!(hit("claude", "mach-lookup", "com.apple.diagnosticd").is_none());
        assert!(hit("node", "network-inbound", "local:*:50958").is_some());
        assert!(hit("python3", "network-inbound", "local:*:50958").is_none());
        assert!(hit("any", "mach-lookup", "com.example.agent").is_some());
        assert!(hit("any", "mach-lookup", "com.example.other").is_none());

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
