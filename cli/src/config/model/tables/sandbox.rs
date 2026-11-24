//! `[sandbox]`: what a sandboxed pi may do beyond Piton's base policy (`launch/sandbox/`), one map per resource.
//!
//! Each map takes a path, host, or name to an effect: `[sandbox.filesystem]` (`"read"`, `"write"`, `"deny"`),
//! `[sandbox.network.connections]`, `[sandbox.network.unix_socket_connections]`,
//! `[sandbox.network.unix_socket_bindings]`, and `[sandbox.macos.services]` (`"allow"`, `"deny"`). Across levels the
//! nearest file's value wins per key; within the merged maps the most specific matching entry decides (the sandbox's
//! business, `launch/sandbox/resolve.rs`). Three booleans cover what has nothing to list; presets are toolchain path
//! bundles; profiles are built-in entries of the same maps.
//!
//! Every key may be set in any file. In a repository's committed `config.toml`, an entry that opens something
//! ([`SandboxTable::grants`]: `"read"`, `"write"`, `"allow"`, a `true` switch, a preset, `profile`, `commands`) applies
//! only once the user trusts the repository (`crate::trust`); `"deny"`, `false`, and `silence` always apply. A
//! committed `"write"` never reopens what a credential or the user's own `"deny"` closes (`launch/sandbox/`).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, is_grant_path, union};
use crate::config::model::grants::{GrantEntry, GrantFilter, Grants};

/// One file's `[sandbox]`, as written.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxTable {
    /// Which built-in entries the sandbox starts from. Unset: the nearer level's, else [`Profile::Standard`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// Toolchain path bundles, and `pi-claude-code`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    /// Listening on TCP, on every address (the sandbox can't limit it to an address or a port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_tcp_listening: Option<bool>,
    /// Writing Git's hooks and configuration, which Git on the host runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_git_control_files: Option<bool>,
    /// Launching apps and sending Apple Events: what macOS starts runs outside the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_launching_apps: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filesystem: BTreeMap<PathKey, FileAccess>,
    #[serde(default, skip_serializing_if = "NetworkTable::is_empty")]
    pub network: NetworkTable,
    #[serde(default, skip_serializing_if = "MacosTable::is_empty")]
    pub macos: MacosTable,
    /// Blocks with no path or host that stay blocked without a notice: Mach service lookups and TCP listens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub silence: Vec<Silence>,
    /// Per-command entries: a bash call that matches runs through the daemon on the host, or in the launch's sandbox
    /// widened by the grant (`[[sandbox.commands]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandGrant>,
}

/// `[sandbox.network]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTable {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub connections: BTreeMap<NetworkTarget, Effect>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unix_socket_connections: BTreeMap<PathKey, Effect>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unix_socket_bindings: BTreeMap<PathKey, Effect>,
}

impl NetworkTable {
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty() && self.unix_socket_connections.is_empty() && self.unix_socket_bindings.is_empty()
    }
}

/// `[sandbox.macos]`: what only macOS has.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MacosTable {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<ServiceKey, Effect>,
}

impl MacosTable {
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }
}

/// A filesystem entry's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileAccess {
    /// Readable (and runnable), not writable: reopens reading inside a credential deny.
    Read,
    /// Readable and writable.
    Write,
    /// No read, write, connect, or socket creation.
    Deny,
}

impl FileAccess {
    pub fn name(self) -> &'static str {
        match self {
            FileAccess::Read => "read",
            FileAccess::Write => "write",
            FileAccess::Deny => "deny",
        }
    }
}

/// Every other entry's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

/// A path key as written: absolute, `~/…`, or `$VAR/…`; the path and everything below it. No wildcards.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct PathKey(String);

impl PathKey {
    pub fn new(path: &str) -> Result<Self, String> {
        match is_grant_path(path) && !path.contains('*') {
            true => Ok(Self(path.to_string())),
            false => Err(format!(
                "{path:?}: a path is absolute, or starts with `~/` or `$` (no wildcards: an entry covers the path and \
                 everything below it)"
            )),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PathKey {
    type Error = String;

    fn try_from(path: String) -> Result<Self, String> {
        Self::new(&path)
    }
}

impl From<PathKey> for String {
    fn from(key: PathKey) -> String {
        key.0
    }
}

impl std::fmt::Display for PathKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `[sandbox.network.connections]` key: what a process connects to over TCP or UDP. Ports and `*` are enforced by
/// the sandbox itself; a named host only through Piton's gate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum NetworkTarget {
    /// `*`: any host, any port.
    Anywhere,
    /// `*:PORT`: a port on any host.
    AnyHostPort { port: u16 },
    /// `localhost:*`: every port on this Mac, every local service included.
    LocalAnyPort,
    /// `localhost:PORT`: a port on this Mac.
    LocalPort { port: u16 },
    /// A named host or IP address (`github.com`, `*.npmjs.org`, `10.0.0.5`), on one port or every port.
    Host { host: String, port: Option<u16> },
}

impl NetworkTarget {
    /// A named host or `*.domain`: only the gate can decide it.
    pub fn is_named(&self) -> bool {
        match self {
            NetworkTarget::Host { .. } => true,
            NetworkTarget::Anywhere
            | NetworkTarget::AnyHostPort { .. }
            | NetworkTarget::LocalAnyPort
            | NetworkTarget::LocalPort { .. } => false,
        }
    }

    /// How specific the entry is: higher decides over lower when both match. Hosts first (`*` < `localhost` <
    /// `*.domain`, longer first < an exact host), then a port over any port.
    pub fn specificity(&self) -> (u8, usize, u8) {
        match self {
            NetworkTarget::Anywhere => (0, 0, 0),
            NetworkTarget::AnyHostPort { .. } => (0, 0, 1),
            NetworkTarget::LocalAnyPort => (1, 0, 0),
            NetworkTarget::LocalPort { .. } => (1, 0, 1),
            NetworkTarget::Host { host, port } => {
                let rank = match host.strip_prefix("*.") {
                    Some(domain) => (2, domain.len()),
                    None => (3, 0),
                };
                (rank.0, rank.1, u8::from(port.is_some()))
            }
        }
    }
}

impl TryFrom<String> for NetworkTarget {
    type Error = String;

    fn try_from(entry: String) -> Result<Self, String> {
        let bad = |why: &str| format!("{entry:?}: {why}");
        if entry == "*" {
            return Ok(NetworkTarget::Anywhere);
        }
        if entry == "localhost:*" {
            return Ok(NetworkTarget::LocalAnyPort);
        }
        let (host, port) = match entry.rsplit_once(':') {
            Some((host, port)) => {
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| bad("the port is a number from 1 to 65535"))?;
                (host, Some(port))
            }
            None => (entry.as_str(), None),
        };
        Ok(match (host, port) {
            ("localhost", Some(port)) => NetworkTarget::LocalPort { port },
            ("localhost", None) => return Err(bad("name the port: `localhost:PORT`, or `localhost:*` for every port")),
            ("*", Some(port)) => NetworkTarget::AnyHostPort { port },
            ("*", None) => NetworkTarget::Anywhere,
            (host, port) => {
                let name = host.strip_prefix("*.").unwrap_or(host);
                let valid = !name.is_empty()
                    && name
                        .split('.')
                        .all(|label| !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
                if !valid {
                    return Err(bad(
                        "expected `localhost:PORT`, `localhost:*`, `*:PORT`, `*`, or a host (`github.com`, \
                         `*.npmjs.org`, `10.0.0.5`) with an optional `:PORT`; sockets are in \
                         `[sandbox.network.unix_socket_connections]`",
                    ));
                }
                NetworkTarget::Host {
                    host: host.to_ascii_lowercase(),
                    port,
                }
            }
        })
    }
}

impl From<NetworkTarget> for String {
    fn from(target: NetworkTarget) -> String {
        target.to_string()
    }
}

impl std::fmt::Display for NetworkTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkTarget::LocalPort { port } => write!(f, "localhost:{port}"),
            NetworkTarget::LocalAnyPort => write!(f, "localhost:*"),
            NetworkTarget::AnyHostPort { port } => write!(f, "*:{port}"),
            NetworkTarget::Anywhere => write!(f, "*"),
            NetworkTarget::Host { host, port: Some(port) } => write!(f, "{host}:{port}"),
            NetworkTarget::Host { host, port: None } => write!(f, "{host}"),
        }
    }
}

/// A `[sandbox.macos.services]` key: every Mach service, or a name or prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ServiceKey {
    /// `*`: every service but the keychain's.
    Every,
    Name(ServiceName),
}

impl ServiceKey {
    /// Whether a lookup of `service` matches.
    pub fn matches(&self, service: &str) -> bool {
        match self {
            ServiceKey::Every => true,
            ServiceKey::Name(name) => name.matches(service),
        }
    }

    /// How specific the entry is: an exact name over a longer prefix over a shorter one over `*`.
    pub fn specificity(&self) -> (u8, usize) {
        match self {
            ServiceKey::Every => (0, 0),
            ServiceKey::Name(ServiceName::Prefix(start)) => (1, start.len()),
            ServiceKey::Name(ServiceName::Exact(_)) => (2, 0),
        }
    }
}

impl TryFrom<String> for ServiceKey {
    type Error = String;

    fn try_from(entry: String) -> Result<Self, String> {
        match entry.as_str() {
            "*" => Ok(ServiceKey::Every),
            text => ServiceName::parse(text).map(ServiceKey::Name),
        }
    }
}

impl From<ServiceKey> for String {
    fn from(key: ServiceKey) -> String {
        key.to_string()
    }
}

impl std::fmt::Display for ServiceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceKey::Every => f.write_str("*"),
            ServiceKey::Name(name) => write!(f, "{name}"),
        }
    }
}

/// A Mach service's global name, or the start of one. Real names aren't all reverse-DNS
/// (`com.apple.distributed_notifications@Uv3`, an app's plain-word port), so a name is checked only for what a Seatbelt
/// string and a lookup need: not empty, and no quote, backslash, slash, whitespace, control character, or `*` other
/// than a trailing `.*`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum ServiceName {
    /// Exactly this name.
    Exact(String),
    /// Every name starting with this (`com.example.` from `com.example.*`).
    Prefix(String),
}

impl ServiceName {
    /// Text a name may be: what Seatbelt can quote and a lookup can name (a `*` is checked by the caller).
    pub fn valid_text(text: &str) -> bool {
        !text.is_empty()
            && text
                .chars()
                .all(|c| !matches!(c, '"' | '\\' | '/') && !c.is_whitespace() && !c.is_control())
    }

    /// A name as a lookup reports it, or as an entry writes it (`name.*` for a prefix).
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "service {text:?}: a Mach service name (`com.example.agent`), a prefix (`com.example.*`), or `*`; \
                 no quotes, slashes, or spaces"
            )
        };
        let (name, prefix) = match text.strip_suffix(".*") {
            Some(start) => (format!("{start}."), true),
            None => (text.to_string(), false),
        };
        if !Self::valid_text(&name) || name.contains('*') || name == "." {
            return Err(bad());
        }
        Ok(match prefix {
            true => ServiceName::Prefix(name),
            false => ServiceName::Exact(name),
        })
    }

    /// Whether a lookup of `service` matches.
    pub fn matches(&self, service: &str) -> bool {
        match self {
            ServiceName::Exact(name) => name == service,
            ServiceName::Prefix(start) => service.starts_with(start.as_str()),
        }
    }
}

impl std::fmt::Display for ServiceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceName::Exact(name) => write!(f, "{name}"),
            ServiceName::Prefix(start) => write!(f, "{start}*"),
        }
    }
}

/// One `silence` entry: `{ target = "...", process = "..." }`, for a block that names no path or host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Silence {
    pub target: SilenceTarget,
    /// Only blocks of processes with this name (`claude`, `security`). Unset: every process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// What a `silence` entry's `target` names. A path or host is decided by a map entry instead (`"deny"`: only logged).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum SilenceTarget {
    /// A Mach service (`com.apple.SecurityServer`), matched exactly against a lookup of it.
    Service(String),
    /// `tcp-listen`: a refused TCP listen, on any port.
    TcpListen,
}

/// How a `silence` entry names TCP listening.
pub const TCP_LISTEN_TARGET: &str = "tcp-listen";

impl TryFrom<String> for SilenceTarget {
    type Error = String;

    fn try_from(target: String) -> Result<Self, String> {
        if target == TCP_LISTEN_TARGET {
            return Ok(SilenceTarget::TcpListen);
        }
        // A host and port (`example.com:443`) isn't a service: it's a connection entry's.
        let host_port = target
            .rsplit_once(':')
            .is_some_and(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()));
        match ServiceName::valid_text(&target) && !target.contains('*') && !is_grant_path(&target) && !host_port {
            true => Ok(SilenceTarget::Service(target)),
            false => Err(format!(
                "silence target {target:?}: a Mach service name (`com.apple.SecurityServer`) or `tcp-listen`; a path \
                 or host stays blocked without a notice with a `\"deny\"` entry in its table"
            )),
        }
    }
}

impl From<SilenceTarget> for String {
    fn from(target: SilenceTarget) -> String {
        match target {
            SilenceTarget::Service(name) => name,
            SilenceTarget::TcpListen => TCP_LISTEN_TARGET.into(),
        }
    }
}

impl Silence {
    /// The entry as a TOML inline table, as a notice names it.
    pub fn inline(&self) -> String {
        let target = String::from(self.target.clone());
        match &self.process {
            Some(process) => format!(
                "{{ process = {}, target = {} }}",
                toml_string(process),
                toml_string(&target)
            ),
            None => format!("{{ target = {} }}", toml_string(&target)),
        }
    }
}

/// A TOML basic string.
fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_string()).to_string()
}

/// `[[sandbox.commands]]`: where a matching command runs, and with what.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "CommandEntry", into = "CommandEntry")]
pub struct CommandGrant {
    /// A `bash(...)` permission rule, matched by the daemon as the permissions extension matches it.
    pub pattern: CommandPattern,
    pub runtime: CommandRuntime,
}

/// Where a per-command entry's command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRuntime {
    /// `runtime = "host"`: no sandbox, the user's full access.
    Host,
    /// `runtime = "sandbox"` with its `grant`: the launch's sandbox widened by it, never past the floor.
    Sandbox(Grant),
}

/// A `[[sandbox.commands]]` entry as written: `match`, a required `runtime`, and `grant` for a sandbox entry only.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CommandEntry {
    #[serde(rename = "match")]
    pattern: CommandPattern,
    #[serde(default)]
    runtime: Option<RuntimeName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grant: Option<Grant>,
}

/// The words `runtime` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RuntimeName {
    Host,
    Sandbox,
}

impl TryFrom<CommandEntry> for CommandGrant {
    type Error = String;

    fn try_from(entry: CommandEntry) -> Result<Self, String> {
        let rule = String::from(entry.pattern.clone());
        let runtime = match (entry.runtime, entry.grant) {
            (None, _) => {
                return Err(format!(
                    "sandbox.commands {rule}: `runtime` is required: \"sandbox\" (with `grant`) or \"host\""
                ));
            }
            (Some(RuntimeName::Host), None) => CommandRuntime::Host,
            (Some(RuntimeName::Host), Some(_)) => {
                return Err(format!(
                    "sandbox.commands {rule}: `grant` is for `runtime = \"sandbox\"`; a host command runs with no \
                     sandbox"
                ));
            }
            (Some(RuntimeName::Sandbox), Some(grant)) => CommandRuntime::Sandbox(grant),
            (Some(RuntimeName::Sandbox), None) => {
                return Err(format!(
                    "sandbox.commands {rule}: `runtime = \"sandbox\"` needs `grant`: \"all\" or a table of entries"
                ));
            }
        };
        Ok(Self {
            pattern: entry.pattern,
            runtime,
        })
    }
}

impl From<CommandGrant> for CommandEntry {
    fn from(grant: CommandGrant) -> Self {
        let (runtime, grant_value) = match grant.runtime {
            CommandRuntime::Host => (RuntimeName::Host, None),
            CommandRuntime::Sandbox(g) => (RuntimeName::Sandbox, Some(g)),
        };
        Self {
            pattern: grant.pattern,
            runtime: Some(runtime),
            grant: grant_value,
        }
    }
}

/// The command pattern of a `bash(...)` rule: `bash(git push *)` is `git push *`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandPattern(String);

impl CommandPattern {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
        match pattern.is_empty() || pattern.contains('\n') {
            true => Err("a command pattern is one non-empty line".into()),
            false => Ok(Self(pattern.to_string())),
        }
    }

    /// The pattern inside `bash(...)`.
    pub fn pattern(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommandPattern {
    type Error = String;

    fn try_from(rule: String) -> Result<Self, String> {
        let inner = rule
            .strip_prefix("bash(")
            .and_then(|r| r.strip_suffix(')'))
            .ok_or_else(|| format!("match {rule:?}: a `bash(...)` rule, such as \"bash(git push *)\""))?;
        Self::new(inner).map_err(|why| format!("match {rule:?}: {why}"))
    }
}

impl From<CommandPattern> for String {
    fn from(pattern: CommandPattern) -> String {
        format!("bash({})", pattern.0)
    }
}

/// What a per-command grant gives the command beyond the launch's sandbox. Never the floor: protected files,
/// configuration levels, the daemon's state, the user's `"deny"`, and credentials while fnox is on (`launch/sandbox/`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Grant {
    /// `"all"`: everything except the floor. What persisting a sandbox answer saves.
    All(AllGrant),
    /// Entries in `[sandbox]`'s shape, added as if the file that lists the command held them.
    Entries(GrantEntries),
}

/// The word `all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AllGrant {
    All,
}

/// A narrowed grant: the `[sandbox]` keys that widen (no profile, `silence`, or commands).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantEntries {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_tcp_listening: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_git_control_files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_launching_apps: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filesystem: BTreeMap<PathKey, FileAccess>,
    #[serde(default, skip_serializing_if = "NetworkTable::is_empty")]
    pub network: NetworkTable,
    #[serde(default, skip_serializing_if = "MacosTable::is_empty")]
    pub macos: MacosTable,
}

impl GrantEntries {
    /// The grant as a `[sandbox]` table: what [`SandboxSettings::add`] folds in.
    pub fn table(&self) -> SandboxTable {
        SandboxTable {
            presets: self.presets.clone(),
            allow_tcp_listening: self.allow_tcp_listening,
            allow_git_control_files: self.allow_git_control_files,
            allow_launching_apps: self.allow_launching_apps,
            filesystem: self.filesystem.clone(),
            network: self.network.clone(),
            macos: self.macos.clone(),
            ..SandboxTable::default()
        }
    }
}

/// A built-in set of entries the sandbox starts from. The entries are in `launch/sandbox/policy.rs`; any file can
/// close one with a `"deny"` or a `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Nothing beyond the base.
    Strict,
    /// What most users need to try Piton without configuring anything.
    #[default]
    Standard,
    /// Standard and more: close to host mode.
    Yolo,
}

impl Profile {
    #[cfg(test)]
    pub const ALL: [Profile; 3] = [Profile::Strict, Profile::Standard, Profile::Yolo];

    pub fn name(self) -> &'static str {
        match self {
            Profile::Strict => "strict",
            Profile::Standard => "standard",
            Profile::Yolo => "yolo",
        }
    }
}

/// A named bundle of path entries. Written as its name, or as a table with the name and the preset's own options:
/// `{ name = "pi-claude-code", login = "keychain" }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "toml::Value", into = "toml::Value")]
pub enum Preset {
    /// mise's cache and install bookkeeping, never its installed tools (installs ask, #241). Each toolchain preset
    /// grants that toolchain's caches and state, which host builds reuse; the folders are in `launch/sandbox/policy.rs`.
    Mise,
    /// Cargo's registry and git checkouts and rustup's downloads, never `~/.cargo/bin` or installed toolchains.
    Rust,
    /// npm, pnpm, bun, and Yarn caches.
    Node,
    /// SwiftPM's caches, clang's module cache, and Xcode's DerivedData.
    Swift,
    /// What the pi-claude-code extension's `claude` writes: the config folder `PI_CLAUDE_CODE_CONFIG_DIR` names (never
    /// the interactive Claude Code's, which loads hooks and plugins), and its log cache; the extension's local relay;
    /// and, with `login = "keychain"`, the keychain.
    PiClaudeCode(ClaudeCodeOptions),
}

/// The `pi-claude-code` preset's options. Unset options are the nearest level's that sets them, else their default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeCodeOptions {
    /// How the extension's `claude` signs in. Default: [`ClaudeLogin::Token`].
    pub login: Option<ClaudeLogin>,
}

impl ClaudeCodeOptions {
    pub fn login(&self) -> ClaudeLogin {
        self.login.unwrap_or_default()
    }
}

/// How the extension's `claude` signs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClaudeLogin {
    /// `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` in pi's environment (a placeholder behind fnox): no keychain.
    #[default]
    Token,
    /// A `claude auth login` saved in the keychain: the keychain opens to the sandbox. Refused while fnox applies.
    Keychain,
}

impl ClaudeLogin {
    pub fn name(self) -> &'static str {
        match self {
            ClaudeLogin::Token => "token",
            ClaudeLogin::Keychain => "keychain",
        }
    }
}

impl Preset {
    #[cfg(test)]
    pub const ALL: [Preset; 5] = [
        Preset::Mise,
        Preset::Rust,
        Preset::Node,
        Preset::Swift,
        Preset::PiClaudeCode(ClaudeCodeOptions { login: None }),
    ];

    /// Every preset's name, as written.
    pub const NAMES: [&'static str; 5] = ["mise", "rust", "node", "swift", "pi-claude-code"];

    pub fn name(&self) -> &'static str {
        match self {
            Preset::Mise => "mise",
            Preset::Rust => "rust",
            Preset::Node => "node",
            Preset::Swift => "swift",
            Preset::PiClaudeCode(_) => "pi-claude-code",
        }
    }

    /// Whether this entry sets any of its preset's options.
    pub fn has_options(&self) -> bool {
        match self {
            Preset::PiClaudeCode(options) => options.login.is_some(),
            Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift => false,
        }
    }

    /// Folds a nearer entry for the same preset in: per option, the nearer value when it sets one.
    pub fn merge(&mut self, nearer: &Preset) {
        match (self, nearer) {
            (Preset::PiClaudeCode(options), Preset::PiClaudeCode(n)) => {
                if n.login.is_some() {
                    options.login = n.login;
                }
            }
            (Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift | Preset::PiClaudeCode(_), _) => {}
        }
    }
}

impl TryFrom<toml::Value> for Preset {
    type Error = String;

    fn try_from(value: toml::Value) -> Result<Self, String> {
        let (name, mut options) = match value {
            toml::Value::String(name) => (name, toml::Table::new()),
            toml::Value::Table(mut table) => match table.remove("name") {
                Some(toml::Value::String(name)) => (name, table),
                Some(_) | None => return Err("a preset with options needs `name = \"...\"`".into()),
            },
            other @ (toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_)
            | toml::Value::Array(_)) => {
                return Err(format!(
                    "a preset is a name or a table with `name`, not {}",
                    other.type_str()
                ));
            }
        };
        let preset = match name.as_str() {
            "mise" => Preset::Mise,
            "rust" => Preset::Rust,
            "node" => Preset::Node,
            "swift" => Preset::Swift,
            "pi-claude-code" => {
                let login = match options.remove("login") {
                    None => None,
                    Some(toml::Value::String(login)) => Some(match login.as_str() {
                        "token" => ClaudeLogin::Token,
                        "keychain" => ClaudeLogin::Keychain,
                        other => return Err(format!("pi-claude-code login {other:?}: \"token\" or \"keychain\"")),
                    }),
                    Some(_) => return Err("pi-claude-code login: \"token\" or \"keychain\"".into()),
                };
                Preset::PiClaudeCode(ClaudeCodeOptions { login })
            }
            other => return Err(format!("unknown preset {other:?}: {}", Preset::NAMES.join(", "))),
        };
        match options.keys().next() {
            Some(key) => Err(format!("the {} preset has no option `{key}`", preset.name())),
            None => Ok(preset),
        }
    }
}

impl From<Preset> for toml::Value {
    /// The name alone when no option is set.
    fn from(preset: Preset) -> Self {
        let mut table = toml::Table::new();
        match &preset {
            Preset::PiClaudeCode(ClaudeCodeOptions { login: Some(login) }) => {
                table.insert("login".into(), toml::Value::String(login.name().into()));
            }
            Preset::PiClaudeCode(ClaudeCodeOptions { login: None })
            | Preset::Mise
            | Preset::Rust
            | Preset::Node
            | Preset::Swift => {}
        }
        match table.is_empty() {
            true => toml::Value::String(preset.name().into()),
            false => {
                table.insert("name".into(), toml::Value::String(preset.name().into()));
                toml::Value::Table(table)
            }
        }
    }
}

impl TableValue for SandboxTable {
    fn grants(&self) -> Grants {
        SandboxTable::grants(self)
    }
    fn gated(self, filter: &GrantFilter) -> (Self, Vec<GrantEntry>) {
        self.filtered(filter)
    }
}

/// A boolean switch as a grant: `true` opens, `false` only closes.
fn switch_grant(key: &str, value: bool) -> Option<GrantEntry> {
    match value {
        true => Some(GrantEntry::set(key, true)),
        false => None,
    }
}

fn file_grant(key: &PathKey, access: FileAccess) -> Option<GrantEntry> {
    match access {
        FileAccess::Read | FileAccess::Write => {
            Some(GrantEntry::entry("sandbox.filesystem", key.as_str(), access.name()))
        }
        FileAccess::Deny => None,
    }
}

fn effect_grant(table: &str, key: &str, effect: Effect) -> Option<GrantEntry> {
    match effect {
        Effect::Allow => Some(GrantEntry::entry(table, key, "allow")),
        Effect::Deny => None,
    }
}

fn preset_grant(preset: &Preset) -> GrantEntry {
    GrantEntry::item("sandbox.presets", toml::Value::from(*preset))
}

fn command_grant(command: &CommandGrant) -> GrantEntry {
    let value =
        toml::Value::try_from(command.clone()).unwrap_or_else(|_| toml::Value::String(command.pattern.clone().into()));
    GrantEntry::item("sandbox.commands", value)
}

impl SandboxTable {
    /// Every entry that opens something: what the file grants. `"deny"` entries, `false` switches, and `silence` only
    /// close, so they're never grants.
    pub fn grants(&self) -> Grants {
        let mut grants = Grants::new();
        if let Some(profile) = self.profile {
            grants.insert(GrantEntry::set("sandbox.profile", profile.name()));
        }
        for (key, value) in [
            ("sandbox.allow_tcp_listening", self.allow_tcp_listening),
            ("sandbox.allow_git_control_files", self.allow_git_control_files),
            ("sandbox.allow_launching_apps", self.allow_launching_apps),
        ] {
            grants.extend(value.and_then(|value| switch_grant(key, value)));
        }
        grants.extend(self.presets.iter().map(preset_grant));
        grants.extend(self.filesystem.iter().filter_map(|(k, a)| file_grant(k, *a)));
        let maps = [
            ("sandbox.network.connections", effects(&self.network.connections)),
            (
                "sandbox.network.unix_socket_connections",
                effects(&self.network.unix_socket_connections),
            ),
            (
                "sandbox.network.unix_socket_bindings",
                effects(&self.network.unix_socket_bindings),
            ),
            ("sandbox.macos.services", effects(&self.macos.services)),
        ];
        for (table, entries) in maps {
            grants.extend(entries.into_iter().filter_map(|(k, e)| effect_grant(table, &k, e)));
        }
        grants.extend(self.commands.iter().map(command_grant));
        grants
    }

    /// The table with only the grants `filter` keeps; restrictions stay. Returns the grants left out.
    pub fn filtered(mut self, filter: &GrantFilter) -> (Self, Vec<GrantEntry>) {
        let mut out = Vec::new();
        filter.retain_option(
            &mut self.profile,
            |p| Some(GrantEntry::set("sandbox.profile", p.name())),
            &mut out,
        );
        filter.retain_option(
            &mut self.allow_tcp_listening,
            |v| switch_grant("sandbox.allow_tcp_listening", *v),
            &mut out,
        );
        filter.retain_option(
            &mut self.allow_git_control_files,
            |v| switch_grant("sandbox.allow_git_control_files", *v),
            &mut out,
        );
        filter.retain_option(
            &mut self.allow_launching_apps,
            |v| switch_grant("sandbox.allow_launching_apps", *v),
            &mut out,
        );
        filter.retain(&mut self.presets, preset_grant, &mut out);
        filter.retain_map(&mut self.filesystem, |k, a| file_grant(k, *a), &mut out);
        filter.retain_map(
            &mut self.network.connections,
            |k, e| effect_grant("sandbox.network.connections", &k.to_string(), *e),
            &mut out,
        );
        filter.retain_map(
            &mut self.network.unix_socket_connections,
            |k, e| effect_grant("sandbox.network.unix_socket_connections", k.as_str(), *e),
            &mut out,
        );
        filter.retain_map(
            &mut self.network.unix_socket_bindings,
            |k, e| effect_grant("sandbox.network.unix_socket_bindings", k.as_str(), *e),
            &mut out,
        );
        filter.retain_map(
            &mut self.macos.services,
            |k, e| effect_grant("sandbox.macos.services", &k.to_string(), *e),
            &mut out,
        );
        filter.retain(&mut self.commands, command_grant, &mut out);
        (self, out)
    }
}

/// A map's entries by their key as written.
fn effects<K: std::fmt::Display>(map: &BTreeMap<K, Effect>) -> Vec<(String, Effect)> {
    map.iter().map(|(k, e)| (k.to_string(), *e)).collect()
}

impl Merge for SandboxTable {
    /// The nearest value wins per key; presets and commands add.
    fn merge(&mut self, n: Self) {
        let SandboxTable {
            profile,
            presets,
            allow_tcp_listening,
            allow_git_control_files,
            allow_launching_apps,
            filesystem,
            network,
            macos,
            silence,
            commands,
        } = n;
        super::nearer(&mut self.profile, profile);
        super::nearer(&mut self.allow_tcp_listening, allow_tcp_listening);
        super::nearer(&mut self.allow_git_control_files, allow_git_control_files);
        super::nearer(&mut self.allow_launching_apps, allow_launching_apps);
        for preset in presets {
            match self.presets.iter_mut().find(|p| p.name() == preset.name()) {
                Some(existing) => existing.merge(&preset),
                None => self.presets.push(preset),
            }
        }
        self.filesystem.extend(filesystem);
        self.network.connections.extend(network.connections);
        self.network
            .unix_socket_connections
            .extend(network.unix_socket_connections);
        self.network.unix_socket_bindings.extend(network.unix_socket_bindings);
        self.macos.services.extend(macos.services);
        union(&mut self.silence, silence);
        union(&mut self.commands, commands);
    }
}

/// A value with the file that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed<T> {
    pub value: T,
    pub file: PathBuf,
}

/// Which file of a level wrote an entry: decides whether a committed entry may reopen what the user closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A project's or parent folder's committed `config.toml`: travels with the repository.
    Committed,
    /// The global files or a `config.local.toml`: the user's own.
    UsersOwn,
}

/// One entry of a map, with the file that set its value and whether that file is the user's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<T> {
    pub value: T,
    pub file: PathBuf,
    pub origin: Origin,
}

/// The `[sandbox]` of a launch's whole chain, every entry with the file it came from: what `piton sandbox show` and
/// `why` name. The profile's and presets' entries are added by the sandbox (`launch/sandbox/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxSettings {
    /// The nearest file's `profile`; `None` when no file sets one.
    pub profile: Option<Listed<Profile>>,
    pub presets: Vec<Entry<Preset>>,
    pub allow_tcp_listening: Option<Listed<bool>>,
    pub allow_git_control_files: Option<Listed<bool>>,
    pub allow_launching_apps: Option<Listed<bool>>,
    pub filesystem: BTreeMap<PathKey, Entry<FileAccess>>,
    pub connections: BTreeMap<NetworkTarget, Entry<Effect>>,
    pub unix_socket_connections: BTreeMap<PathKey, Entry<Effect>>,
    pub unix_socket_bindings: BTreeMap<PathKey, Entry<Effect>>,
    pub services: BTreeMap<ServiceKey, Entry<Effect>>,
    pub silence: Vec<Listed<Silence>>,
    /// Per-command grants, farthest file first; the first that matches a call decides.
    pub commands: Vec<Listed<CommandGrant>>,
    /// Entries a committed file wrote over the user's own `"deny"` for the same path, left out: what `piton sandbox
    /// show` warns about.
    pub ignored: Vec<Listed<String>>,
}

impl SandboxSettings {
    /// Folds in one file's table, files farthest first: each value replaces a farther one.
    pub fn add(&mut self, table: SandboxTable, file: &Path, origin: Origin) {
        let SandboxTable {
            profile,
            presets,
            allow_tcp_listening,
            allow_git_control_files,
            allow_launching_apps,
            filesystem,
            network,
            macos,
            silence,
            commands,
        } = table;
        if let Some(profile) = profile {
            self.profile = Some(listed(profile, file));
        }
        for (slot, value) in [
            (&mut self.allow_tcp_listening, allow_tcp_listening),
            (&mut self.allow_git_control_files, allow_git_control_files),
            (&mut self.allow_launching_apps, allow_launching_apps),
        ] {
            if let Some(value) = value {
                *slot = Some(listed(value, file));
            }
        }
        // A preset is listed once, with the file that decided it: the nearest that sets one of its options, else the
        // first that lists it.
        for preset in presets {
            match self.presets.iter_mut().find(|p| p.value.name() == preset.name()) {
                Some(existing) => {
                    existing.value.merge(&preset);
                    if preset.has_options() {
                        existing.file = file.to_path_buf();
                        existing.origin = origin;
                    }
                }
                None => self.presets.push(Entry {
                    value: preset,
                    file: file.to_path_buf(),
                    origin,
                }),
            }
        }
        // A committed file never reopens a path the user's own file denies (a deeper key is the sandbox's to check).
        for (key, access) in filesystem {
            let reopens = self
                .filesystem
                .get(&key)
                .is_some_and(|existing| existing.origin == Origin::UsersOwn && existing.value == FileAccess::Deny)
                && origin == Origin::Committed
                && access != FileAccess::Deny;
            match reopens {
                true => self.ignored.push(listed(
                    format!("sandbox.filesystem.\"{key}\" = \"{}\"", access.name()),
                    file,
                )),
                false => {
                    self.filesystem.insert(
                        key,
                        Entry {
                            value: access,
                            file: file.to_path_buf(),
                            origin,
                        },
                    );
                }
            }
        }
        add_entries(&mut self.connections, network.connections, file, origin);
        add_entries(
            &mut self.unix_socket_connections,
            network.unix_socket_connections,
            file,
            origin,
        );
        add_entries(
            &mut self.unix_socket_bindings,
            network.unix_socket_bindings,
            file,
            origin,
        );
        add_entries(&mut self.services, macos.services, file, origin);
        for entry in silence {
            if !self.silence.iter().any(|l| l.value == entry) {
                self.silence.push(listed(entry, file));
            }
        }
        for command in commands {
            if !self.commands.iter().any(|l| l.value == command) {
                self.commands.push(listed(command, file));
            }
        }
    }

    /// The profile in effect: the nearest file's, else the default.
    pub fn profile(&self) -> Profile {
        self.profile.as_ref().map(|p| p.value).unwrap_or_default()
    }
}

fn listed<T>(value: T, file: &Path) -> Listed<T> {
    Listed {
        value,
        file: file.to_path_buf(),
    }
}

fn add_entries<K: Ord, V>(map: &mut BTreeMap<K, Entry<V>>, values: BTreeMap<K, V>, file: &Path, origin: Origin) {
    for (key, value) in values {
        map.insert(
            key,
            Entry {
                value,
                file: file.to_path_buf(),
                origin,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> SandboxTable {
        toml::from_str(text).unwrap()
    }

    fn path(text: &str) -> PathKey {
        PathKey::new(text).unwrap()
    }

    /// Every resource is a map of key to effect; nearer values win per key.
    #[test]
    fn maps_parse_and_the_nearest_value_wins() {
        let mut base = table(
            "profile = \"strict\"\npresets = [\"rust\"]\nallow_tcp_listening = true\n[filesystem]\n\"/a\" = \
             \"write\"\n\"~/secret\" = \"deny\"\n[network.connections]\n\"*\" = \"allow\"\n\"github.com:22\" = \
             \"deny\"\n[network.unix_socket_connections]\n\"$SSH_AUTH_SOCK\" = \"allow\"\n[macos.services]\n\"com.example.*\" \
             = \"allow\"\n",
        );
        base.merge(table(
            "presets = [\"swift\", \"rust\"]\nallow_tcp_listening = false\n[filesystem]\n\"/a\" = \"read\"\n\"/b\" = \
             \"write\"\n[network.connections]\n\"*\" = \"deny\"\n",
        ));
        assert_eq!(base.profile, Some(Profile::Strict), "a level without it keeps it");
        assert_eq!(base.presets, [Preset::Rust, Preset::Swift]);
        assert_eq!(base.allow_tcp_listening, Some(false), "the nearest boolean wins");
        assert_eq!(base.filesystem[&path("/a")], FileAccess::Read, "the nearest value wins");
        assert_eq!(base.filesystem[&path("/b")], FileAccess::Write);
        assert_eq!(base.filesystem[&path("~/secret")], FileAccess::Deny);
        assert_eq!(base.network.connections[&NetworkTarget::Anywhere], Effect::Deny);
        assert_eq!(
            base.network.connections[&NetworkTarget::Host {
                host: "github.com".into(),
                port: Some(22)
            }],
            Effect::Deny
        );
        assert_eq!(
            base.network.unix_socket_connections[&path("$SSH_AUTH_SOCK")],
            Effect::Allow
        );
        assert_eq!(
            base.macos.services[&ServiceKey::Name(ServiceName::Prefix("com.example.".into()))],
            Effect::Allow
        );
        // Written back as given.
        let text = toml::to_string(&base).unwrap();
        assert_eq!(
            toml::from_str::<SandboxTable>(&text).unwrap(),
            base,
            "round trip: {text}"
        );
    }

    /// Each entry keeps the file that set its value, and whether that file is the user's own.
    #[test]
    fn settings_keep_where_each_entry_came_from() {
        let (global, repo) = (Path::new("/g/config.toml"), Path::new("/r/.piton/config.toml"));
        let mut s = SandboxSettings::default();
        assert_eq!(s.profile(), Profile::Standard, "the default");
        s.add(
            table("profile = \"strict\"\npresets = [\"mise\"]\n[filesystem]\n\"/a\" = \"deny\"\n\"/c\" = \"read\""),
            global,
            Origin::UsersOwn,
        );
        s.add(
            table("presets = [\"mise\"]\n[filesystem]\n\"/c\" = \"deny\"\n\"/b\" = \"write\""),
            repo,
            Origin::Committed,
        );
        assert_eq!(s.profile(), Profile::Strict);
        assert_eq!(s.profile.as_ref().unwrap().file, global);
        assert_eq!(
            s.filesystem[&path("/b")],
            Entry {
                value: FileAccess::Write,
                file: repo.into(),
                origin: Origin::Committed
            }
        );
        assert_eq!(s.filesystem[&path("/a")].value, FileAccess::Deny);
        assert_eq!(
            s.filesystem[&path("/c")].origin,
            Origin::Committed,
            "a committed deny replaces a read"
        );
        assert_eq!(s.presets.len(), 1);
        assert_eq!(s.presets[0].file, global, "the first file that listed it");
    }

    /// A committed file can't write over the user's own `"deny"` for the same path; a local file can.
    #[test]
    fn a_committed_file_never_reopens_the_users_deny() {
        let (global, repo, local) = (
            Path::new("/g/config.toml"),
            Path::new("/r/.piton/config.toml"),
            Path::new("/r/.piton/config.local.toml"),
        );
        let mut s = SandboxSettings::default();
        s.add(
            table("[filesystem]\n\"~/secrets\" = \"deny\""),
            global,
            Origin::UsersOwn,
        );
        s.add(
            table("[filesystem]\n\"~/secrets\" = \"write\""),
            repo,
            Origin::Committed,
        );
        assert_eq!(s.filesystem[&path("~/secrets")].value, FileAccess::Deny);
        assert_eq!(s.ignored.len(), 1);
        assert_eq!(s.ignored[0].file, repo);
        s.add(table("[filesystem]\n\"~/secrets\" = \"read\""), local, Origin::UsersOwn);
        assert_eq!(
            s.filesystem[&path("~/secrets")].value,
            FileAccess::Read,
            "the user's own choice"
        );
    }

    #[test]
    fn profiles_are_named() {
        for profile in Profile::ALL {
            let parsed: SandboxTable = toml::from_str(&format!("profile = \"{}\"", profile.name())).unwrap();
            assert_eq!(parsed.profile, Some(profile));
        }
        assert!(toml::from_str::<SandboxTable>("profile = \"open\"").is_err());
    }

    /// Every form a connection key takes, and what isn't one (a socket path included).
    #[test]
    fn connection_keys_are_typed() {
        let parse = |text: &str| NetworkTarget::try_from(text.to_string());
        assert_eq!(parse("localhost:5432"), Ok(NetworkTarget::LocalPort { port: 5432 }));
        assert_eq!(parse("localhost:*"), Ok(NetworkTarget::LocalAnyPort));
        assert_eq!(parse("*:22"), Ok(NetworkTarget::AnyHostPort { port: 22 }));
        assert_eq!(parse("*"), Ok(NetworkTarget::Anywhere));
        assert_eq!(
            parse("GitHub.com:22"),
            Ok(NetworkTarget::Host {
                host: "github.com".into(),
                port: Some(22)
            })
        );
        assert_eq!(
            parse("*.npmjs.org"),
            Ok(NetworkTarget::Host {
                host: "*.npmjs.org".into(),
                port: None
            })
        );
        for bad in [
            "localhost",
            "github.com:0",
            "github.com:x",
            "github.com:70000",
            "a..b",
            "",
            "https://github.com",
            "$SSH_AUTH_SOCK",
            "/tmp/x.sock",
            "foo bar",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(toml::from_str::<SandboxTable>("[network.connections]\n\"/tmp/x.sock\" = \"allow\"").is_err());
        assert!(toml::from_str::<SandboxTable>("[network.connections]\n\"*\" = \"read\"").is_err());
    }

    /// Specificity: a host over `localhost` over `*`, a longer domain over a shorter, a port over any port.
    #[test]
    fn connection_specificity() {
        let s = |text: &str| NetworkTarget::try_from(text.to_string()).unwrap().specificity();
        assert!(s("*") < s("*:22"));
        assert!(s("*:22") < s("localhost:*"));
        assert!(s("localhost:*") < s("localhost:5432"));
        assert!(s("localhost:5432") < s("*.npmjs.org"));
        assert!(s("*.org") < s("*.npmjs.org"));
        assert!(s("*.npmjs.org") < s("registry.npmjs.org"));
        assert!(s("github.com") < s("github.com:22"));
    }

    /// A service key is `*`, a name, or a `name.*` prefix; real names that aren't reverse-DNS parse too.
    #[test]
    fn service_keys_are_typed() {
        let parse = |text: &str| ServiceKey::try_from(text.to_string());
        let exact = |n: &str| Ok(ServiceKey::Name(ServiceName::Exact(n.into())));
        assert_eq!(parse("*"), Ok(ServiceKey::Every));
        assert_eq!(parse("com.apple.fonts"), exact("com.apple.fonts"));
        assert_eq!(
            parse("com.apple.distributed_notifications@Uv3"),
            exact("com.apple.distributed_notifications@Uv3")
        );
        assert_eq!(parse("Hammerspoon"), exact("Hammerspoon"));
        assert_eq!(
            parse("com.example.*"),
            Ok(ServiceKey::Name(ServiceName::Prefix("com.example.".into())))
        );
        for bad in ["", ".*", "a*b", "com.*.x", "a\"b", "a b", "a/b", "a\\b", "a\nb"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let prefix = ServiceName::Prefix("com.example.".into());
        assert!(prefix.matches("com.example.agent"));
        assert!(!prefix.matches("com.examplex"));
        let spec = |text: &str| ServiceKey::try_from(text.to_string()).unwrap().specificity();
        assert!(spec("*") < spec("com.*"));
        assert!(spec("com.*") < spec("com.example.*"));
        assert!(spec("com.example.*") < spec("com.example.agent"));
    }

    /// Path keys: absolute, `~/`, or `$VAR`; anything else is refused when the file is read.
    #[test]
    fn path_keys_are_checked() {
        for good in ["/a", "~/x", "~", "$TMPDIR/x"] {
            assert!(
                toml::from_str::<SandboxTable>(&format!("[filesystem]\n\"{good}\" = \"write\"")).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "[filesystem]\n\"rel/x\" = \"write\"",
            "[filesystem]\n\"~/**/x\" = \"deny\"",
            "[filesystem]\n\"/a\" = \"allow\"",
            "[network.unix_socket_bindings]\n\"x\" = \"allow\"",
            "[network.unix_socket_connections]\n\"/s\" = \"write\"",
        ] {
            assert!(toml::from_str::<SandboxTable>(bad).is_err(), "{bad}");
        }
    }

    /// A preset is its name, or a table with its name and its own options; `git` and `apps` became booleans.
    #[test]
    fn presets_are_names_or_tables_with_options() {
        for preset in Preset::ALL {
            let parsed = table(&format!("presets = [\"{}\"]", preset.name()));
            assert_eq!(parsed.presets, [preset]);
            assert_eq!(toml::Value::from(preset), toml::Value::String(preset.name().into()));
        }
        let claude = |login| Preset::PiClaudeCode(ClaudeCodeOptions { login });
        for (text, login) in [("token", ClaudeLogin::Token), ("keychain", ClaudeLogin::Keychain)] {
            let parsed = table(&format!(
                "presets = [{{ name = \"pi-claude-code\", login = \"{text}\" }}]"
            ));
            assert_eq!(parsed.presets, [claude(Some(login))]);
        }
        assert_eq!(ClaudeCodeOptions::default().login(), ClaudeLogin::Token);
        for bad in [
            "presets = [\"git\"]",
            "presets = [\"apps\"]",
            "presets = [\"python\"]",
            "presets = [{ name = \"rust\", login = \"token\" }]",
            "presets = [{ name = \"pi-claude-code\", login = \"oauth\" }]",
            "presets = [1]",
        ] {
            assert!(toml::from_str::<SandboxTable>(bad).is_err(), "{bad}");
        }
    }

    /// One preset entry per name: a nearer level's options win one by one.
    #[test]
    fn preset_options_merge_per_option_and_name_their_file() {
        let claude = |login| Preset::PiClaudeCode(ClaudeCodeOptions { login });
        let mut merged = table("presets = [{ name = \"pi-claude-code\", login = \"keychain\" }]");
        merged.merge(table("presets = [\"pi-claude-code\", \"rust\"]"));
        assert_eq!(merged.presets, [claude(Some(ClaudeLogin::Keychain)), Preset::Rust]);
        let (global, local) = (Path::new("/g/config.toml"), Path::new("/r/.piton/config.local.toml"));
        let mut s = SandboxSettings::default();
        s.add(table("presets = [\"pi-claude-code\"]"), global, Origin::UsersOwn);
        s.add(
            table("presets = [{ name = \"pi-claude-code\", login = \"keychain\" }]"),
            local,
            Origin::UsersOwn,
        );
        assert_eq!(s.presets[0].file, local, "the file that set the login");
    }

    /// `[[sandbox.commands]]`: a `bash(...)` rule, a required `runtime`, and for a sandbox entry `all` or a table in
    /// `[sandbox]`'s shape.
    #[test]
    fn command_grants_parse() {
        let parsed = table(
            "[[commands]]\nmatch = \"bash(husky install *)\"\nruntime = \"sandbox\"\ngrant = \"all\"\n\n[[commands]]\nmatch \
             = \"bash(git push *)\"\nruntime = \"sandbox\"\ngrant = { network = { unix_socket_connections = { \
             \"$SSH_AUTH_SOCK\" = \"allow\" } }, filesystem = { \"~/x\" = \"write\" } }\n\n[[commands]]\nmatch = \
             \"bash(log show *)\"\nruntime = \"host\"\n",
        );
        assert_eq!(parsed.commands[0].pattern.pattern(), "husky install *");
        assert_eq!(
            parsed.commands[0].runtime,
            CommandRuntime::Sandbox(Grant::All(AllGrant::All))
        );
        let CommandRuntime::Sandbox(Grant::Entries(entries)) = &parsed.commands[1].runtime else {
            panic!("{:?}", parsed.commands[1].runtime)
        };
        assert_eq!(entries.filesystem[&path("~/x")], FileAccess::Write);
        assert_eq!(
            entries.network.unix_socket_connections[&path("$SSH_AUTH_SOCK")],
            Effect::Allow
        );
        assert_eq!(parsed.commands[2].runtime, CommandRuntime::Host);
        let text = toml::to_string(&parsed).unwrap();
        assert_eq!(toml::from_str::<SandboxTable>(&text).unwrap(), parsed, "round trip");
        for (bad, says) in [
            ("[[commands]]\nmatch = \"git push *\"\nruntime = \"host\"", "bash(...)"),
            ("[[commands]]\nmatch = \"bash(x)\"", "`runtime` is required"),
            (
                "[[commands]]\nmatch = \"bash(x)\"\nruntime = \"sandbox\"",
                "needs `grant`",
            ),
            (
                "[[commands]]\nmatch = \"bash(x)\"\nruntime = \"host\"\ngrant = \"all\"",
                "`grant` is for",
            ),
            (
                "[[commands]]\nmatch = \"bash(x)\"\nruntime = \"sandbox\"\ngrant = { write = [\"/a\"] }",
                "",
            ),
            (
                "[[commands]]\nmatch = \"bash(x)\"\nruntime = \"sandbox\"\ngrant = { profile = \"yolo\" }",
                "",
            ),
        ] {
            let err = toml::from_str::<SandboxTable>(bad).expect_err(bad).to_string();
            assert!(err.contains(says), "{bad}: {err}");
        }
    }

    /// Every entry that opens is a grant, one stable line each; what only closes isn't.
    #[test]
    fn grants_are_what_opens() {
        let grants = |text: &str| {
            table(text)
                .grants()
                .into_iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(grants("profile = \"yolo\""), ["sandbox.profile = \"yolo\""]);
        assert_eq!(
            grants("allow_tcp_listening = true"),
            ["sandbox.allow_tcp_listening = true"]
        );
        assert_eq!(
            grants("[filesystem]\n\"~/.ssh/config\" = \"read\"\n\"/a\" = \"write\""),
            [
                "sandbox.filesystem.\"/a\" = \"write\"",
                "sandbox.filesystem.\"~/.ssh/config\" = \"read\""
            ]
        );
        assert_eq!(
            grants("[network.connections]\n\"*\" = \"allow\""),
            ["sandbox.network.connections.\"*\" = \"allow\""]
        );
        assert_eq!(
            grants("presets = [\"rust\", { name = \"pi-claude-code\", login = \"keychain\" }]"),
            [
                "sandbox.presets \"rust\"",
                "sandbox.presets { login = \"keychain\", name = \"pi-claude-code\" }"
            ]
        );
        assert_eq!(
            grants("[[commands]]\nmatch = \"bash(x)\"\nruntime = \"host\""),
            ["sandbox.commands { match = \"bash(x)\", runtime = \"host\" }"]
        );
        for text in [
            "allow_tcp_listening = false",
            "allow_git_control_files = false",
            "[filesystem]\n\"/b\" = \"deny\"",
            "[network.connections]\n\"*\" = \"deny\"\n\"evil.com\" = \"deny\"",
            "[network.unix_socket_connections]\n\"/\" = \"deny\"",
            "[macos.services]\n\"*\" = \"deny\"",
            "silence = [{ target = \"com.apple.SecurityServer\" }]",
        ] {
            assert!(table(text).grants().is_empty(), "{text}");
        }
    }

    /// A filter keeps the grants it names and every restriction.
    #[test]
    fn filtering_keeps_restrictions() {
        let all = table(
            "profile = \"yolo\"\npresets = [\"rust\", \"swift\"]\nallow_tcp_listening = true\n\
             allow_git_control_files = false\n[filesystem]\n\"/a\" = \"write\"\n\"/b\" = \"deny\"\n\
             [network.connections]\n\"*\" = \"deny\"\n\"github.com\" = \"allow\"\n\
             [[commands]]\nmatch = \"bash(x)\"\nruntime = \"host\"",
        );
        let (none, out) = all.clone().filtered(&GrantFilter::None);
        assert_eq!(out.len(), all.grants().len());
        assert_eq!(none.profile, None);
        assert!(none.presets.is_empty() && none.commands.is_empty());
        assert_eq!(none.allow_tcp_listening, None);
        assert_eq!(none.allow_git_control_files, Some(false), "a false switch closes");
        assert_eq!(none.filesystem.len(), 1);
        assert_eq!(none.filesystem[&path("/b")], FileAccess::Deny);
        assert_eq!(none.network.connections.len(), 1);
        assert_eq!(none.network.connections[&NetworkTarget::Anywhere], Effect::Deny);
        let rust = GrantEntry::item("sandbox.presets", "rust");
        let (only, _) = all.clone().filtered(&GrantFilter::Only([rust].into()));
        assert_eq!(only.presets, [Preset::Rust]);
        assert_eq!(all.clone().filtered(&GrantFilter::All).0, all);
    }

    /// `silence` names a Mach service or `tcp-listen`; a path or host is refused, saying to use a `"deny"`.
    #[test]
    fn silence_is_for_services_and_listening() {
        let parse = |text: &str| toml::from_str::<SandboxTable>(text).map(|t| t.silence);
        assert_eq!(
            parse("silence = [{ process = \"claude\", target = \"com.apple.SecurityServer\" }]").unwrap(),
            [Silence {
                target: SilenceTarget::Service("com.apple.SecurityServer".into()),
                process: Some("claude".into()),
            }]
        );
        assert_eq!(
            parse("silence = [{ target = \"tcp-listen\" }]").unwrap()[0].target,
            SilenceTarget::TcpListen
        );
        for bad in [
            "silence = [{ target = \"~/x\" }]",
            "silence = [{ target = \"/a\" }]",
            "silence = [{ target = \"example.com:443\" }]",
            "silence = [{ target = \"com.example.*\" }]",
            "silence = [\"x\"]",
        ] {
            let err = parse(bad).expect_err(bad).to_string();
            assert!(!err.is_empty(), "{bad}");
        }
        let entry = Silence {
            target: SilenceTarget::Service("a.b".into()),
            process: Some("x".into()),
        };
        assert_eq!(parse(&format!("silence = [{}]", entry.inline())).unwrap(), [entry]);
    }

    /// The earlier lists, and keys from before them, are unknown keys like any typo.
    #[test]
    fn earlier_keys_are_unknown() {
        for text in [
            "write = [\"/a\"]",
            "write_file = [\"/a\"]",
            "read = [\"~/.ssh/config\"]",
            "connect = [\"*\"]",
            "unix_bind = [\"/\"]",
            "tcp_listen = \"*\"",
            "services = [\"*\"]",
            "deny = [\"/a\"]",
            "allow = [\"/a\"]",
            "[network]\nopen = true",
            "[macos]\napps = true",
        ] {
            assert!(toml::from_str::<SandboxTable>(text).is_err(), "{text}");
        }
    }
}
