//! `[sandbox]`: what a sandboxed pi may do beyond Piton's base policy (`launch/sandbox/`). A profile adds a fixed set of
//! entries; the user's lists add on top, and lists only add across levels.
//!
//! A key that loosens a protection (`profile`, `read`, `connect`, `bind`, `services`, the `git`, `apps`, and
//! `pi-claude-code` presets, `commands`) is accepted in the global `config.toml` or a `config.local.toml`, never a
//! project's committed `config.toml`: a repository can't open the sandbox for whoever clones it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Merge, Rule, TableValue, is_grant_path, present, union};

/// One file's `[sandbox]`, as written.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxTable {
    /// Which built-in set of entries the sandbox starts from. Unset: the nearer level's, else [`Profile::Standard`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// Writable folders: each folder and everything below it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
    /// Writable files: exactly each path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_file: Vec<String>,
    /// Paths whose built-in credential deny is lifted for reading: readable then; writable only through `write` or
    /// `write_file`, connectable only through `connect`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    /// Sockets, ports, and hosts the sandbox may connect to. A socket entry at or inside a credential folder opens just
    /// that path, for connecting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<Connect>,
    /// Paths where processes may create Unix sockets (`bind`): each path and everything below it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bind: Vec<String>,
    /// Mach services the sandbox may look up beyond the built-in list: a name, a `name.*` prefix, or `*` for every
    /// service. The keychain's stay closed unless `read` opens the keychain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceEntry>,
    /// Named bundles: toolchain caches, `git`, `apps`, and `pi-claude-code`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    /// Paths denied like the built-in credential list: no read, write, or connect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Blocks of processes that stay blocked without asking or a notice, each described by what its notice showed:
    /// "Deny" saved from the block question, or an entry a notice names. Changes nothing in the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub silence: Vec<Silence>,
    /// Per-command grants: a bash call that matches runs through the daemon with the launch's sandbox widened by the
    /// grant (`[[sandbox.commands]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandGrant>,
}

/// One `silence` entry: `{ target = "...", process = "..." }`. It matches what a notice shows (the process and its
/// target), never why the block happened.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Silence {
    pub target: SilenceTarget,
    /// Only blocks of processes with this name (`git`, `claude`). Unset: every process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// What a `silence` entry's `target` names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum SilenceTarget {
    /// `/...`, `~/...`, or `$VAR/...`: the path and everything below it. A `*` segment matches one folder entry, a
    /// `**` segment any number of them.
    Path(String),
    /// A Mach service (`com.apple.SecurityServer`), matched exactly against a lookup of it.
    Service(String),
}

impl TryFrom<String> for SilenceTarget {
    type Error = String;

    fn try_from(target: String) -> Result<Self, String> {
        if is_grant_path(&target) {
            return Ok(SilenceTarget::Path(target));
        }
        match ServiceName::valid_text(&target) && !target.contains('*') {
            true => Ok(SilenceTarget::Service(target)),
            false => Err(format!(
                "silence target {target:?}: a path (absolute, or starting with `~/` or `$`; `*` and `**` segments \
                 match folders) or a Mach service name (`com.apple.SecurityServer`)"
            )),
        }
    }
}

impl From<SilenceTarget> for String {
    fn from(target: SilenceTarget) -> String {
        match target {
            SilenceTarget::Path(path) | SilenceTarget::Service(path) => path,
        }
    }
}

impl Silence {
    /// The entry as a TOML inline table, as a notice names it and the block question saves it.
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

/// A `services` entry: every Mach service, or a name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum ServiceEntry {
    /// `*`: every service but the keychain's.
    Every,
    Name(ServiceName),
}

/// A Mach service's global name, or the start of one. Real names aren't all reverse-DNS
/// (`com.apple.distributed_notifications@Uv3`, an app's plain-word port), so a name is checked only for what a Seatbelt
/// string and a lookup need: not empty, and no quote, backslash, slash, whitespace, control character, or `*` other
/// than a trailing `.*`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

impl TryFrom<String> for ServiceEntry {
    type Error = String;

    fn try_from(entry: String) -> Result<Self, String> {
        match entry.as_str() {
            "*" => Ok(ServiceEntry::Every),
            text => ServiceName::parse(text).map(ServiceEntry::Name),
        }
    }
}

impl From<ServiceEntry> for String {
    fn from(entry: ServiceEntry) -> String {
        match entry {
            ServiceEntry::Every => "*".into(),
            ServiceEntry::Name(name) => name.to_string(),
        }
    }
}

/// `[[sandbox.commands]]`: what a matching command gets.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandGrant {
    /// A `bash(...)` permission rule, matched by the daemon as the permissions extension matches it.
    #[serde(rename = "match")]
    pub pattern: CommandPattern,
    pub grant: Grant,
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
/// configuration levels, the daemon's state, the user's `deny`, and credentials while fnox is on (`launch/sandbox/`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Grant {
    /// `"all"`: everything except the floor. What "Allow and persist" saves.
    All(AllGrant),
    /// The `[sandbox]` grant lists, added as the user's own lists are.
    Lists(GrantLists),
}

/// The word `all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AllGrant {
    All,
}

/// A narrowed grant: the `[sandbox]` lists that widen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantLists {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_file: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<Connect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bind: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
}

/// A built-in set of entries the sandbox starts from. Profiles only add: `strict` adds nothing to the base, and a
/// profile's entry is removed only by choosing another profile. The entries are in `launch/sandbox/policy.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// The base alone.
    Strict,
    /// What most users need to try Piton without configuring anything.
    #[default]
    Standard,
    /// Standard and more.
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

/// A `connect` entry: what the sandbox may connect to, by intent. Piton enforces each the way it can: sockets and ports
/// in the sandbox, named hosts through its proxy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub enum Connect {
    /// Unix sockets at and below a path, as written (`/`, `~/…`, or `$VAR/…`); `/` is every socket.
    Socket(String),
    /// A network destination.
    Network(NetworkTarget),
}

/// A network destination a `connect` entry names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkTarget {
    /// `localhost:PORT`: a port on this Mac.
    LocalPort { port: u16 },
    /// `localhost:*`: every port on this Mac, every local service included.
    LocalAnyPort,
    /// `*:PORT`: a port on any host.
    AnyHostPort { port: u16 },
    /// `*`: any host, any port.
    Anywhere,
    /// A named host or IP address (`github.com`, `*.npmjs.org`, `10.0.0.5`), on one port or every port.
    Host { host: String, port: Option<u16> },
}

impl TryFrom<String> for Connect {
    type Error = String;

    fn try_from(entry: String) -> Result<Self, String> {
        let bad = |why: &str| format!("connect {entry:?}: {why}");
        if entry.starts_with('/') || entry.starts_with('~') || entry.starts_with('$') {
            return match is_grant_path(&entry) {
                true => Ok(Connect::Socket(entry)),
                false => Err(bad("a socket path is absolute, or starts with `~/` or `$`")),
            };
        }
        if entry == "*" {
            return Ok(Connect::Network(NetworkTarget::Anywhere));
        }
        if entry == "localhost:*" {
            return Ok(Connect::Network(NetworkTarget::LocalAnyPort));
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
        let target = match (host, port) {
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
                        "expected a socket path, `localhost:PORT`, `*:PORT`, `*`, or a host (`github.com`, \
                         `*.npmjs.org`, `10.0.0.5`) with an optional `:PORT`",
                    ));
                }
                NetworkTarget::Host {
                    host: host.to_ascii_lowercase(),
                    port,
                }
            }
        };
        Ok(Connect::Network(target))
    }
}

impl From<Connect> for String {
    fn from(connect: Connect) -> String {
        match connect {
            Connect::Socket(path) => path,
            Connect::Network(target) => target.to_string(),
        }
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

/// A named bundle of grants. Written as its name, or as a table with the name and the preset's own options:
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
    /// Git's hooks and configuration (`.git/hooks/`, `.git/config`, `.git/commondir`, replacing `.git`) become
    /// writable, so `husky install` and `git remote add` work in the sandbox, and the next Git command on the host may
    /// run an agent's code (`agent/git.rs`). Hooks run in the sandbox either way.
    Git,
    /// Launching apps and sending Apple Events: `open`, `osascript`, and an app's own launches through Launch Services.
    /// What macOS starts this way runs outside the sandbox, so the agent can run what it wrote on the host.
    Apps,
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
    pub const ALL: [Preset; 7] = [
        Preset::Mise,
        Preset::Rust,
        Preset::Node,
        Preset::Swift,
        Preset::Git,
        Preset::Apps,
        Preset::PiClaudeCode(ClaudeCodeOptions { login: None }),
    ];

    /// Every preset's name, as written.
    pub const NAMES: [&'static str; 7] = ["mise", "rust", "node", "swift", "git", "apps", "pi-claude-code"];

    pub fn name(&self) -> &'static str {
        match self {
            Preset::Mise => "mise",
            Preset::Rust => "rust",
            Preset::Node => "node",
            Preset::Swift => "swift",
            Preset::Git => "git",
            Preset::Apps => "apps",
            Preset::PiClaudeCode(_) => "pi-claude-code",
        }
    }

    /// Whether this entry sets any of its preset's options.
    pub fn has_options(&self) -> bool {
        match self {
            Preset::PiClaudeCode(options) => options.login.is_some(),
            Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift | Preset::Git | Preset::Apps => false,
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
            (
                Preset::Mise
                | Preset::Rust
                | Preset::Node
                | Preset::Swift
                | Preset::Git
                | Preset::Apps
                | Preset::PiClaudeCode(_),
                _,
            ) => {}
        }
    }

    /// The key of a preset that loosens a protection (it lets the sandbox write what the host runs, or opens what it
    /// otherwise closes), or `None` for one any configuration file may list.
    fn loosening(&self) -> Option<&'static str> {
        match self {
            Preset::Git => Some("sandbox.presets git"),
            Preset::Apps => Some("sandbox.presets apps"),
            Preset::PiClaudeCode(_) => Some("sandbox.presets pi-claude-code"),
            Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift => None,
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
            "git" => Preset::Git,
            "apps" => Preset::Apps,
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
            | Preset::Swift
            | Preset::Git
            | Preset::Apps => {}
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
    /// Every key that loosens a protection: the global file or a `config.local.toml`, never a project's `config.toml`.
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        let mut keys = present(
            Rule::USERS_OWN,
            &[
                ("sandbox.profile", self.profile.is_some()),
                ("sandbox.read", !self.read.is_empty()),
                ("sandbox.connect", !self.connect.is_empty()),
                ("sandbox.bind", !self.bind.is_empty()),
                ("sandbox.services", !self.services.is_empty()),
                ("sandbox.commands", !self.commands.is_empty()),
            ],
        );
        for key in self.presets.iter().filter_map(Preset::loosening) {
            if !keys.iter().any(|(k, _)| *k == key) {
                keys.push((key, Rule::USERS_OWN));
            }
        }
        keys
    }

    fn check(&self) -> Result<(), String> {
        for (key, paths) in [
            ("write", &self.write),
            ("write_file", &self.write_file),
            ("read", &self.read),
            ("bind", &self.bind),
            ("deny", &self.deny),
        ] {
            if let Some(grant) = paths.iter().find(|grant| !is_grant_path(grant)) {
                return Err(format!(
                    "sandbox.{key} {grant:?}: must be absolute, or start with `~/` or `$`"
                ));
            }
        }
        for command in &self.commands {
            match &command.grant {
                Grant::All(AllGrant::All) => {}
                Grant::Lists(lists) => lists
                    .check()
                    .map_err(|why| format!("sandbox.commands {}: {why}", String::from(command.pattern.clone())))?,
            }
        }
        Ok(())
    }
}

impl GrantLists {
    fn check(&self) -> Result<(), String> {
        for (key, paths) in [
            ("write", &self.write),
            ("write_file", &self.write_file),
            ("read", &self.read),
            ("bind", &self.bind),
        ] {
            if let Some(grant) = paths.iter().find(|grant| !is_grant_path(grant)) {
                return Err(format!(
                    "grant {key} {grant:?}: must be absolute, or start with `~/` or `$`"
                ));
            }
        }
        Ok(())
    }
}

impl Merge for SandboxTable {
    /// Lists only add; a nearer level can't remove a grant or a deny. The nearest `profile` wins.
    fn merge(&mut self, n: Self) {
        if n.profile.is_some() {
            self.profile = n.profile;
        }
        union(&mut self.write, n.write);
        union(&mut self.write_file, n.write_file);
        union(&mut self.read, n.read);
        union(&mut self.connect, n.connect);
        union(&mut self.bind, n.bind);
        union(&mut self.services, n.services);
        for preset in n.presets {
            match self.presets.iter_mut().find(|p| p.name() == preset.name()) {
                Some(existing) => existing.merge(&preset),
                None => self.presets.push(preset),
            }
        }
        union(&mut self.deny, n.deny);
        union(&mut self.silence, n.silence);
        union(&mut self.commands, n.commands);
    }
}

/// One entry of a list, with the file that listed it first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed<T> {
    pub value: T,
    pub file: PathBuf,
}

/// The `[sandbox]` of a launch's whole chain, every entry with the file it came from: what `piton sandbox show` and
/// `why` name. The profile's own entries are added by the sandbox (`launch/sandbox/`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxSettings {
    /// The nearest file's `profile`; `None` when no file sets one.
    pub profile: Option<Listed<Profile>>,
    pub write: Vec<Listed<String>>,
    pub write_file: Vec<Listed<String>>,
    pub read: Vec<Listed<String>>,
    pub connect: Vec<Listed<Connect>>,
    pub bind: Vec<Listed<String>>,
    pub services: Vec<Listed<ServiceEntry>>,
    pub presets: Vec<Listed<Preset>>,
    pub deny: Vec<Listed<String>>,
    pub silence: Vec<Listed<Silence>>,
    /// Per-command grants, farthest file first; the first that matches a call decides.
    pub commands: Vec<Listed<CommandGrant>>,
}

impl SandboxSettings {
    /// Folds in one file's table, files farthest first.
    pub fn add(&mut self, table: SandboxTable, file: &Path) {
        let SandboxTable {
            profile,
            write,
            write_file,
            read,
            connect,
            bind,
            services,
            presets,
            deny,
            silence,
            commands,
        } = table;
        if let Some(profile) = profile {
            self.profile = Some(Listed {
                value: profile,
                file: file.to_path_buf(),
            });
        }
        add_listed(&mut self.write, write, file);
        add_listed(&mut self.write_file, write_file, file);
        add_listed(&mut self.read, read, file);
        add_listed(&mut self.connect, connect, file);
        add_listed(&mut self.bind, bind, file);
        add_listed(&mut self.services, services, file);
        // A preset is listed once, with the file that decided it: the nearest that sets one of its options, else the
        // first that lists it.
        for preset in presets {
            match self.presets.iter_mut().find(|p| p.value.name() == preset.name()) {
                Some(existing) => {
                    existing.value.merge(&preset);
                    if preset.has_options() {
                        existing.file = file.to_path_buf();
                    }
                }
                None => self.presets.push(Listed {
                    value: preset,
                    file: file.to_path_buf(),
                }),
            }
        }
        add_listed(&mut self.deny, deny, file);
        add_listed(&mut self.silence, silence, file);
        add_listed(&mut self.commands, commands, file);
    }

    /// The profile in effect: the nearest file's, else the default.
    pub fn profile(&self) -> Profile {
        self.profile.as_ref().map(|p| p.value).unwrap_or_default()
    }
}

fn add_listed<T: PartialEq>(list: &mut Vec<Listed<T>>, values: Vec<T>, file: &Path) {
    for value in values {
        if !list.iter().any(|l| l.value == value) {
            list.push(Listed {
                value,
                file: file.to_path_buf(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> SandboxTable {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn lists_only_add() {
        let mut base = table(
            "write = [\"/a\"]\ndeny = [\"/secret\"]\npresets = [\"rust\"]\nread = [\"~/.ssh/config\"]\nprofile = \"strict\"\n",
        );
        base.merge(table(
            "write = [\"/a\", \"/b\"]\npresets = [\"swift\", \"rust\"]\nconnect = [\"*:22\"]\n",
        ));
        assert_eq!(base.write, ["/a", "/b"]);
        assert_eq!(base.deny, ["/secret"], "a nearer level can't drop a deny");
        assert_eq!(base.presets, [Preset::Rust, Preset::Swift]);
        assert_eq!(base.read, ["~/.ssh/config"]);
        assert_eq!(
            base.profile,
            Some(Profile::Strict),
            "a level without a profile keeps it"
        );
        base.merge(table("profile = \"yolo\""));
        assert_eq!(base.profile, Some(Profile::Yolo), "the nearest wins");
    }

    /// Each entry keeps the first file that listed it; the nearest profile wins.
    #[test]
    fn settings_keep_where_each_entry_came_from() {
        let (global, repo) = (Path::new("/g/config.toml"), Path::new("/r/.piton/config.toml"));
        let mut s = SandboxSettings::default();
        assert_eq!(s.profile(), Profile::Standard, "the default");
        s.add(
            table("profile = \"strict\"\nwrite = [\"/a\"]\npresets = [\"mise\"]"),
            global,
        );
        s.add(table("write = [\"/a\", \"/b\"]\npresets = [\"mise\"]"), repo);
        assert_eq!(s.profile(), Profile::Strict);
        assert_eq!(s.profile.as_ref().unwrap().file, global);
        let writes: Vec<_> = s.write.iter().map(|l| (l.value.as_str(), l.file.as_path())).collect();
        assert_eq!(writes, [("/a", global), ("/b", repo)]);
        assert_eq!(
            s.presets,
            [Listed {
                value: Preset::Mise,
                file: global.into()
            }]
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

    /// Every form a `connect` entry takes, and what isn't one.
    #[test]
    fn connect_entries_are_typed() {
        let parse = |text: &str| Connect::try_from(text.to_string());
        let net = |t| Ok(Connect::Network(t));
        assert_eq!(parse("$SSH_AUTH_SOCK"), Ok(Connect::Socket("$SSH_AUTH_SOCK".into())));
        assert_eq!(parse("/"), Ok(Connect::Socket("/".into())));
        assert_eq!(
            parse("~/.orbstack/run/docker.sock"),
            Ok(Connect::Socket("~/.orbstack/run/docker.sock".into()))
        );
        assert_eq!(parse("localhost:5432"), net(NetworkTarget::LocalPort { port: 5432 }));
        assert_eq!(parse("localhost:*"), net(NetworkTarget::LocalAnyPort));
        assert_eq!(
            String::from(Connect::Network(NetworkTarget::LocalAnyPort)),
            "localhost:*"
        );
        assert_eq!(parse("*:22"), net(NetworkTarget::AnyHostPort { port: 22 }));
        assert_eq!(parse("*"), net(NetworkTarget::Anywhere));
        assert_eq!(
            parse("GitHub.com:22"),
            net(NetworkTarget::Host {
                host: "github.com".into(),
                port: Some(22)
            })
        );
        assert_eq!(
            parse("*.npmjs.org"),
            net(NetworkTarget::Host {
                host: "*.npmjs.org".into(),
                port: None
            })
        );
        assert_eq!(
            parse("10.0.0.5:8080"),
            net(NetworkTarget::Host {
                host: "10.0.0.5".into(),
                port: Some(8080)
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
            "~x",
            "foo bar",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // Written back as given (hosts lowercased).
        let value = toml::Value::try_from(table("connect = [\"*:22\", \"$SSH_AUTH_SOCK\", \"Example.com\"]")).unwrap();
        assert_eq!(
            value["connect"].to_string(),
            r#"["*:22", "$SSH_AUTH_SOCK", "example.com"]"#
        );
    }

    /// A `services` entry is `*`, a name, or a `name.*` prefix; real names that aren't reverse-DNS parse too.
    #[test]
    fn service_entries_are_typed() {
        let parse = |text: &str| ServiceEntry::try_from(text.to_string());
        let exact = |n: &str| Ok(ServiceEntry::Name(ServiceName::Exact(n.into())));
        assert_eq!(parse("*"), Ok(ServiceEntry::Every));
        assert_eq!(parse("com.apple.fonts"), exact("com.apple.fonts"));
        assert_eq!(
            parse("com.apple.distributed_notifications@Uv3"),
            exact("com.apple.distributed_notifications@Uv3")
        );
        assert_eq!(parse("Hammerspoon"), exact("Hammerspoon"));
        assert_eq!(
            parse("com.example.*"),
            Ok(ServiceEntry::Name(ServiceName::Prefix("com.example.".into())))
        );
        for bad in ["", ".*", "a*b", "com.*.x", "a\"b", "a b", "a/b", "a\\b", "a\nb"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let prefix = ServiceName::Prefix("com.example.".into());
        assert!(prefix.matches("com.example.agent"));
        assert!(!prefix.matches("com.examplex"));
        assert!(ServiceName::Exact("a.b".into()).matches("a.b"));
        assert!(!ServiceName::Exact("a.b".into()).matches("a.b.c"));
        // Written back as given.
        let value = toml::Value::try_from(table("services = [\"*\", \"com.example.*\", \"x.y\"]")).unwrap();
        assert_eq!(value["services"].to_string(), r#"["*", "com.example.*", "x.y"]"#);
        // A silence entry names one service, never a prefix.
        assert!(SilenceTarget::try_from("Hammerspoon".to_string()).is_ok());
        assert!(SilenceTarget::try_from("com.example.*".to_string()).is_err());
    }

    /// A preset is its name, or a table with its name and its own options; anything else is refused, the `mise`
    /// preset's former `install` option included. Written back as its name when no option is set.
    #[test]
    fn presets_are_names_or_tables_with_options() {
        for preset in Preset::ALL {
            let parsed = table(&format!("presets = [\"{}\"]", preset.name()));
            assert_eq!(parsed.presets, [preset]);
            assert_eq!(toml::Value::from(preset), toml::Value::String(preset.name().into()));
            let named = table(&format!("presets = [{{ name = \"{}\" }}]", preset.name()));
            assert_eq!(named.presets, [preset], "a table with only the name");
        }
        let claude = |login| Preset::PiClaudeCode(ClaudeCodeOptions { login });
        for (text, login) in [("token", ClaudeLogin::Token), ("keychain", ClaudeLogin::Keychain)] {
            let parsed = table(&format!(
                "presets = [{{ name = \"pi-claude-code\", login = \"{text}\" }}]"
            ));
            assert_eq!(parsed.presets, [claude(Some(login))]);
            let back = toml::Value::from(claude(Some(login)));
            assert_eq!(back.get("login").and_then(toml::Value::as_str), Some(text));
            assert_eq!(back.get("name").and_then(toml::Value::as_str), Some("pi-claude-code"));
        }
        assert_eq!(claude(None), Preset::PiClaudeCode(ClaudeCodeOptions::default()));
        assert_eq!(
            ClaudeCodeOptions::default().login(),
            ClaudeLogin::Token,
            "the default login"
        );
        for bad in [
            "presets = [\"python\"]",
            "presets = [\"claude\"]",
            "presets = [{ name = \"mise\", install = \"ask\" }]",
            "presets = [{ name = \"rust\", login = \"token\" }]",
            "presets = [{ name = \"pi-claude-code\", login = \"oauth\" }]",
            "presets = [{ name = \"pi-claude-code\", login = true }]",
            "presets = [{ name = \"pi-claude-code\", config = \"~/x\" }]",
            "presets = [{ login = \"token\" }]",
            "presets = [1]",
        ] {
            assert!(toml::from_str::<SandboxTable>(bad).is_err(), "{bad}");
        }
    }

    /// One preset entry per name: a nearer level's options win one by one, and the preset's file is the one that
    /// decided it (the nearest that sets an option, else the first that lists it).
    #[test]
    fn preset_options_merge_per_option_and_name_their_file() {
        let claude = |login| Preset::PiClaudeCode(ClaudeCodeOptions { login });
        let mut merged = table("presets = [{ name = \"pi-claude-code\", login = \"keychain\" }]");
        merged.merge(table("presets = [\"pi-claude-code\", \"rust\"]"));
        assert_eq!(
            merged.presets,
            [claude(Some(ClaudeLogin::Keychain)), Preset::Rust],
            "a nearer entry without the option keeps it"
        );
        merged.merge(table("presets = [{ name = \"pi-claude-code\", login = \"token\" }]"));
        assert_eq!(merged.presets, [claude(Some(ClaudeLogin::Token)), Preset::Rust]);

        let (global, local) = (Path::new("/g/config.toml"), Path::new("/r/.piton/config.local.toml"));
        let mut s = SandboxSettings::default();
        s.add(table("presets = [\"pi-claude-code\"]"), global);
        s.add(table("presets = [\"pi-claude-code\"]"), local);
        assert_eq!(s.presets.len(), 1);
        assert_eq!(s.presets[0].file, global, "no option: the first file");
        s.add(
            table("presets = [{ name = \"pi-claude-code\", login = \"keychain\" }]"),
            local,
        );
        assert_eq!(
            s.presets,
            [Listed {
                value: claude(Some(ClaudeLogin::Keychain)),
                file: local.into()
            }],
            "the file that set the login"
        );
    }

    /// The `pi-claude-code` preset loosens the sandbox whatever its options: the user's own files only.
    #[test]
    fn the_claude_code_preset_is_the_users_own() {
        for text in [
            "presets = [\"pi-claude-code\"]",
            "presets = [{ name = \"pi-claude-code\", login = \"token\" }]",
            "presets = [{ name = \"pi-claude-code\", login = \"keychain\" }]",
        ] {
            assert_eq!(
                table(text).ruled_keys(),
                [("sandbox.presets pi-claude-code", Rule::USERS_OWN)],
                "{text}"
            );
        }
    }

    /// `[[sandbox.commands]]`: a `bash(...)` rule and `all` or the grant lists.
    #[test]
    fn command_grants_parse() {
        let parsed = table(
            "[[commands]]\nmatch = \"bash(husky install *)\"\ngrant = \"all\"\n\n[[commands]]\nmatch = \"bash(git push \
             *)\"\ngrant = { connect = [\"$SSH_AUTH_SOCK\"], write = [\"~/x\"] }\n",
        );
        assert_eq!(parsed.commands[0].pattern.pattern(), "husky install *");
        assert_eq!(parsed.commands[0].grant, Grant::All(AllGrant::All));
        assert_eq!(
            parsed.commands[1].grant,
            Grant::Lists(GrantLists {
                connect: vec![Connect::Socket("$SSH_AUTH_SOCK".into())],
                write: vec!["~/x".into()],
                ..GrantLists::default()
            })
        );
        assert!(parsed.check().is_ok());
        // Written back as given.
        let value = toml::Value::try_from(parsed).unwrap();
        assert_eq!(value["commands"][0]["match"].as_str(), Some("bash(husky install *)"));
        assert_eq!(value["commands"][0]["grant"].as_str(), Some("all"));
        for bad in [
            "[[commands]]\nmatch = \"git push *\"\ngrant = \"all\"",
            "[[commands]]\nmatch = \"read(*)\"\ngrant = \"all\"",
            "[[commands]]\nmatch = \"bash()\"\ngrant = \"all\"",
            "[[commands]]\nmatch = \"bash(x)\"\ngrant = \"everything\"",
            "[[commands]]\nmatch = \"bash(x)\"\ngrant = { deny = [\"/a\"] }",
            "[[commands]]\nmatch = \"bash(x)\"",
            "[[commands]]\nmatch = \"bash(x)\"\ngrant = \"all\"\nask = true",
        ] {
            assert!(toml::from_str::<SandboxTable>(bad).is_err(), "{bad}");
        }
        assert!(
            table("[[commands]]\nmatch = \"bash(x)\"\ngrant = { write = [\"rel\"] }")
                .check()
                .is_err()
        );
        // `bind` in a grant: paths, checked like `write`.
        let bind = table("[[commands]]\nmatch = \"bash(x)\"\ngrant = { bind = [\"$TMPDIR\"] }");
        assert_eq!(
            bind.commands[0].grant,
            Grant::Lists(GrantLists {
                bind: vec!["$TMPDIR".into()],
                ..GrantLists::default()
            })
        );
        assert!(bind.check().is_ok());
        assert!(
            table("[[commands]]\nmatch = \"bash(x)\"\ngrant = { bind = [\"rel\"] }")
                .check()
                .is_err()
        );
        assert!(table("bind = [\"rel\"]").check().is_err());
    }

    /// Every key that loosens a protection is the user's own: the global file or a `config.local.toml`. Writes,
    /// toolchains, and denies may be listed anywhere.
    #[test]
    fn loosening_keys_are_the_users_own() {
        assert_eq!(
            table("[[commands]]\nmatch = \"bash(x)\"\ngrant = \"all\"").ruled_keys(),
            [("sandbox.commands", Rule::USERS_OWN)]
        );
        assert_eq!(
            table("presets = [\"git\", \"rust\"]").ruled_keys(),
            [("sandbox.presets git", Rule::USERS_OWN)]
        );
        assert_eq!(
            table("presets = [\"apps\"]\nservices = [\"x.y\"]").ruled_keys(),
            [
                ("sandbox.services", Rule::USERS_OWN),
                ("sandbox.presets apps", Rule::USERS_OWN)
            ]
        );
        assert_eq!(
            table("profile = \"yolo\"\nread = [\"~/.ssh/config\"]\nconnect = [\"*\"]\nbind = [\"/\"]").ruled_keys(),
            [
                ("sandbox.profile", Rule::USERS_OWN),
                ("sandbox.read", Rule::USERS_OWN),
                ("sandbox.connect", Rule::USERS_OWN),
                ("sandbox.bind", Rule::USERS_OWN)
            ]
        );
        for text in [
            "presets = [\"mise\"]",
            "presets = [\"rust\", \"swift\"]",
            "write = [\"/a\"]",
            "write_file = [\"/a\"]",
            "deny = [\"/a\"]",
            // Changes nothing in the sandbox: any level, a project's committed `config.toml` included.
            "silence = [{ target = \"~/Library/Caches/x\" }]",
        ] {
            assert!(table(text).ruled_keys().is_empty(), "{text}");
        }
    }

    /// `silence` adds across levels like every list, and each entry keeps the file that listed it.
    #[test]
    fn silence_lists_add() {
        let path = |p: &str| Silence {
            target: SilenceTarget::Path(p.into()),
            process: None,
        };
        let mut base = table("silence = [{ target = \"/a\" }]");
        base.merge(table("silence = [{ target = \"/a\" }, { target = \"/b\" }]"));
        assert_eq!(base.silence, [path("/a"), path("/b")]);
        let mut s = SandboxSettings::default();
        s.add(
            table("silence = [{ target = \"/a\" }]"),
            Path::new("/r/.piton/config.toml"),
        );
        assert_eq!(s.silence[0].file, Path::new("/r/.piton/config.toml"));
    }

    /// An entry is always a table naming a path or a Mach service, and optionally a process; anything else is refused
    /// when the file is read, so an entry can't silently match nothing.
    #[test]
    fn silence_entries_are_tables_of_target_and_process() {
        let parse = |text: &str| toml::from_str::<SandboxTable>(text).map(|t| t.silence);
        assert_eq!(
            parse("silence = [{ process = \"git\", target = \"~/p/**/.git/config\" }]").unwrap(),
            [Silence {
                target: SilenceTarget::Path("~/p/**/.git/config".into()),
                process: Some("git".into()),
            }]
        );
        assert_eq!(
            parse("silence = [{ target = \"com.apple.SecurityServer\" }]").unwrap()[0].target,
            SilenceTarget::Service("com.apple.SecurityServer".into())
        );
        for bad in [
            "silence = [\"/a\"]",
            "silence = [{ target = \"relative/x\" }]",
            "silence = [{ target = \"**/.git/config\" }]",
            "silence = [{ target = \"/a\", why = \"x\" }]",
            "silence = [{ process = \"git\" }]",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // The inline form a notice names parses back to the same entry.
        let entry = Silence {
            target: SilenceTarget::Path("/p/**/.git/config".into()),
            process: Some("git".into()),
        };
        assert_eq!(parse(&format!("silence = [{}]", entry.inline())).unwrap(), [entry]);
    }

    #[test]
    fn paths_are_checked() {
        assert!(table("write = [\"relative/path\"]").check().is_err());
        assert!(table("read = [\"x\"]").check().is_err());
        assert!(table("write = [\"~/x\", \"/abs\", \"$TMPDIR/x\"]").check().is_ok());
    }

    /// Earlier keys are unknown keys, like any typo.
    #[test]
    fn earlier_keys_are_unknown() {
        for text in [
            "allow = [\"/a\"]",
            "allow_file = [\"/a\"]",
            "expose = [\"~/.ssh/config\"]",
            "base = \"mise-dev\"",
            "unix_socket = [\"/s\"]",
            "[network]\n",
        ] {
            assert!(toml::from_str::<SandboxTable>(text).is_err(), "{text}");
        }
    }
}
