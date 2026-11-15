//! One file into its parts: a project's `config[.local].toml` table by table, the global tier's host and client files
//! whole, `mcp[.local].json`, and `policies.md` as text.
//!
//! A syntax error or an unknown top-level key fails the whole file; a table that doesn't match its schema, or that
//! a file at this level (or of this sharing) may not set, fails only that table.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;
use crate::config::model::client::ClientFile;
use crate::config::model::host::HostFile;
use crate::config::model::level::LevelKind;
use crate::config::model::paths::{Scope, Sharing, global_tier};
use crate::config::model::tables::{
    BranchesTable, Files, Integration, Levels, PermissionsTable, Rule, SandboxTable, Table, TableValue, WebSearchTable,
    WorkersTable,
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

    /// One table of the file, strictly, with each key's rule checked against the file's level and sharing; `None` when
    /// the file doesn't set it.
    pub fn part<T: TableValue>(&self, table: Table) -> Result<Option<T>, ConfigError> {
        let path = &self.origin.path;
        let invalid = |reason: String| ConfigError::Invalid {
            path: path.clone(),
            reason,
        };
        let name = table.name();
        let Some(value) = self.table.get(name) else {
            return Ok(None);
        };
        // serde reads an empty array as an empty struct: a table key must hold a table.
        if !value.is_table() {
            return Err(invalid(format!("`{name}` must be a table")));
        }
        let value: T = value
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| invalid(format!("[{name}] {e}")))?;
        value.check().map_err(invalid)?;
        // `[tracker]` and `[forge]` are project only as a whole.
        let table_rule = match table {
            Table::Tracker | Table::Forge => vec![(name, Rule::PROJECT)],
            Table::Sandbox | Table::Workers | Table::Permissions | Table::Branches | Table::WebSearch => Vec::new(),
        };
        for (key, rule) in table_rule.into_iter().chain(value.ruled_keys()) {
            self.check_rule(key, rule)?;
        }
        Ok(Some(value))
    }

    /// Refuses `key` when this file may not hold it.
    fn check_rule(&self, key: &str, rule: Rule) -> Result<(), ConfigError> {
        use LevelKind::{Global, Parent, Repository};
        let path = self.origin.path.clone();
        match (rule.levels, self.origin.level) {
            (Levels::Global, Parent | Repository) => {
                return Err(ConfigError::GlobalOnly {
                    path,
                    key: key.to_string(),
                    global: global_tier(),
                });
            }
            (Levels::Project, Global) => {
                return Err(ConfigError::ProjectOnly {
                    path,
                    key: key.to_string(),
                });
            }
            (Levels::Any, Global | Parent | Repository)
            | (Levels::Global, Global)
            | (Levels::Project, Parent | Repository) => {}
        }
        match (rule.files, self.origin.level, self.sharing) {
            (Files::Local, Global | Parent | Repository, Sharing::Shared)
            | (Files::LocalInProjects, Parent | Repository, Sharing::Shared) => Err(ConfigError::LocalOnly {
                path,
                key: key.to_string(),
            }),
            (Files::Either, Global | Parent | Repository, Sharing::Shared | Sharing::Local)
            | (Files::Local | Files::LocalInProjects, Global | Parent | Repository, Sharing::Local)
            | (Files::LocalInProjects, Global, Sharing::Shared) => Ok(()),
        }
    }

    /// Every table of the file, checked: a file every reader accepts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn drop_value<T>(_: Option<T>) {}
        for table in Table::ALL {
            match table {
                Table::Sandbox => self.part::<SandboxTable>(table).map(drop_value)?,
                Table::Workers => self.part::<WorkersTable>(table).map(drop_value)?,
                Table::Permissions => self.part::<PermissionsTable>(table).map(drop_value)?,
                Table::Branches => self.part::<BranchesTable>(table).map(drop_value)?,
                Table::Tracker | Table::Forge => self.part::<Integration>(table).map(drop_value)?,
                Table::WebSearch => self.part::<WebSearchTable>(table).map(drop_value)?,
            }
        }
        Ok(())
    }
}

/// A global tier file of its own scope (host or client), parsed whole: one schema, so a mistake fails the file.
pub trait GlobalFile: serde::de::DeserializeOwned + Default {
    const SCOPE: Scope;
    /// Values the schema accepts but piton can't use.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
    /// Folds the local file in.
    fn merge(&mut self, local: Self);
}

impl GlobalFile for HostFile {
    const SCOPE: Scope = Scope::Host;
    fn check(&self) -> Result<(), String> {
        HostFile::check(self)
    }
    fn merge(&mut self, local: Self) {
        HostFile::merge(self, local);
    }
}

impl GlobalFile for ClientFile {
    const SCOPE: Scope = Scope::Client;
    /// There's no local client file.
    fn merge(&mut self, local: Self) {
        *self = local;
    }
}

/// Parses `text` as the global file at `path`, checked.
pub fn parse_global<T: GlobalFile>(text: &str, path: &Path) -> Result<T, ConfigError> {
    let invalid = |reason: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let value: T = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
    value.check().map_err(invalid)?;
    Ok(value)
}

/// Reads a global file; `None` when it doesn't exist.
pub fn read_global<T: GlobalFile>(path: &Path) -> Result<Option<T>, ConfigError> {
    read_text(path)?.map(|text| parse_global(&text, path)).transpose()
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
        // The host's and the apps' keys have their own files; pi's TUI keybindings are gone.
        for text in [
            "[host]\nroots = []\n",
            "[client]\ntext_font = \"x\"\n",
            "[tailnet]\nport = 1\n",
            "socket = \"/s\"\n",
            "[development]\nsession_analysis = true\n",
        ] {
            assert!(doc(text, LevelKind::Global, Sharing::Shared).is_err(), "{text}");
        }
        let d = doc(
            "[permissions.keybindings]\nauto_accept_edits = \"ctrl+a\"\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<PermissionsTable>(Table::Permissions).is_err());
        // A table key holding anything else, even an empty list (which serde reads as an empty table), is refused.
        let d = doc("permissions = []\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<PermissionsTable>(Table::Permissions).is_err());
        let d = doc("tracker = 1\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<Integration>(Table::Tracker).is_err());
    }

    /// A broken table fails only its part.
    #[test]
    fn tables_are_parsed_on_their_own() {
        let d = doc(
            "[tracker]\nprovider = 1\n[branches]\nformt = \"x\"\n[permissions]\nallow = [\"read\"]\n[sandbox]\nread = [\"/r\"]\n",
            LevelKind::Repository,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<BranchesTable>(Table::Branches).is_err());
        assert_eq!(
            d.part::<PermissionsTable>(Table::Permissions).unwrap().unwrap().allow,
            ["read"]
        );
        assert_eq!(d.part::<SandboxTable>(Table::Sandbox).unwrap().unwrap().read, ["/r"]);
        assert!(d.part::<Integration>(Table::Tracker).unwrap().is_some());
        assert!(d.part::<Integration>(Table::Forge).unwrap().is_none());
    }

    /// Each key's rule, per level and file.
    #[test]
    fn keys_are_where_their_rule_allows() {
        let part = |text: &str, level, sharing, table| -> Result<(), ConfigError> {
            let d = doc(text, level, sharing).unwrap();
            match table {
                Table::Sandbox => d.part::<SandboxTable>(table).map(drop),
                Table::Branches => d.part::<BranchesTable>(table).map(drop),
                Table::WebSearch => d.part::<WebSearchTable>(table).map(drop),
                Table::Tracker | Table::Forge => d.part::<Integration>(table).map(drop),
                Table::Workers | Table::Permissions => Ok(()),
            }
        };
        use LevelKind::{Global, Parent, Repository};
        use Sharing::{Local, Shared};
        // The base sandbox: global only.
        let base = "[sandbox]\nbase = \"default\"\n";
        assert!(matches!(
            part(base, Parent, Shared, Table::Sandbox),
            Err(ConfigError::GlobalOnly { ref key, .. }) if key == "sandbox.base"
        ));
        assert!(part(base, Global, Local, Table::Sandbox).is_ok());
        assert!(part("[sandbox]\nread = [\"/r\"]\n", Repository, Shared, Table::Sandbox).is_ok());
        // Integrations: project only.
        for table in [Table::Tracker, Table::Forge] {
            let text = format!("[{}]\nprovider = \"github\"\n", table.name());
            assert!(matches!(
                part(&text, Global, Shared, table),
                Err(ConfigError::ProjectOnly { .. })
            ));
            assert!(part(&text, Parent, Local, table).is_ok());
        }
        // A secret: local only everywhere.
        let key = "[web_search]\napi_key = \"k\"\n";
        assert!(matches!(
            part(key, Global, Shared, Table::WebSearch),
            Err(ConfigError::LocalOnly { .. })
        ));
        assert!(part(key, Repository, Local, Table::WebSearch).is_ok());
        // A name: local only in projects, either file in the global tier.
        let name = "[branches]\nusername = \"me\"\n";
        assert!(matches!(
            part(name, Repository, Shared, Table::Branches),
            Err(ConfigError::LocalOnly { .. })
        ));
        assert!(part(name, Repository, Local, Table::Branches).is_ok());
        assert!(part(name, Global, Shared, Table::Branches).is_ok());
    }

    #[test]
    fn global_files_are_parsed_whole() {
        let path = Path::new("config.host.toml");
        let host: HostFile = parse_global(
            "roots = [\"~/p\"]\nkeep_awake = false\n[sandbox]\ncommand = [\"nono\"]\n",
            path,
        )
        .unwrap();
        assert_eq!(host.keep_awake, Some(false));
        assert!(parse_global::<HostFile>("[pi]\ncommand = []\n", path).is_err());
        assert!(parse_global::<HostFile>("[host]\nroots = []\n", path).is_err());
        let client: ClientFile = parse_global("text_font = \"Inter\"\n", Path::new("config.client.toml")).unwrap();
        assert_eq!(client.text_font.as_deref(), Some("Inter"));
    }

    #[test]
    fn missing_files_are_none_and_folders_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(
            read_config(&path, LevelKind::Global, Sharing::Shared)
                .unwrap()
                .is_none()
        );
        std::fs::create_dir(&path).unwrap();
        assert!(read_config(&path, LevelKind::Global, Sharing::Shared).is_err());
        assert!(
            read_global::<HostFile>(&dir.path().join("config.host.toml"))
                .unwrap()
                .is_none()
        );
        assert!(load_mcp(&dir.path().join("mcp.json")).unwrap().is_none());
        std::fs::write(dir.path().join("mcp.json"), "[]").unwrap();
        assert!(load_mcp(&dir.path().join("mcp.json")).is_err());
    }
}
