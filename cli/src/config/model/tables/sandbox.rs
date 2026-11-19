//! `[sandbox]`: what a sandboxed pi may do beyond Piton's built-in policy (`launch/sandbox/`). Lists only add across
//! levels.
//!
//! A knob that loosens a protection (`expose`, the `git` preset, the `mise` preset's `install = "allow"`) is accepted
//! in the global `config.toml` or a `config.local.toml`, never a project's committed `config.toml`: a repository can't
//! open the sandbox for whoever clones it. Knobs are on or off; asking when the sandbox blocks something is #241's.
//!
//! Keys of the nono-CLI backend and the network keys are refused with a message that says what replaces them.

use serde::{Deserialize, Serialize};

use super::{Merge, Rule, TableValue, is_grant_path, present, union};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxTable {
    /// Writable folders, beyond the workspace and the built-in write roots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Writable files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_file: Vec<String>,
    /// Paths denied like the built-in credential list: no read, write, or connect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Named permissions beyond the default: toolchain caches, and `git`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    /// Paths whose built-in deny is lifted. Readable then; writable only through `allow` or `allow_file`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<String>,

    // Removed keys, read only to say what replaces them (see `check`).
    #[serde(default, skip_serializing)]
    base: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    groups: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    bypass: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    read: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    read_file: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    unix_socket: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    network: Option<toml::Value>,
}

/// A named permission beyond the default. Written as its name, or as a table with the name and the preset's own
/// options: `{ name = "mise", install = "allow" }`.
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

    /// Folds a nearer level's entry for the same preset in: per option, the nearer value when it sets one.
    fn merge(&mut self, nearer: Preset) {
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

/// Where network filtering is planned; users who need a network key say so there.
const NETWORK_ISSUE: &str = "https://github.com/alex35mil/Piton/issues/228";

impl SandboxTable {
    /// The first removed key the table sets, with what to do instead.
    fn removed_key(&self) -> Option<String> {
        let presets = "Piton has a built-in default; add toolchain grants with `presets` (mise, rust, node, swift)";
        let readable = "everything is readable by default except the built-in denies; use `expose` in the global \
                        config.toml or a config.local.toml for a denied path";
        let removed = |key: &str, why: &str| format!("sandbox.{key} was removed: {why}");
        if self.base.is_some() {
            return Some(removed("base", presets));
        }
        if self.groups.is_some() {
            return Some(removed("groups", presets));
        }
        if self.bypass.is_some() {
            return Some(removed(
                "bypass",
                "use `expose` in the global config.toml or a config.local.toml to lift a built-in deny for a path",
            ));
        }
        if self.read.is_some() {
            return Some(removed("read", readable));
        }
        if self.read_file.is_some() {
            return Some(removed("read_file", readable));
        }
        if self.unix_socket.is_some() {
            return Some(format!(
                "sandbox.unix_socket isn't supported yet: the sandbox network is open, so pi already reaches every \
                 Unix socket. Restricting it is planned; if you need this, say so in {NETWORK_ISSUE}"
            ));
        }
        let network = self.network.as_ref()?;
        let set = |key: &str| network.as_table().is_some_and(|t| t.contains_key(key));
        Some(match (set("open_port"), set("profile")) {
            (true, _) => format!(
                "sandbox.network.open_port isn't supported yet: the sandbox network is open, so pi already reaches \
                 every localhost port. Restricting it is planned; if you need this, say so in {NETWORK_ISSUE}"
            ),
            (false, true) => format!(
                "sandbox.network.profile isn't supported yet: the sandbox network is open. Network filtering is \
                 planned; if you need it, say so in {NETWORK_ISSUE}"
            ),
            (false, false) => format!(
                "[sandbox.network] isn't supported yet: the sandbox network is open. Network filtering is planned; \
                 if you need it, say so in {NETWORK_ISSUE}"
            ),
        })
    }
}

impl TableValue for SandboxTable {
    /// Every knob that loosens a protection: the global file or a `config.local.toml`, never a project's
    /// `config.toml`.
    fn ruled_keys(&self) -> Vec<(&'static str, Rule)> {
        let mut keys = present(Rule::USERS_OWN, &[("sandbox.expose", !self.expose.is_empty())]);
        for key in self.presets.iter().filter_map(Preset::loosening) {
            if !keys.iter().any(|(k, _)| *k == key) {
                keys.push((key, Rule::USERS_OWN));
            }
        }
        keys
    }

    fn check(&self) -> Result<(), String> {
        if let Some(message) = self.removed_key() {
            return Err(message);
        }
        for (key, paths) in [
            ("allow", &self.allow),
            ("allow_file", &self.allow_file),
            ("deny", &self.deny),
            ("expose", &self.expose),
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
    /// Lists only add; a nearer level can't remove a grant or a deny.
    fn merge(&mut self, n: Self) {
        union(&mut self.allow, n.allow);
        union(&mut self.allow_file, n.allow_file);
        union(&mut self.deny, n.deny);
        for preset in n.presets {
            match self.presets.iter_mut().find(|p| p.name() == preset.name()) {
                Some(existing) => existing.merge(preset),
                None => self.presets.push(preset),
            }
        }
        union(&mut self.expose, n.expose);
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
        let mut base =
            table("allow = [\"/a\"]\ndeny = [\"/secret\"]\npresets = [\"rust\"]\nexpose = [\"~/.ssh/config\"]\n");
        base.merge(table("allow = [\"/a\", \"/b\"]\npresets = [\"swift\", \"rust\"]\n"));
        assert_eq!(base.allow, ["/a", "/b"]);
        assert_eq!(base.deny, ["/secret"], "a nearer level can't drop a deny");
        assert_eq!(base.presets, [Preset::Rust, Preset::Swift]);
        assert_eq!(base.expose, ["~/.ssh/config"]);
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

    /// Every knob that loosens a protection is the user's own: the global file or a `config.local.toml`. Toolchains,
    /// and installs that ask or are denied, may be listed anywhere.
    #[test]
    fn loosening_knobs_are_the_users_own() {
        assert_eq!(
            table("presets = [{ name = \"mise\", install = \"allow\" }]").ruled_keys(),
            [("sandbox.presets mise install = \"allow\"", Rule::USERS_OWN)]
        );
        assert_eq!(
            table("presets = [\"git\", \"rust\"]").ruled_keys(),
            [("sandbox.presets git", Rule::USERS_OWN)]
        );
        assert_eq!(
            table("expose = [\"~/.ssh/config\"]").ruled_keys(),
            [("sandbox.expose", Rule::USERS_OWN)]
        );
        for text in [
            "presets = [\"mise\"]",
            "presets = [{ name = \"mise\", install = \"deny\" }]",
            "presets = [\"rust\", \"swift\"]",
            "allow = [\"/a\"]",
        ] {
            assert!(table(text).ruled_keys().is_empty(), "{text}");
        }
        assert!(
            toml::from_str::<SandboxTable>("git = \"unprotected\"").is_err(),
            "the key is a preset"
        );
    }

    #[test]
    fn paths_are_checked() {
        assert!(table("allow = [\"relative/path\"]").check().is_err());
        assert!(table("expose = [\"x\"]").check().is_err());
        assert!(table("allow = [\"~/x\", \"/abs\", \"$TMPDIR/x\"]").check().is_ok());
        assert!(toml::from_str::<SandboxTable>("presets = [\"python\"]").is_err());
    }

    /// Each removed key fails the table with what replaces it; network keys point at the filtering issue.
    #[test]
    fn removed_keys_say_what_replaces_them() {
        let err = |text: &str| table(text).check().unwrap_err();
        assert!(err("base = \"mise-dev\"").starts_with("sandbox.base was removed: "));
        assert!(err("groups = [\"git_config\"]").contains("`presets`"));
        assert!(err("bypass = [\"~/x\"]").contains("`expose`"));
        assert!(err("read = [\"~/x\"]").contains("readable by default"));
        assert!(err("read_file = [\"~/x\"]").contains("readable by default"));
        for text in [
            "unix_socket = [\"$SSH_AUTH_SOCK\"]",
            "[network]\nopen_port = [0]",
            "[network]\nprofile = false",
            "[network]\n",
        ] {
            let message = err(text);
            assert!(message.contains("isn't supported yet"), "{message}");
            assert!(message.ends_with(&format!("say so in {NETWORK_ISSUE}")), "{message}");
        }
        assert!(err("[network]\nopen_port = [0]").starts_with("sandbox.network.open_port "));
        assert!(err("[network]\nprofile = false").starts_with("sandbox.network.profile "));
    }

    #[test]
    fn removed_keys_are_never_written_back() {
        let value = toml::Value::try_from(table("allow = [\"/a\"]")).unwrap();
        assert_eq!(value.as_table().unwrap().keys().collect::<Vec<_>>(), ["allow"]);
    }
}
