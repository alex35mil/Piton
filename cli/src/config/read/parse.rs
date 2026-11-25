//! One file into its parts: a project's `config[.local].toml` table by table, the global tier's host and client files
//! whole, `mcp[.local].json`, and `policies.md` as text.
//!
//! A syntax error or an unknown top-level key fails the whole file; a table that doesn't match its schema, or that
//! a file at this level (or of this sharing) may not set, fails only that table.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::{
    Spanned,
    de::{DeTable, DeValue},
};

use crate::config::{
    ConfigError,
    model::{
        client::ClientFile,
        host::HostFile,
        key::{Issue, Key, KeyPart, Location},
        level::LevelKind,
        paths::{Scope, Sharing},
        tables::{
            BranchesTable, Files, ForgeTable, Levels, ReadError, Rule, SandboxTable, Table, TableValue, ToolsTable,
            TrackerTable, TrustTable, WebSearchTable, WorkersTable,
        },
    },
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

/// One `config[.local].toml`, syntax-checked, its top-level keys known, ready to read table by table. Keeps the text:
/// each table is read from the parsed document, so an error names its key and line.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    pub origin: FileOrigin,
    pub sharing: Sharing,
    text: String,
    /// Whether errors give the line: a file's do; a value checked on its own was written by Piton (`validate`).
    lines: bool,
}

impl ConfigDoc {
    /// Parses `text` as the file at `path`: TOML, and only top-level keys a part owns.
    pub fn parse(text: &str, path: &Path, level: LevelKind, sharing: Sharing) -> Result<Self, ConfigError> {
        let doc = Self {
            origin: FileOrigin {
                path: path.to_path_buf(),
                level,
            },
            sharing,
            text: text.to_string(),
            lines: true,
        };
        let table = doc.document()?;
        if let Some(key) = table.get_ref().keys().find(|key| Table::of(key.get_ref()).is_none()) {
            return Err(doc.error(key.span(), format!("unknown key `{}`", key.get_ref())));
        }
        Ok(doc)
    }

    /// One table's value on its own (`piton config validate`): read and checked as in a file, without the file's
    /// placement rules, its errors naming keys but no lines. `value` is the table's JSON, keys as the file has them.
    pub fn table_value(table: Table, value: &serde_json::Value) -> Result<Self, ConfigError> {
        let path = PathBuf::from(format!("[{}]", table.name()));
        let mut document = toml::Table::new();
        let value: toml::Value = toml::Value::try_from(value)
            .map_err(|e| ConfigError::invalid(&path, format!("can't be a TOML table: {e}")))?;
        document.insert(table.name().to_string(), value);
        let text = toml::to_string(&document).map_err(|e| ConfigError::invalid(&path, e.to_string()))?;
        // A repository's `config.local.toml` may hold every key (project only, local only, the user's own), so read as
        // one, no placement rule refuses the value.
        let mut doc = Self::parse(&text, &path, LevelKind::Repository, Sharing::Local)?;
        doc.lines = false;
        Ok(doc)
    }

    fn document(&self) -> Result<Spanned<DeTable<'_>>, ConfigError> {
        DeTable::parse(&self.text).map_err(|e| {
            let mut e = e;
            e.set_input(Some(&self.text));
            ConfigError::invalid(&self.origin.path, e.to_string().trim_end())
        })
    }

    /// An error at byte `span` of the file: its key, and its line.
    fn error(&self, span: std::ops::Range<usize>, reason: impl Into<String>) -> ConfigError {
        let at = self.document().ok().map(|document| {
            let mut key = Vec::new();
            locate(document.get_ref(), span.start, &mut key);
            Location {
                key: Key::new(key),
                line: self.lines.then(|| line_of(&self.text, span.start)),
            }
        });
        ConfigError::Invalid {
            path: self.origin.path.clone(),
            at,
            reason: reason.into(),
        }
    }

    /// A check's issue: its key, and the line that sets it.
    fn issue(&self, issue: Issue) -> ConfigError {
        let line = match self.lines {
            true => self
                .document()
                .ok()
                .and_then(|document| span_of(document.get_ref(), issue.key.parts()))
                .map(|span| line_of(&self.text, span.start)),
            false => None,
        };
        ConfigError::Invalid {
            path: self.origin.path.clone(),
            at: Some(Location { key: issue.key, line }),
            reason: issue.reason,
        }
    }

    /// One table of the file, strictly, with each key's rule checked against the file's level and sharing; `None` when
    /// the file doesn't set it.
    pub fn part<T: TableValue>(&self, table: Table) -> Result<Option<T>, ConfigError> {
        let document = self.document()?;
        let Some(value) = document.get_ref().get(table.name()) else {
            return Ok(None);
        };
        if let Some(rule) = table.rule() {
            self.check_rule(table.name(), rule)?;
        }
        let value: T = T::read(value.clone()).map_err(|ReadError { message, span }| match span {
            Some(span) => self.error(span, message),
            None => ConfigError::Invalid {
                path: self.origin.path.clone(),
                at: Some(Location {
                    key: Key::of(&[table.name()]),
                    line: None,
                }),
                reason: message,
            },
        })?;
        value.check().map_err(|issue| self.issue(issue))?;
        for (key, rule) in value.ruled_keys() {
            self.check_rule(key, rule)?;
        }
        Ok(Some(value))
    }

    /// Refuses `key` when this file may not hold it.
    fn check_rule(&self, key: &str, rule: Rule) -> Result<(), ConfigError> {
        use LevelKind::{Global, Parent, Repository};
        let path = self.origin.path.clone();
        match (rule.levels, self.origin.level) {
            (Levels::Project, Global) => {
                return Err(ConfigError::ProjectOnly {
                    path,
                    key: key.to_string(),
                });
            }
            (Levels::Any, Global | Parent | Repository) | (Levels::Project, Parent | Repository) => {}
        }
        match (rule.files, self.origin.level, self.sharing) {
            (Files::Local, Global | Parent | Repository, Sharing::Shared) => Err(ConfigError::LocalOnly {
                path,
                key: key.to_string(),
            }),
            (Files::UsersOwn, Repository, Sharing::Shared) => Err(ConfigError::NotInProjectConfig {
                path,
                key: key.to_string(),
            }),
            (Files::Either, Global | Parent | Repository, Sharing::Shared | Sharing::Local)
            | (Files::Local | Files::UsersOwn, Global | Parent | Repository, Sharing::Local)
            | (Files::UsersOwn, Global | Parent, Sharing::Shared) => Ok(()),
        }
    }

    /// Every table of the file, checked: a file every reader accepts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn drop_value<T>(_: Option<T>) {}
        for table in Table::ALL {
            match table {
                Table::Sandbox => self.part::<SandboxTable>(table).map(drop_value)?,
                Table::Workers => self.part::<WorkersTable>(table).map(drop_value)?,
                Table::Tools => self.part::<ToolsTable>(table).map(drop_value)?,
                Table::Branches => self.part::<BranchesTable>(table).map(drop_value)?,
                Table::Tracker => self.part::<TrackerTable>(table).map(drop_value)?,
                Table::Forge => self.part::<ForgeTable>(table).map(drop_value)?,
                Table::WebSearch => self.part::<WebSearchTable>(table).map(drop_value)?,
                Table::Trust => self.part::<TrustTable>(table).map(drop_value)?,
            }
        }
        Ok(())
    }
}

/// `value` (JSON, keys as the file has them) read and checked as `table` of a `config.toml`, without the file's
/// placement rules; returned as the file would hold it. What setup checks before changing anything at a provider.
pub fn validate_table(table: &str, value: &serde_json::Value) -> Result<serde_json::Value, ConfigError> {
    let Some(table) = Table::of(table) else {
        return Err(ConfigError::invalid(
            format!("[{table}]"),
            format!(
                "not a table of config.toml; expected one of {}",
                Table::ALL.map(Table::name).join(", ")
            ),
        ));
    };
    let doc = ConfigDoc::table_value(table, value)?;
    fn back<T: TableValue + Serialize>(doc: &ConfigDoc, table: Table) -> Result<serde_json::Value, ConfigError> {
        let value = doc.part::<T>(table)?;
        serde_json::to_value(value).map_err(|e| ConfigError::invalid(&doc.origin.path, e.to_string()))
    }
    match table {
        Table::Sandbox => back::<SandboxTable>(&doc, table),
        Table::Workers => back::<WorkersTable>(&doc, table),
        Table::Tools => back::<ToolsTable>(&doc, table),
        Table::Branches => back::<BranchesTable>(&doc, table),
        Table::Tracker => back::<TrackerTable>(&doc, table),
        Table::Forge => back::<ForgeTable>(&doc, table),
        Table::WebSearch => back::<WebSearchTable>(&doc, table),
        Table::Trust => back::<TrustTable>(&doc, table),
    }
}

/// The key at byte `offset`: the deepest key whose key or value holds it.
fn locate(table: &DeTable<'_>, offset: usize, key: &mut Vec<KeyPart>) {
    for (name, value) in table.iter() {
        let holds = |span: std::ops::Range<usize>| span.contains(&offset);
        if holds(name.span()) {
            key.push(KeyPart::Name(name.get_ref().to_string()));
            return;
        }
        if holds(value.span()) || contains_offset(value, offset) {
            key.push(KeyPart::Name(name.get_ref().to_string()));
            locate_value(value, offset, key);
            return;
        }
    }
}

fn locate_value(value: &Spanned<DeValue<'_>>, offset: usize, key: &mut Vec<KeyPart>) {
    match value.get_ref() {
        DeValue::Table(table) => locate(table, offset, key),
        DeValue::Array(items) => {
            if let Some((i, item)) = items
                .iter()
                .enumerate()
                .find(|(_, item)| item.span().contains(&offset) || contains_offset(item, offset))
            {
                key.push(KeyPart::Index(i));
                locate_value(item, offset, key);
            }
        }
        DeValue::String(_) | DeValue::Integer(_) | DeValue::Float(_) | DeValue::Boolean(_) | DeValue::Datetime(_) => {}
    }
}

/// Whether any key or value inside `value` holds `offset`: a `[table]` header's span covers only the header, not the
/// keys under it.
fn contains_offset(value: &Spanned<DeValue<'_>>, offset: usize) -> bool {
    match value.get_ref() {
        DeValue::Table(table) => table.iter().any(|(name, inner)| {
            name.span().contains(&offset) || inner.span().contains(&offset) || contains_offset(inner, offset)
        }),
        DeValue::Array(items) => items
            .iter()
            .any(|item| item.span().contains(&offset) || contains_offset(item, offset)),
        DeValue::String(_) | DeValue::Integer(_) | DeValue::Float(_) | DeValue::Boolean(_) | DeValue::Datetime(_) => {
            false
        }
    }
}

/// Where `key` is set: its key's span, or the nearest enclosing one when part of it isn't in the file.
fn span_of(table: &DeTable<'_>, key: &[KeyPart]) -> Option<std::ops::Range<usize>> {
    let (KeyPart::Name(first), rest) = key.split_first()? else {
        return None;
    };
    let (name, value) = table.get_key_value(first.as_str())?;
    Some(value_span(value, rest).unwrap_or(name.span()))
}

fn value_span(value: &Spanned<DeValue<'_>>, key: &[KeyPart]) -> Option<std::ops::Range<usize>> {
    let (part, rest) = key.split_first()?;
    match (part, value.get_ref()) {
        (KeyPart::Name(_), DeValue::Table(table)) => span_of(table, key),
        (KeyPart::Index(i), DeValue::Array(items)) => {
            let item = items.get(*i)?;
            Some(value_span(item, rest).unwrap_or(item.span()))
        }
        (KeyPart::Name(_) | KeyPart::Index(_), _) => None,
    }
}

/// The 1-based line of byte `offset`.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
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
    let invalid = |reason: String| ConfigError::invalid(path, reason);
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
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Err(ConfigError::invalid(path, "must be a file")),
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
    let invalid = |reason: String| ConfigError::invalid(path, reason);
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
        let err = doc("[tols]\nallow = []\n", LevelKind::Repository, Sharing::Shared).unwrap_err();
        assert!(err.to_string().contains("tols"), "{err}");
        let d = doc("[sandbox]\nallw = []\n", LevelKind::Repository, Sharing::Shared).unwrap();
        let err = d.part::<SandboxTable>(Table::Sandbox).unwrap_err();
        assert!(err.to_string().contains("allw"), "{err}");
        // The host's and the apps' keys have their own files; pi's TUI keybindings are gone.
        for text in [
            "[host]\nroots = []\n",
            "[client]\ntext_font = \"x\"\n",
            "[integrations.tailscale]\nport = 1\n",
            "socket = \"/s\"\n",
            "[debugging]\nsession_analysis = true\n",
            // Tool rules moved to `[tools.permissions]`; the old table is just an unknown key.
            "[permissions]\nallow = [\"read\"]\n",
        ] {
            assert!(doc(text, LevelKind::Global, Sharing::Shared).is_err(), "{text}");
        }
        let err = doc("[permissions]\n", LevelKind::Repository, Sharing::Shared).unwrap_err();
        assert!(err.to_string().contains("unknown key `permissions`"), "{err}");
        let d = doc(
            "[tools.permissions.keybindings]\nauto_accept_edits = \"ctrl+a\"\n",
            LevelKind::Global,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<ToolsTable>(Table::Tools).is_err());
        // A table key holding anything else, even an empty list (which serde reads as an empty table), is refused, at
        // the top and inside `[tools]`.
        for text in ["tools = []\n", "[tools]\npermissions = []\n"] {
            let d = doc(text, LevelKind::Repository, Sharing::Shared).unwrap();
            assert!(d.part::<ToolsTable>(Table::Tools).is_err(), "{text}");
        }
        let d = doc("tracker = 1\n", LevelKind::Repository, Sharing::Shared).unwrap();
        assert!(d.part::<TrackerTable>(Table::Tracker).is_err());
    }

    /// A table's error names its key and the line that sets it; a syntax error keeps toml's own message.
    #[test]
    fn errors_name_the_key_and_line() {
        let located = |text: &str, read: &dyn Fn(&ConfigDoc) -> Result<(), ConfigError>| {
            let d = doc(text, LevelKind::Repository, Sharing::Shared).unwrap();
            match read(&d) {
                Err(ConfigError::Invalid { at: Some(at), .. }) => (at.key.to_string(), at.line),
                other => panic!("{text}: {other:?}"),
            }
        };
        let tracker = |d: &ConfigDoc| d.part::<TrackerTable>(Table::Tracker).map(drop);
        let workers = |d: &ConfigDoc| d.part::<WorkersTable>(Table::Workers).map(drop);
        let branches = |d: &ConfigDoc| d.part::<BranchesTable>(Table::Branches).map(drop);
        // A schema error: an unknown key, a wrong value.
        assert_eq!(
            located(
                "[branches]\nformat = \"x\"\n\n[tracker]\nprovider = \"github\"\nmcp_servr = \"g\"\n",
                &tracker
            ),
            ("tracker.mcp_servr".into(), Some(6))
        );
        assert_eq!(
            located("[branches]\n\nformat = \"x\"\n", &branches),
            ("branches.format".into(), Some(3))
        );
        assert_eq!(
            located("[tracker]\nprovider = \"jira\"\n", &tracker),
            ("tracker.provider".into(), Some(2))
        );
        // A check's issue, inside an array.
        assert_eq!(
            located("[workers]\nskip = [\n  \"ok\",\n  \"../out\",\n]\n", &workers),
            ("workers.skip[1]".into(), Some(4))
        );
        // A whole-file error has no location.
        let err = doc("[tracker\n", LevelKind::Repository, Sharing::Shared).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { at: None, .. }), "{err}");
        assert!(err.to_string().contains("line 1"), "{err}");
        let err = doc("\n[tols]\n", LevelKind::Repository, Sharing::Shared).unwrap_err();
        assert!(err.to_string().contains("line 2, tols: unknown key `tols`"), "{err}");
    }

    /// A broken table fails only its part.
    #[test]
    fn tables_are_parsed_on_their_own() {
        let d = doc(
            "[tracker]\nprovider = 1\n[branches]\nformt = \"x\"\n[tools.permissions]\nallow = [\"read\"]\n[sandbox]\npresets = [\"rust\"]\n",
            LevelKind::Repository,
            Sharing::Shared,
        )
        .unwrap();
        assert!(d.part::<BranchesTable>(Table::Branches).is_err());
        assert_eq!(
            d.part::<ToolsTable>(Table::Tools).unwrap().unwrap().permissions.allow,
            ["read"]
        );
        assert_eq!(
            d.part::<SandboxTable>(Table::Sandbox).unwrap().unwrap().presets,
            [crate::config::Preset::Rust]
        );
        assert!(d.part::<TrackerTable>(Table::Tracker).is_err());
        assert!(d.part::<ForgeTable>(Table::Forge).unwrap().is_none());
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
                Table::Tracker => d.part::<TrackerTable>(table).map(drop),
                Table::Forge => d.part::<ForgeTable>(table).map(drop),
                Table::Trust => d.part::<TrustTable>(table).map(drop),
                Table::Workers | Table::Tools => Ok(()),
            }
        };
        use LevelKind::{Global, Parent, Repository};
        use Sharing::{Local, Shared};
        // Every sandbox key, in any file: what a repository's committed file grants is gated by trust instead.
        for entry in [
            "[sandbox.filesystem]\n\"~/.ssh/config\" = \"read\"\n\"/s\" = \"deny\"\n",
            "[sandbox.network.connections]\n\"*\" = \"allow\"\n",
            "[sandbox]\nprofile = \"yolo\"\nallow_git_control_files = true\npresets = [\"pi-claude-code\"]\n",
            "[[sandbox.commands]]\nmatch = \"bash(husky install *)\"\nruntime = \"sandbox\"\ngrant = \"all\"\n",
        ] {
            for level in [Global, Parent, Repository] {
                for sharing in [Shared, Local] {
                    assert!(part(entry, level, sharing, Table::Sandbox).is_ok(), "{entry}");
                }
            }
        }
        // How trust is re-checked: never in a repository's committed file; a parent folder's is the user's own.
        let reconfirm = "[trust]\nreconfirm = \"on-change\"\n";
        assert!(matches!(
            part(reconfirm, Repository, Shared, Table::Trust),
            Err(ConfigError::NotInProjectConfig { .. })
        ));
        for (level, sharing) in [(Repository, Local), (Parent, Shared), (Global, Shared)] {
            assert!(part(reconfirm, level, sharing, Table::Trust).is_ok());
        }
        // Integrations: project only.
        for table in [Table::Tracker, Table::Forge] {
            let text = format!("[{}]\nprovider = \"none\"\n", table.name());
            assert!(matches!(
                part(&text, Global, Shared, table),
                Err(ConfigError::ProjectOnly { .. })
            ));
            assert!(part(&text, Parent, Local, table).is_ok());
            // A half-written table in the wrong file is refused for being there, before what it's missing.
            let partial = format!("[{}]\nprovider = \"github\"\n", table.name());
            assert!(matches!(
                part(&partial, Global, Shared, table),
                Err(ConfigError::ProjectOnly { .. })
            ));
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
            Err(ConfigError::NotInProjectConfig { .. })
        ));
        assert!(part(name, Repository, Local, Table::Branches).is_ok());
        assert!(part(name, Parent, Shared, Table::Branches).is_ok());
        assert!(part(name, Global, Shared, Table::Branches).is_ok());
    }

    #[test]
    fn global_files_are_parsed_whole() {
        let path = Path::new("config.host.toml");
        let host: HostFile = parse_global("roots = [\"~/p\"]\nkeep_awake = false\n", path).unwrap();
        assert_eq!(host.keep_awake, Some(false));
        assert!(parse_global::<HostFile>("[sandbox]\ncommand = [\"nono\"]\n", path).is_err());
        assert!(parse_global::<HostFile>("[pi]\ncommand = []\n", path).is_err());
        assert!(parse_global::<HostFile>("[host]\nroots = []\n", path).is_err());
        let client_path = Path::new("config.client.toml");
        let client: ClientFile = parse_global("[appearance]\ntext_font = \"Inter\"\n", client_path).unwrap();
        assert_eq!(
            client.appearance.text_font,
            Some(crate::config::PerDevice::Every("Inter".into()))
        );
        // Ungrouped keys are gone: unknown keys.
        assert!(parse_global::<ClientFile>("text_font = \"Inter\"\n", client_path).is_err());
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
