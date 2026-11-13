//! `[sandbox]`: grants for sandboxed pis. Grant lists only add across levels; the base profile, extra groups,
//! bypasses, the nono command, and the network profile belong to the global tier.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, is_grant_path, nearer, present, union};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxTable {
    /// nono executable and leading arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// The backend's built-in base profile (nono: `default`, `mise-dev`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Extra built-in groups of the backend (nono: `git_config`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Read and write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_file: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_file: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Lifts a deny of the base profile for a path granted above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unix_socket: Vec<String>,
    #[serde(default, skip_serializing_if = "NetworkTable::is_empty")]
    pub network: NetworkTable,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTable {
    /// Localhost ports the sandbox may connect to and listen on (`0`: every port, macOS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_port: Vec<u16>,
    /// The proxy's network profile: a name, or `false` for no proxy filtering. Absent: the base profile's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<NetworkProfile>,
}

impl NetworkTable {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum NetworkProfile {
    Named(String),
    /// Only `false` is accepted: no proxy filtering.
    Off(bool),
}

impl TableValue for SandboxTable {
    fn global_only_keys(&self) -> Vec<&'static str> {
        present(&[
            ("sandbox.command", self.command.is_some()),
            ("sandbox.base", self.base.is_some()),
            ("sandbox.groups", !self.groups.is_empty()),
            ("sandbox.bypass", !self.bypass.is_empty()),
            ("sandbox.network.profile", self.network.profile.is_some()),
        ])
    }

    fn check(&self) -> Result<(), String> {
        if self.command.as_ref().is_some_and(Vec::is_empty) {
            return Err("sandbox.command must not be empty".into());
        }
        for (key, paths) in [
            ("allow", &self.allow),
            ("read", &self.read),
            ("read_file", &self.read_file),
            ("allow_file", &self.allow_file),
            ("deny", &self.deny),
            ("bypass", &self.bypass),
            ("unix_socket", &self.unix_socket),
        ] {
            if let Some(grant) = paths.iter().find(|grant| !is_grant_path(grant)) {
                return Err(format!(
                    "sandbox.{key} {grant:?}: must be absolute, or start with `~/` or `$`"
                ));
            }
        }
        match self.network.profile {
            Some(NetworkProfile::Off(true)) => {
                Err("sandbox.network.profile: a profile name, or `false` for no filtering".into())
            }
            Some(NetworkProfile::Off(false) | NetworkProfile::Named(_)) | None => Ok(()),
        }
    }
}

impl Merge for SandboxTable {
    /// Grants only add; a nearer level can't remove a grant or a deny.
    fn merge(&mut self, n: Self) {
        nearer(&mut self.command, n.command);
        nearer(&mut self.base, n.base);
        union(&mut self.groups, n.groups);
        union(&mut self.allow, n.allow);
        union(&mut self.read, n.read);
        union(&mut self.read_file, n.read_file);
        union(&mut self.allow_file, n.allow_file);
        union(&mut self.deny, n.deny);
        union(&mut self.bypass, n.bypass);
        union(&mut self.unix_socket, n.unix_socket);
        union(&mut self.network.open_port, n.network.open_port);
        nearer(&mut self.network.profile, n.network.profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_only_add() {
        let mut base: SandboxTable = toml::from_str(
            "base = \"mise-dev\"\nallow = [\"/a\"]\ndeny = [\"/secret\"]\n[network]\nopen_port = [0]\nprofile = false\n",
        )
        .unwrap();
        let nearer: SandboxTable = toml::from_str("allow = [\"/a\", \"/b\"]\nread = [\"/r\"]\n").unwrap();
        base.merge(nearer);
        assert_eq!(base.allow, ["/a", "/b"]);
        assert_eq!(base.read, ["/r"]);
        assert_eq!(base.deny, ["/secret"], "a nearer level can't drop a deny");
        assert_eq!(base.base.as_deref(), Some("mise-dev"));
        assert_eq!(base.network.open_port, [0]);
        assert_eq!(base.network.profile, Some(NetworkProfile::Off(false)));
    }

    #[test]
    fn values_are_checked() {
        let mut sandbox = SandboxTable {
            allow: vec!["relative/path".into()],
            ..SandboxTable::default()
        };
        assert!(sandbox.check().is_err());
        sandbox.allow = vec!["~/x".into(), "/abs".into(), "$SSH_AUTH_SOCK".into()];
        assert!(sandbox.check().is_ok());
        sandbox.network.profile = Some(NetworkProfile::Off(true));
        assert!(sandbox.check().is_err());
        sandbox.network.profile = Some(NetworkProfile::Off(false));
        assert!(sandbox.check().is_ok());
        let toml: SandboxTable = toml::from_str("[network]\nprofile = false\n").unwrap();
        assert_eq!(toml.network.profile, Some(NetworkProfile::Off(false)));
    }
}
