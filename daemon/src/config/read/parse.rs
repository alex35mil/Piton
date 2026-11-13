//! One file into its parts: `config[.local].toml` table by table, `mcp[.local].json`, and `policies.md` as text.
//!
//! A syntax error or an unknown top-level key fails the whole file; a table that doesn't match its schema, or that
//! a file at this level (or of this sharing) may not set, fails only that table.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;
use crate::config::model::level::LevelKind;
use crate::config::model::paths::{Sharing, global_tier};
use crate::config::model::tables::{
    BranchesTable, ClientTable, DevelopmentTable, HostTable, PermissionsTable, RunTable, SandboxTable, Table,
    TableValue, WebSearchTable, WorkersTable,
};

/// Where a file came from, for messages and for `piton config check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileOrigin {
    pub path: PathBuf,
    pub level: LevelKind,
}

/// `mcp[.local].json`: MCP servers stay JSON, the format other harnesses read. Server entries are passed to pi
/// unchanged; pi validates them. File order is kept (serde_json's `preserve_order`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpFile {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: serde_json::Map<String, serde_json::Value>,
    #[serde(rename = "autoEnableCodemode", default, skip_serializing_if = "Option::is_none")]
    pub auto_enable_codemode: Option<bool>,
}

/// One `config[.local].toml`, syntax-checked, its top-level keys known, ready to read table by table.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    pub origin: FileOrigin,
    pub sharing: Sharing,
    table: toml::Table,
}

impl ConfigDoc {
    /// Parses `text` as the file at `path`: TOML, and only top-level keys a part owns.
    pub fn parse(text: &str, path: &Path, level: LevelKind, sharing: Sharing) -> Result<Self, ConfigError> {
        let invalid = |reason: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            reason,
        };
        let table: toml::Table = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if let Some(key) = table.keys().find(|key| Table::of(key).is_none()) {
            return Err(invalid(format!("unknown key `{key}`")));
        }
        Ok(Self {
            origin: FileOrigin {
                path: path.to_path_buf(),
                level,
            },
            sharing,
            table,
        })
    }

    /// One part of the file, strictly, with the level's and the file's scope checked; `None` when the file doesn't
    /// set it.
    pub fn part<T: TableValue>(&self, table: Table) -> Result<Option<T>, ConfigError> {
        let path = &self.origin.path;
        let invalid = |reason: String| ConfigError::Invalid {
            path: path.clone(),
            reason,
        };
        let value = match table {
            // The run keys sit at the top of the file: the part is their subset.
            Table::Run => {
                let subset: toml::Table = self
                    .table
                    .iter()
                    .filter(|(key, _)| table.keys().contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                if subset.is_empty() {
                    return Ok(None);
                }
                // serde reads an empty array as an empty struct: a key that names a table must hold one.
                if let Some((key, _)) = subset
                    .iter()
                    .find(|(key, value)| !matches!(key.as_str(), "socket" | "log") && !value.is_table())
                {
                    return Err(invalid(format!("`{key}` must be a table")));
                }
                toml::Value::Table(subset)
            }
            Table::Sandbox
            | Table::Workers
            | Table::Host
            | Table::Client
            | Table::Development
            | Table::Permissions
            | Table::Branches
            | Table::Tracker
            | Table::Forge
            | Table::WebSearch => {
                let key = table.keys()[0];
                let Some(value) = self.table.get(key) else {
                    return Ok(None);
                };
                if !value.is_table() {
                    return Err(invalid(format!("`{key}` must be a table")));
                }
                value.clone()
            }
        };
        let value: T = value.try_into().map_err(|e: toml::de::Error| {
            invalid(match table {
                Table::Run => e.to_string(),
                Table::Sandbox
                | Table::Workers
                | Table::Host
                | Table::Client
                | Table::Development
                | Table::Permissions
                | Table::Branches
                | Table::Tracker
                | Table::Forge
                | Table::WebSearch => format!("[{}] {e}", table.keys()[0]),
            })
        })?;
        value.check().map_err(invalid)?;
        if self.origin.level != LevelKind::Global
            && let Some(key) = value.global_only_keys().first()
        {
            return Err(ConfigError::GlobalOnly {
                path: path.clone(),
                key: (*key).to_string(),
                global: global_tier(),
            });
        }
        match self.sharing {
            Sharing::Local => {}
            Sharing::Shared => {
                if let Some(key) = value.local_only_keys().first() {
                    return Err(ConfigError::LocalOnly {
                        path: path.clone(),
                        key: (*key).to_string(),
                    });
                }
            }
        }
        Ok(Some(value))
    }

    /// Every table of the file, checked: a file every reader accepts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn drop_value<T>(_: Option<T>) {}
        for table in Table::ALL {
            match table {
                Table::Run => self.part::<RunTable>(table).map(drop_value)?,
                Table::Sandbox => self.part::<SandboxTable>(table).map(drop_value)?,
                Table::Workers => self.part::<WorkersTable>(table).map(drop_value)?,
                Table::Host => self.part::<HostTable>(table).map(drop_value)?,
                Table::Client => self.part::<ClientTable>(table).map(drop_value)?,
                Table::Development => self.part::<DevelopmentTable>(table).map(drop_value)?,
                Table::Permissions => self.part::<PermissionsTable>(table).map(drop_value)?,
                Table::Branches => self.part::<BranchesTable>(table).map(drop_value)?,
                Table::Tracker | Table::Forge => self.part::<toml::Table>(table).map(drop_value)?,
                Table::WebSearch => self.part::<WebSearchTable>(table).map(drop_value)?,
            }
        }
        Ok(())
    }
}

/// Reads one level's file; `None` when it doesn't exist.
pub fn read_config(path: &Path, level: LevelKind, sharing: Sharing) -> Result<Option<ConfigDoc>, ConfigError> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    ConfigDoc::parse(&text, path, level, sharing).map(Some)
}

/// A file's text, or `None` when it doesn't exist. A folder or an unreadable file is an error.
pub fn read_text(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            reason: "must be a file".into(),
        }),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads `mcp[.local].json`; `None` when it doesn't exist.
pub fn load_mcp(path: &Path) -> Result<Option<McpFile>, ConfigError> {
    let Some(text) = read_text(path)? else {
        return Ok(None);
    };
    let invalid = |reason: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    // serde reads a JSON array into a struct too; the file must be an object.
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    if !value.is_object() {
        return Err(invalid("must be a JSON object".into()));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str, level: LevelKind, sharing: Sharing) -> Result<ConfigDoc, ConfigError> {
        ConfigDoc::parse(text, Path::new("config.toml"), level, sharing)
    }

    #[test]
    fn unknown_keys_are_refused() {
        let err = doc("[permisions]\nallow = []\n", LevelKind::Repository, Sharing::Shared).unwrap_err();
        assert!(err.to_string().contains("permisions"), "{err}");
        let d = doc("[sandbox]\nallw = []\n", LevelKind::Repository, Sharing::Shared).unwrap();
        let err = d.part::<SandboxTable>(Table::Sandbox).unwrap_err();
        assert!(err.to_string().contains("allw"), "{err}");
        let d = doc(
            "[permissions.host]\nkeybindings = {}\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<PermissionsTable>(Table::Permissions).is_err());
        // A table key holding anything else, even an empty list (which serde reads as an empty table), is refused.
        let d = doc("permissions = []\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<PermissionsTable>(Table::Permissions).is_err());
        let d = doc("tracker = 1\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<toml::Table>(Table::Tracker).is_err());
        let d = doc("stop = []\n", LevelKind::Global, Sharing::Shared).unwrap();
        assert!(d.part::<RunTable>(Table::Run).is_err());
    }

    /// A broken table fails only its part: a typo in `[tailnet]` leaves `[sandbox]` readable, and a bad `[client]`
    /// leaves `[host]`.
    #[test]
    fn parts_are_parsed_on_their_own() {
        let d = doc(
            "[tracker]\nprovider = 1\n[branches]\nformt = \"x\"\n[permissions]\nallow = [\"read\"]\n[tailnet]\nprot = 1\n[sandbox]\nread = [\"/r\"]\n[client]\ntext_font = 1\n[host]\nroots = [\"~/p\"]\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<BranchesTable>(Table::Branches).is_err());
        assert!(d.part::<RunTable>(Table::Run).is_err());
        assert!(d.part::<ClientTable>(Table::Client).is_err());
        assert_eq!(
            d.part::<PermissionsTable>(Table::Permissions).unwrap().unwrap().allow,
            ["read"]
        );
        assert_eq!(d.part::<SandboxTable>(Table::Sandbox).unwrap().unwrap().read, ["/r"]);
        assert!(d.part::<HostTable>(Table::Host).unwrap().is_some());
        assert!(d.part::<toml::Table>(Table::Tracker).unwrap().is_some());
        assert!(d.part::<toml::Table>(Table::Forge).unwrap().is_none());
    }

    #[test]
    fn global_only_and_local_only_keys() {
        let d = doc("[sandbox]\nbase = \"default\"\n", LevelKind::Parent, Sharing::Shared).unwrap();
        let err = d.part::<SandboxTable>(Table::Sandbox).unwrap_err();
        assert!(matches!(err, ConfigError::GlobalOnly { ref key, .. } if key == "sandbox.base"));
        let refused = |text: &str, table: Table| {
            let d = doc(text, LevelKind::Repository, Sharing::Local).unwrap();
            match table {
                Table::Host => d.part::<HostTable>(table).is_err(),
                Table::Development => d.part::<DevelopmentTable>(table).is_err(),
                Table::Permissions => d.part::<PermissionsTable>(table).is_err(),
                Table::Run => d.part::<RunTable>(table).is_err(),
                Table::Sandbox
                | Table::Workers
                | Table::Client
                | Table::Branches
                | Table::Tracker
                | Table::Forge
                | Table::WebSearch => false,
            }
        };
        for (text, table) in [
            ("[host]\nroots = []\n", Table::Host),
            ("[development]\nsession_analysis = true\n", Table::Development),
            (
                "[permissions.keybindings]\nauto_accept_edits = \"ctrl+a\"\n",
                Table::Permissions,
            ),
            ("[tailnet]\nport = 1\n", Table::Run),
        ] {
            assert!(refused(text, table), "{text}");
            assert!(
                doc(text, LevelKind::Global, Sharing::Shared)
                    .unwrap()
                    .validate()
                    .is_ok()
            );
        }

        let text = "[branches]\nusername = \"me\"\n";
        let err = doc(text, LevelKind::Global, Sharing::Shared)
            .unwrap()
            .part::<BranchesTable>(Table::Branches)
            .unwrap_err();
        assert!(matches!(err, ConfigError::LocalOnly { .. }), "{err}");
        assert!(
            doc(text, LevelKind::Repository, Sharing::Local)
                .unwrap()
                .part::<BranchesTable>(Table::Branches)
                .is_ok()
        );
    }

    #[test]
    fn missing_files_are_none_and_folders_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(Sharing::Shared.file_name());
        assert!(
            read_config(&path, LevelKind::Global, Sharing::Shared)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(&path).unwrap();
        assert!(read_config(&path, LevelKind::Global, Sharing::Shared).is_err());
        assert!(load_mcp(&dir.path().join("mcp.json")).unwrap().is_none());
        std::fs::write(dir.path().join("mcp.json"), "[]").unwrap();
        assert!(load_mcp(&dir.path().join("mcp.json")).is_err());
    }
}
