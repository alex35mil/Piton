//! `[sandbox]`: what a sandboxed pi may do beyond Piton's base policy (`launch/sandbox/`). A profile adds a fixed set of
//! entries; the user's lists add on top, and lists only add across levels.
//!
//! A key that loosens a protection (`profile`, `read`, `connect`, the `git` preset, the `mise` preset's
//! `install = "allow"`) is accepted in the global `config.toml` or a `config.local.toml`, never a project's committed
//! `config.toml`: a repository can't open the sandbox for whoever clones it.

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
    /// Paths whose built-in credential deny is lifted: readable (and connectable) then; writable only through `write`
    /// or `write_file`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    /// Sockets, ports, and hosts the sandbox may connect to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connect: Vec<Connect>,
    /// Named bundles: toolchain caches, and `git`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    /// Paths denied like the built-in credential list: no read, write, or connect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
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
            ("localhost", None) => return Err(bad("name the port: `localhost:PORT`")),
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
            NetworkTarget::AnyHostPort { port } => write!(f, "*:{port}"),
            NetworkTarget::Anywhere => write!(f, "*"),
            NetworkTarget::Host { host, port: Some(port) } => write!(f, "{host}:{port}"),
            NetworkTarget::Host { host, port: None } => write!(f, "{host}"),
        }
    }
}

/// A named bundle of grants. Written as its name, or as a table with the name and the preset's own options:
/// `{ name = "mise", install = "allow" }`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "toml::Value", into = "toml::Value")]
pub enum Preset {
    /// mise's cache and install bookkeeping; with `install = "allow"`, its installed tools too. Each toolchain preset
    /// grants that toolchain's caches and state, which host builds reuse; the folders are in `launch/sandbox/policy.rs`.
    Mise(MiseOptions),
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
}

/// The `mise` preset's options. Unset options are the nearest level's that sets them, else their default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MiseOptions {
    /// How `mise install` runs for a sandboxed agent. Default: [`InstallMode::Ask`].
    pub install: Option<InstallMode>,
}

/// How a sandboxed agent installs tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// The sandbox may write the installed tools: no prompt, and the agent can replace binaries the host runs. Global
    /// configuration only.
    Allow,
    /// Each install is approved by the user and run on the host (#241); until that exists, installs fail in the
    /// sandbox.
    Ask,
    /// Installs fail in the sandbox.
    Deny,
}

impl InstallMode {
    fn name(self) -> &'static str {
        match self {
            InstallMode::Allow => "allow",
            InstallMode::Ask => "ask",
            InstallMode::Deny => "deny",
        }
    }
}

impl Preset {
    #[cfg(test)]
    pub const ALL: [Preset; 5] = [
        Preset::Mise(MiseOptions { install: None }),
        Preset::Rust,
        Preset::Node,
        Preset::Swift,
        Preset::Git,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Preset::Mise(_) => "mise",
            Preset::Rust => "rust",
            Preset::Node => "node",
            Preset::Swift => "swift",
            Preset::Git => "git",
        }
    }

    /// Folds a nearer entry for the same preset in: per option, the nearer value when it sets one.
    pub fn merge(&mut self, nearer: &Preset) {
        match (self, nearer) {
            (Preset::Mise(options), Preset::Mise(n)) => {
                if n.install.is_some() {
                    options.install = n.install;
                }
            }
            (Preset::Mise(_) | Preset::Rust | Preset::Node | Preset::Swift | Preset::Git, _) => {}
        }
    }

    /// The key of a preset that loosens a protection (it lets the sandbox write what the host runs), or `None` for one
    /// any configuration file may list.
    fn loosening(&self) -> Option<&'static str> {
        match self {
            Preset::Mise(options) => match options.install {
                Some(InstallMode::Allow) => Some("sandbox.presets mise install = \"allow\""),
                Some(InstallMode::Ask | InstallMode::Deny) | None => None,
            },
            Preset::Git => Some("sandbox.presets git"),
            Preset::Rust | Preset::Node | Preset::Swift => None,
        }
    }
}

impl TryFrom<toml::Value> for Preset {
    type Error = String;

    fn try_from(value: toml::Value) -> Result<Self, String> {
        let names = "mise, rust, node, swift, or git";
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
            "mise" => {
                let install = match options.remove("install") {
                    None => None,
                    Some(toml::Value::String(mode)) => Some(match mode.as_str() {
                        "allow" => InstallMode::Allow,
                        "ask" => InstallMode::Ask,
                        "deny" => InstallMode::Deny,
                        other => return Err(format!("mise install {other:?}: \"allow\", \"ask\", or \"deny\"")),
                    }),
                    Some(_) => return Err("mise install: \"allow\", \"ask\", or \"deny\"".into()),
                };
                Preset::Mise(MiseOptions { install })
            }
            "rust" => Preset::Rust,
            "node" => Preset::Node,
            "swift" => Preset::Swift,
            "git" => Preset::Git,
            other => return Err(format!("unknown preset {other:?}: {names}")),
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
            Preset::Mise(MiseOptions { install: Some(mode) }) => {
                table.insert("install".into(), toml::Value::String(mode.name().into()));
            }
            Preset::Mise(MiseOptions { install: None }) | Preset::Rust | Preset::Node | Preset::Swift | Preset::Git => {
            }
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
            ("deny", &self.deny),
        ] {
            if let Some(grant) = paths.iter().find(|grant| !is_grant_path(grant)) {
                return Err(format!(
                    "sandbox.{key} {grant:?}: must be absolute, or start with `~/` or `$`"
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
        for preset in n.presets {
            match self.presets.iter_mut().find(|p| p.name() == preset.name()) {
                Some(existing) => existing.merge(&preset),
                None => self.presets.push(preset),
            }
        }
        union(&mut self.deny, n.deny);
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
    /// Each preset once, with the merged options; the file is the first that listed it.
    pub presets: Vec<Listed<Preset>>,
    pub deny: Vec<Listed<String>>,
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
            presets,
            deny,
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
        for preset in presets {
            match self.presets.iter_mut().find(|p| p.value.name() == preset.name()) {
                Some(existing) => existing.value.merge(&preset),
                None => self.presets.push(Listed {
                    value: preset,
                    file: file.to_path_buf(),
                }),
            }
        }
        add_listed(&mut self.deny, deny, file);
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
            table("profile = \"strict\"\nwrite = [\"/a\"]\npresets = [{ name = \"mise\", install = \"allow\" }]"),
            global,
        );
        s.add(
            table("write = [\"/a\", \"/b\"]\npresets = [{ name = \"mise\", install = \"deny\" }]"),
            repo,
        );
        assert_eq!(s.profile(), Profile::Strict);
        assert_eq!(s.profile.as_ref().unwrap().file, global);
        let writes: Vec<_> = s.write.iter().map(|l| (l.value.as_str(), l.file.as_path())).collect();
        assert_eq!(writes, [("/a", global), ("/b", repo)]);
        assert_eq!(
            s.presets,
            [Listed {
                value: Preset::Mise(MiseOptions {
                    install: Some(InstallMode::Deny)
                }),
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

    /// A preset is a name or a table; the same preset at several levels merges its options, the nearer set one
    /// winning.
    #[test]
    fn presets_take_options() {
        let mise = |install| Preset::Mise(MiseOptions { install });
        let mut base = table("presets = [{ name = \"mise\", install = \"allow\" }, \"rust\"]");
        assert_eq!(base.presets, [mise(Some(InstallMode::Allow)), Preset::Rust]);
        base.merge(table("presets = [\"mise\"]"));
        assert_eq!(
            base.presets[0],
            mise(Some(InstallMode::Allow)),
            "a bare name keeps the options"
        );
        base.merge(table("presets = [{ name = \"mise\", install = \"deny\" }]"));
        assert_eq!(base.presets[0], mise(Some(InstallMode::Deny)), "a nearer level narrows");
        // Written back: the name alone without options.
        let value =
            toml::Value::try_from(table("presets = [\"rust\", { name = \"mise\", install = \"ask\" }]")).unwrap();
        assert_eq!(
            value["presets"].to_string(),
            r#"["rust", { install = "ask", name = "mise" }]"#
        );
        for bad in [
            "presets = [\"python\"]",
            "presets = [{ install = \"ask\" }]",
            "presets = [{ name = \"mise\", install = true }]",
            "presets = [{ name = \"mise\", install = \"maybe\" }]",
            "presets = [{ name = \"rust\", install = \"ask\" }]",
            "presets = [1]",
        ] {
            assert!(toml::from_str::<SandboxTable>(bad).is_err(), "{bad}");
        }
    }

    /// Every key that loosens a protection is the user's own: the global file or a `config.local.toml`. Writes,
    /// toolchains, installs that ask or are denied, and denies may be listed anywhere.
    #[test]
    fn loosening_keys_are_the_users_own() {
        assert_eq!(
            table("presets = [{ name = \"mise\", install = \"allow\" }]").ruled_keys(),
            [("sandbox.presets mise install = \"allow\"", Rule::USERS_OWN)]
        );
        assert_eq!(
            table("presets = [\"git\", \"rust\"]").ruled_keys(),
            [("sandbox.presets git", Rule::USERS_OWN)]
        );
        assert_eq!(
            table("profile = \"yolo\"\nread = [\"~/.ssh/config\"]\nconnect = [\"*\"]").ruled_keys(),
            [
                ("sandbox.profile", Rule::USERS_OWN),
                ("sandbox.read", Rule::USERS_OWN),
                ("sandbox.connect", Rule::USERS_OWN)
            ]
        );
        for text in [
            "presets = [\"mise\"]",
            "presets = [{ name = \"mise\", install = \"deny\" }]",
            "presets = [\"rust\", \"swift\"]",
            "write = [\"/a\"]",
            "write_file = [\"/a\"]",
            "deny = [\"/a\"]",
        ] {
            assert!(table(text).ruled_keys().is_empty(), "{text}");
        }
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
