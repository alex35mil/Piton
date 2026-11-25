//! The settings list the apps show and change: keys of the global tier's files (configuration). One catalog says, for
//! each key, which file it lives in (its scope), its type, whether a change applies to the next launch or needs a
//! restart, and whether the apps may change it. Reads and writes go through [`crate::config`]; this module only
//! describes the list.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{DaemonConfig, EditError, GlobalConfig, KeyChange, Paths, Placement, Resolved, Scope, Sharing};

/// When a changed value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// At once, or for the next launch.
    Live,
    /// When the daemon restarts.
    Restart,
}

/// The value's shape, for validating a change and for the apps' controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueType {
    Bool,
    Text,
    TextList,
    Integer,
    /// A length of time as text (`"10s"`); `"never"` too when `never`.
    Duration {
        never: bool,
    },
    Choice {
        options: &'static [&'static str],
    },
    /// Several of the listed options.
    ChoiceList {
        options: &'static [&'static str],
    },
    /// A table of keys (paths, hosts, names) to one of the listed values: one row per entry.
    Map {
        values: &'static [&'static str],
    },
}

#[derive(Debug)]
pub struct Entry {
    /// The key as written in its file (`stop.kill_after` is `[stop] kill_after`).
    pub key: &'static str,
    /// The file family it lives in.
    pub scope: Scope,
    pub value_type: ValueType,
    pub applies: Applies,
    /// `socket` is a test and development override the apps don't change.
    pub editable: bool,
    /// The value when no file sets it, from the daemon's defaults.
    default: fn(&DaemonConfig) -> Value,
}

const fn entry_of(
    scope: Scope,
    key: &'static str,
    value_type: ValueType,
    applies: Applies,
    default: fn(&DaemonConfig) -> Value,
) -> Entry {
    Entry {
        key,
        scope,
        value_type,
        applies,
        editable: true,
        default,
    }
}

const fn host(
    key: &'static str,
    value_type: ValueType,
    applies: Applies,
    default: fn(&DaemonConfig) -> Value,
) -> Entry {
    entry_of(Scope::Host, key, value_type, applies, default)
}

const fn project(key: &'static str, value_type: ValueType, default: fn(&DaemonConfig) -> Value) -> Entry {
    entry_of(Scope::Project, key, value_type, Applies::Live, default)
}

fn empty(_: &DaemonConfig) -> Value {
    json!([])
}

fn null(_: &DaemonConfig) -> Value {
    Value::Null
}

use Applies::{Live, Restart};
use ValueType::{Bool, Integer, Text, TextList};

const DURATION: ValueType = ValueType::Duration { never: false };

/// The values of `[sandbox.filesystem]`.
const FILE_ACCESS: &[&str] = &["read", "write", "deny"];
/// The values of every other `[sandbox]` map.
const EFFECTS: &[&str] = &["allow", "deny"];

fn empty_map(_: &DaemonConfig) -> Value {
    json!({})
}

pub static CATALOG: &[Entry] = &[
    host("keep_awake", Bool, Live, |c| json!(c.keep_awake)),
    host("roots", TextList, Live, empty),
    host("favorites", TextList, Live, empty),
    host(
        "default_attach_mode",
        ValueType::Choice {
            options: &["sandbox", "host"],
        },
        Live,
        |c| json!(c.default_attach_mode),
    ),
    host("stop_empty_session_after", DURATION, Live, |c| {
        json!(c.stop_empty_session_after)
    }),
    host("approval_timeout", ValueType::Duration { never: true }, Live, |c| {
        json!(c.approval_timeout)
    }),
    Entry {
        key: "socket",
        scope: Scope::Host,
        value_type: Text,
        applies: Restart,
        editable: false,
        default: |c| json!(c.socket),
    },
    host("log", Text, Restart, |c| json!(c.log)),
    host("pi.command", TextList, Live, |c| json!(c.pi.command)),
    host("pi.args", TextList, Live, |c| json!(c.pi.args)),
    host("env.login_shell", Bool, Restart, |c| json!(c.env.login_shell)),
    host("stop.terminate_after", DURATION, Live, |c| {
        json!(c.stop.terminate_after)
    }),
    host("stop.kill_after", DURATION, Live, |c| json!(c.stop.kill_after)),
    // Unset (`null`): used when installed. See `crate::integrations`.
    host("integrations.mise.enabled", Bool, Restart, null),
    host("integrations.mise.command", TextList, Restart, |c| {
        json!(c.integrations.mise.command)
    }),
    host("integrations.fnox.enabled", Bool, Restart, null),
    host("integrations.fnox.command", TextList, Restart, |c| {
        json!(c.integrations.fnox.command)
    }),
    host("integrations.tailscale.enabled", Bool, Restart, null),
    host("integrations.tailscale.command", TextList, Restart, |c| {
        json!(c.integrations.tailscale.command)
    }),
    host("integrations.tailscale.port", Integer, Restart, |c| {
        json!(c.integrations.tailnet.port)
    }),
    host("integrations.tailscale.heartbeat", DURATION, Restart, |c| {
        json!(c.integrations.tailnet.heartbeat)
    }),
    host("integrations.tailscale.retry", DURATION, Restart, |c| {
        json!(c.integrations.tailnet.retry)
    }),
    host(
        "debugging.log_level",
        ValueType::Choice {
            options: &["info", "debug"],
        },
        Live,
        |c| json!(c.logging.mode),
    ),
    entry_of(Scope::Client, "text_font", Text, Live, null),
    entry_of(Scope::Client, "code_font", Text, Live, null),
    project(
        "sandbox.presets",
        ValueType::ChoiceList {
            options: &crate::config::Preset::NAMES,
        },
        empty,
    ),
    project(
        "sandbox.profile",
        ValueType::Choice {
            options: &["strict", "standard", "yolo"],
        },
        |_| json!("standard"),
    ),
    // Unset (`null`): the profile's value.
    project("sandbox.allow_tcp_listening", Bool, null),
    project("sandbox.allow_git_control_files", Bool, null),
    project("sandbox.allow_launching_apps", Bool, null),
    project("sandbox.filesystem", ValueType::Map { values: FILE_ACCESS }, empty_map),
    project(
        "sandbox.network.connections",
        ValueType::Map { values: EFFECTS },
        empty_map,
    ),
    project(
        "sandbox.network.unix_socket_connections",
        ValueType::Map { values: EFFECTS },
        empty_map,
    ),
    project(
        "sandbox.network.unix_socket_bindings",
        ValueType::Map { values: EFFECTS },
        empty_map,
    ),
    project("sandbox.macos.services", ValueType::Map { values: EFFECTS }, empty_map),
    // Not `sandbox.silence`: its entries are tables (`{ target, process }`), edited in the file; a notice names each.
    project("workers.skip", TextList, empty),
];

/// The entry for `key` in `scope`.
pub fn entry(scope: Scope, key: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|entry| entry.scope == scope && entry.key == key)
}

/// One setting as the apps see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingInfo {
    pub key: &'static str,
    pub scope: Scope,
    /// The value in effect: the file's, else the default.
    pub value: Value,
    /// Which file of the scope's pair sets it; `None` when neither does (the default applies).
    pub placement: Option<Sharing>,
    /// Whether the apps offer the Local choice: the scope has a local file.
    pub local_choice: bool,
    pub value_type: ValueType,
    pub applies: AppliesInfo,
    pub editable: bool,
    /// Something the user should fix, shown with the setting: an integration enabled but not installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// When a change takes effect, and for a restart-only key whether it changed since the daemon started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppliesInfo {
    Live,
    Restart { pending: bool },
}

/// A setting's identity in the list: its scope and key.
type Id = (Scope, &'static str);

/// What the files said at the last good read, and why they can't be used now if they can't.
#[derive(Debug)]
pub struct Snapshot {
    /// Values in effect.
    values: BTreeMap<Id, Value>,
    placements: BTreeMap<Id, Option<Sharing>>,
    /// Restart-only values the daemon started with.
    started: BTreeMap<Id, Value>,
    /// Warnings by setting: each enabled integration that isn't installed, on its `enabled` key; a `socket` a release build
    /// ignores.
    warnings: BTreeMap<Id, String>,
    /// Why some file's latest version can't be used (its values are the last good ones).
    pub error: Option<String>,
}

impl Snapshot {
    /// Reads the global files as the daemon starts. `integrations` is each integration's state the daemon decided at
    /// start.
    pub fn new(paths: &Paths, integrations: &crate::integrations::Integrations) -> Self {
        let mut snapshot = Self {
            values: BTreeMap::new(),
            placements: BTreeMap::new(),
            started: BTreeMap::new(),
            warnings: integration_warnings(integrations),
            error: None,
        };
        snapshot.reload(paths);
        snapshot.started = snapshot.restart_values();
        snapshot
    }

    fn restart_values(&self) -> BTreeMap<Id, Value> {
        CATALOG
            .iter()
            .filter(|entry| entry.applies == Restart)
            .filter_map(|entry| {
                let id = (entry.scope, entry.key);
                self.values.get(&id).map(|value| (id, value.clone()))
            })
            .collect()
    }

    /// Rereads the global files. Each scope's file is read on its own: one that can't be used keeps its last good
    /// values and says why; the others update.
    pub fn reload(&mut self, paths: &Paths) {
        let GlobalConfig { host, client, project } = crate::config::global(paths);
        let defaults = DaemonConfig::defaults(paths.clone());
        // A release build ignores the host file's `socket`: the row keeps the file's value and says so.
        let socket_note = host
            .as_ref()
            .ok()
            .and_then(|host| DaemonConfig::new(host, paths.clone()).ignored_socket_note());
        match socket_note {
            Some(note) => self.warnings.insert((Scope::Host, "socket"), note),
            None => self.warnings.remove(&(Scope::Host, "socket")),
        };
        let mut errors = Vec::new();
        let scopes: [(Scope, Result<Value, String>); 3] = [
            (
                Scope::Host,
                host.map(|host| file_value(&host)).map_err(|e| e.to_string()),
            ),
            (
                Scope::Client,
                client.map(|client| file_value(&client)).map_err(|e| e.to_string()),
            ),
            (Scope::Project, project_value(project)),
        ];
        for (scope, value) in scopes {
            let placements = paths.global_files(scope).read().map_err(|e| e.to_string());
            match (value, placements) {
                (Ok(value), Ok(placements)) => {
                    for entry in CATALOG.iter().filter(|entry| entry.scope == scope) {
                        let id = (scope, entry.key);
                        let current = lookup(&value, entry.key)
                            .cloned()
                            .unwrap_or_else(|| (entry.default)(&defaults));
                        self.values.insert(id, current);
                        self.placements.insert(id, placements.of(entry.key));
                    }
                }
                (Err(error), _) | (_, Err(error)) => errors.push(error),
            }
        }
        self.error = (!errors.is_empty()).then(|| errors.join("; "));
    }

    /// Refuses a change to a value the apps show read-only because the file gives it options.
    pub fn check_editable(&self, change: &Change) -> Result<(), SetError> {
        let key = change.change.key.to_string();
        let current = self.values.get(&(change.scope, key.as_str()));
        match (entry(change.scope, &key), current) {
            (Some(entry), Some(value)) if has_options(value) => Err(SetError::NotEditable { key: entry.key }),
            (Some(_) | None, _) => Ok(()),
        }
    }

    pub fn info(&self) -> Vec<SettingInfo> {
        CATALOG
            .iter()
            .map(|entry| {
                let id = (entry.scope, entry.key);
                let value = self.values.get(&id).cloned().unwrap_or(Value::Null);
                let applies = match entry.applies {
                    Live => AppliesInfo::Live,
                    Restart => AppliesInfo::Restart {
                        pending: self.started.get(&id).is_some_and(|started| *started != value),
                    },
                };
                SettingInfo {
                    key: entry.key,
                    scope: entry.scope,
                    placement: self.placements.get(&id).copied().flatten(),
                    local_choice: entry.scope.local_name().is_some(),
                    value_type: entry.value_type,
                    applies,
                    editable: entry.editable && !has_options(&value),
                    warning: self.warnings.get(&id).cloned(),
                    value,
                }
            })
            .collect()
    }
}

/// The warning for each integration enabled but not installed, on its `enabled` key.
fn integration_warnings(integrations: &crate::integrations::Integrations) -> BTreeMap<Id, String> {
    use crate::integrations::{Integration, IntegrationState};
    Integration::ALL
        .iter()
        .filter_map(|&integration| {
            let key = integration.enabled_key();
            match integrations.get(integration) {
                IntegrationState::Unavailable(unavailable) => Some(((Scope::Host, key), unavailable.to_string())),
                IntegrationState::Off(_) | IntegrationState::Used(_) => None,
            }
        })
        .collect()
}

/// A file's values in its own shape.
fn file_value<T: Serialize>(file: &T) -> Value {
    serde_json::to_value(file).unwrap_or_else(|_| json!({}))
}

/// The global `config.toml`'s tables the apps show, in the file's shape.
fn project_value(project: Result<Resolved, crate::config::ConfigError>) -> Result<Value, String> {
    let project = project.map_err(|e| e.to_string())?;
    let sandbox = project.sandbox.map_err(|e| e.to_string())?;
    let workers = project.workers.map_err(|e| e.to_string())?;
    Ok(json!({"sandbox": file_value(&sandbox), "workers": file_value(&workers)}))
}

fn lookup<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(value, |value, part| value.get(part))
}

#[derive(Debug, thiserror::Error)]
pub enum SetError {
    #[error("unknown setting {key:?} in {scope:?}")]
    Unknown { scope: Scope, key: String },
    #[error("{key} can't be changed from the apps")]
    NotEditable { key: &'static str },
    #[error("{key}: expected {expected}")]
    WrongType { key: &'static str, expected: &'static str },
    #[error("{key} has no Local choice: every client shares it")]
    NoLocal { key: &'static str },
    #[error(transparent)]
    Edit(#[from] EditError),
}

/// A validated change to a global file.
#[derive(Debug)]
pub struct Change {
    pub scope: Scope,
    pub change: KeyChange,
    pub placement: Placement,
}

/// Checks a change from the apps: the key exists in its scope and may change, the value has its type (`null` resets
/// it), and `local` is given only where the scope has a local file.
pub fn validate(scope: Scope, key: &str, value: &Value, local: Option<bool>) -> Result<Change, SetError> {
    let entry = entry(scope, key).ok_or_else(|| SetError::Unknown {
        scope,
        key: key.to_string(),
    })?;
    if !entry.editable {
        return Err(SetError::NotEditable { key: entry.key });
    }
    let value = match value {
        Value::Null => None,
        Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            check_type(entry, value)?;
            Some(value.clone())
        }
    };
    let placement = match (local, scope.local_name()) {
        (None, _) => Placement::Current,
        (Some(_), None) => return Err(SetError::NoLocal { key: entry.key }),
        (Some(true), Some(_)) => Placement::Move(Sharing::Local),
        (Some(false), Some(_)) => Placement::Move(Sharing::Shared),
    };
    Ok(Change {
        scope,
        change: KeyChange {
            key: catalog_key(entry.key),
            value,
        },
        placement,
    })
}

/// A catalog key as a key path: every part is bare.
fn catalog_key(key: &str) -> crate::config::KeyPath {
    crate::config::KeyPath::new(key.split('.').map(str::to_string).collect())
}

/// A list with an entry that isn't a name, such as `presets = [{ name = "pi-claude-code", login = "keychain" }]`: the
/// apps' list control can't show the options, so the value is edited in the file.
fn has_options(value: &Value) -> bool {
    value.as_array().is_some_and(|items| items.iter().any(Value::is_object))
}

/// The value has the entry's type, so the apps' controls and the file agree.
fn check_type(entry: &Entry, value: &Value) -> Result<(), SetError> {
    let integer = |value: &Value| value.as_i64().is_some_and(|n| n >= 0);
    let (ok, expected) = match entry.value_type {
        Bool => (value.is_boolean(), "true or false"),
        Text => (value.is_string(), "a string"),
        TextList => (
            value.as_array().is_some_and(|items| items.iter().all(Value::is_string)),
            "a list of strings",
        ),
        Integer => (integer(value), "a whole number"),
        ValueType::Duration { never: false } => (
            value
                .as_str()
                .is_some_and(|text| text.parse::<crate::config::ConfigDuration>().is_ok()),
            "a duration like \"10s\"",
        ),
        ValueType::Duration { never: true } => (
            value
                .as_str()
                .is_some_and(|text| text.parse::<crate::config::Timeout>().is_ok()),
            "a duration like \"10s\", or \"never\"",
        ),
        ValueType::Choice { options } => (
            value.as_str().is_some_and(|choice| options.contains(&choice)),
            "one of the listed choices",
        ),
        ValueType::ChoiceList { options } => (
            value.as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|c| options.contains(&c)))
            }),
            "a list of the listed choices",
        ),
        ValueType::Map { values } => (
            value.as_object().is_some_and(|map| {
                map.iter()
                    .all(|(key, v)| !key.is_empty() && v.as_str().is_some_and(|v| values.contains(&v)))
            }),
            "a table of keys to the listed values",
        ),
    };
    match ok {
        true => Ok(()),
        false => Err(SetError::WrongType {
            key: entry.key,
            expected,
        }),
    }
}

/// Writes a change to its global file.
pub fn write(paths: &Paths, change: &Change) -> Result<(), SetError> {
    crate::config::set_global(paths, change.scope, &change.change, change.placement)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn paths(dir: &Path) -> Paths {
        Paths {
            home: dir.to_path_buf(),
            global: dir.join("piton"),
        }
    }

    fn info<'a>(list: &'a [SettingInfo], scope: Scope, key: &str) -> &'a SettingInfo {
        list.iter().find(|s| s.scope == scope && s.key == key).unwrap()
    }

    /// Every integration off: no warnings.
    fn no_tools() -> crate::integrations::Integrations {
        use crate::integrations::{IntegrationState, OffReason};
        crate::integrations::Integrations {
            mise: IntegrationState::Off(OffReason::Disabled),
            fnox: IntegrationState::Off(OffReason::Disabled),
            tailscale: IntegrationState::Off(OffReason::Disabled),
        }
    }

    /// An integration enabled but not installed shows its warning on its `enabled` key, and nowhere else.
    #[test]
    fn unavailable_integrations_are_warned_about() {
        use crate::integrations::{Integration, IntegrationState, Unavailable};
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut integrations = no_tools();
        integrations.tailscale = IntegrationState::Unavailable(Unavailable {
            integration: Integration::Tailscale,
            program: "tailscale".into(),
        });
        let list = Snapshot::new(&paths, &integrations).info();
        let warning = info(&list, Scope::Host, "integrations.tailscale.enabled")
            .warning
            .clone()
            .unwrap();
        assert!(warning.contains("[integrations.tailscale] enabled = true"), "{warning}");
        assert_eq!(list.iter().filter(|s| s.warning.is_some()).count(), 1);
    }

    #[test]
    fn every_catalog_key_is_a_key_of_its_file() {
        // Writing each key to an empty file of its scope must parse: the catalog can't name a key the schema lacks.
        // And the list shows what the file sets: every file it reads reaches the apps.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let defaults = DaemonConfig::defaults(paths.clone());
        for entry in CATALOG {
            // Not the default, so reading it back proves the file's value reached the list.
            let default = (entry.default)(&defaults);
            let sample = match entry.value_type {
                ValueType::ChoiceList { options } => json!([options[0]]),
                Bool => json!(default != json!(true)),
                Text => json!("/x"),
                // `workers.skip` takes relative paths.
                TextList if entry.key == "workers.skip" => json!(["x"]),
                TextList => json!(["~/x"]),
                // Keys by table: a host, a Mach service, or a path.
                ValueType::Map { values } if entry.key == "sandbox.network.connections" => {
                    json!({ "github.com:22": values[0] })
                }
                ValueType::Map { values } if entry.key == "sandbox.macos.services" => {
                    json!({ "com.example.agent": values[0] })
                }
                ValueType::Map { values } => json!({ "~/x": values[0] }),
                Integer => json!(default.as_u64().unwrap_or(0) + 1),
                // Not the default of any duration key.
                ValueType::Duration { .. } => json!("42s"),
                ValueType::Choice { options } => {
                    json!(
                        options
                            .iter()
                            .find(|option| json!(option) != default)
                            .unwrap_or(&options[0])
                    )
                }
            };
            // `check_type` is what `validate` uses; calling it directly covers the non-editable `socket` too.
            check_type(entry, &sample).unwrap_or_else(|e| panic!("{}: {e}", entry.key));
            let _ = std::fs::remove_file(&paths.global_files(entry.scope).shared);
            let change = Change {
                scope: entry.scope,
                change: KeyChange {
                    key: catalog_key(entry.key),
                    value: Some(sample.clone()),
                },
                placement: Placement::Current,
            };
            write(&paths, &change).unwrap_or_else(|e| panic!("{}: {e}", entry.key));
            let snapshot = Snapshot::new(&paths, &no_tools());
            assert_eq!(snapshot.error, None, "{}", entry.key);
            assert_eq!(
                info(&snapshot.info(), entry.scope, entry.key).value,
                sample,
                "{}",
                entry.key
            );
        }
    }

    /// A map is written whole as its table, its keys quoted; a value outside the list, or a key the file refuses, is
    /// refused, leaving the file as it was; clearing it (`null`) removes the table.
    #[test]
    fn maps_are_set_refused_or_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let set = |value: Value| -> Result<SettingInfo, SetError> {
            let change = validate(Scope::Project, "sandbox.filesystem", &value, None)?;
            write(&paths, &change)?;
            let snapshot = Snapshot::new(&paths, &no_tools());
            assert_eq!(snapshot.error, None);
            Ok(info(&snapshot.info(), Scope::Project, "sandbox.filesystem").clone())
        };
        let map = json!({ "~/.cache/x": "write", "~/secrets": "deny" });
        let on = set(map.clone()).unwrap();
        assert_eq!(on.value, map);
        assert_eq!(on.placement, Some(Sharing::Shared));
        let text = std::fs::read_to_string(paths.global.join("config.toml")).unwrap();
        assert!(text.contains("\"~/.cache/x\" = \"write\""), "{text}");
        assert!(matches!(
            set(json!({ "~/x": "allow" })),
            Err(SetError::WrongType { .. })
        ));
        assert!(set(json!({ "relative": "write" })).is_err(), "the file's own check");
        assert_eq!(
            info(
                &Snapshot::new(&paths, &no_tools()).info(),
                Scope::Project,
                "sandbox.filesystem"
            )
            .value,
            map,
            "left as it was"
        );
        let off = set(Value::Null).unwrap();
        assert_eq!(off.value, json!({}));
        assert_eq!(off.placement, None, "no file sets it");
    }

    #[test]
    fn changes_are_typed_and_reported_with_their_placement() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.global).unwrap();
        std::fs::write(paths.global.join("config.host.toml"), "roots = [\"~/a\"]\n").unwrap();
        let mut snapshot = Snapshot::new(&paths, &no_tools());

        assert!(matches!(
            validate(Scope::Host, "nope", &json!(1), None),
            Err(SetError::Unknown { .. })
        ));
        assert!(matches!(
            validate(Scope::Client, "roots", &json!([]), None),
            Err(SetError::Unknown { .. })
        ));
        assert!(matches!(
            validate(Scope::Host, "socket", &json!("/s"), None),
            Err(SetError::NotEditable { .. })
        ));
        assert!(matches!(
            validate(Scope::Host, "roots", &json!("~/a"), None),
            Err(SetError::WrongType { .. })
        ));
        assert!(matches!(
            validate(Scope::Host, "debugging.log_level", &json!("loud"), None),
            Err(SetError::WrongType { .. })
        ));
        // Durations are text in their grammar; only the question's timeout may be `never`.
        for (key, value) in [
            ("stop_empty_session_after", json!(10000)),
            ("stop_empty_session_after", json!("10")),
            ("stop_empty_session_after", json!("never")),
            ("stop.kill_after", json!("0s")),
            ("approval_timeout", json!("1m30s")),
        ] {
            assert!(
                matches!(
                    validate(Scope::Host, key, &value, None),
                    Err(SetError::WrongType { .. })
                ),
                "{key} = {value}"
            );
        }
        assert!(validate(Scope::Host, "approval_timeout", &json!("never"), None).is_ok());
        // The client file has no local one: a device keeps its own value in its app.
        assert!(matches!(
            validate(Scope::Client, "text_font", &json!("Inter"), Some(true)),
            Err(SetError::NoLocal { .. })
        ));

        write(
            &paths,
            &validate(Scope::Host, "roots", &json!(["~/b"]), Some(true)).unwrap(),
        )
        .unwrap();
        write(
            &paths,
            &validate(Scope::Host, "integrations.tailscale.port", &json!(9000), None).unwrap(),
        )
        .unwrap();
        write(
            &paths,
            &validate(Scope::Host, "keep_awake", &json!(false), None).unwrap(),
        )
        .unwrap();
        write(
            &paths,
            &validate(Scope::Host, "approval_timeout", &json!("10m"), None).unwrap(),
        )
        .unwrap();
        snapshot.reload(&paths);
        assert_eq!(snapshot.error, None);

        let list = snapshot.info();
        let roots = info(&list, Scope::Host, "roots");
        assert_eq!(roots.value, json!(["~/b"]));
        assert_eq!(roots.placement, Some(Sharing::Local));
        assert!(roots.local_choice);
        assert!(!info(&list, Scope::Client, "text_font").local_choice);
        assert_eq!(
            info(&list, Scope::Host, "integrations.tailscale.port").applies,
            AppliesInfo::Restart { pending: true }
        );
        assert_eq!(
            info(&list, Scope::Host, "integrations.tailscale.retry").applies,
            AppliesInfo::Restart { pending: false }
        );
        assert_eq!(info(&list, Scope::Host, "keep_awake").value, json!(false));
        assert_eq!(
            info(&list, Scope::Host, "integrations.tailscale.enabled").value,
            json!(null)
        );
        assert_eq!(info(&list, Scope::Host, "approval_timeout").value, json!("10m"));
        assert_eq!(info(&list, Scope::Host, "stop.kill_after").value, json!("3s"));

        // A value the schema refuses never reaches the file.
        let before = std::fs::read_to_string(paths.global.join("config.host.toml")).unwrap();
        assert!(
            before.contains("port = 9000")
                && before.contains("keep_awake = false")
                && before.contains("approval_timeout = \"10m\""),
            "{before}"
        );
        assert!(
            write(
                &paths,
                &validate(Scope::Host, "integrations.tailscale.port", &json!(70000), None).unwrap()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(paths.global.join("config.host.toml")).unwrap(),
            before
        );

        // A broken hand edit keeps that file's last good values and says why; the other files still update.
        std::fs::write(paths.global.join("config.host.local.toml"), "roots = [\n").unwrap();
        std::fs::write(paths.global.join("config.client.toml"), "text_font = \"Inter\"\n").unwrap();
        snapshot.reload(&paths);
        assert!(snapshot.error.is_some());
        let list = snapshot.info();
        assert_eq!(info(&list, Scope::Host, "roots").value, json!(["~/b"]));
        assert_eq!(info(&list, Scope::Client, "text_font").value, json!("Inter"));
    }

    /// Presets with options are edited in the file: the apps' list control would drop the options.
    #[test]
    fn presets_with_options_are_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.global).unwrap();
        let config = paths.global.join("config.toml");
        std::fs::write(&config, "[sandbox]\npresets = [\"rust\", \"pi-claude-code\"]\n").unwrap();
        let mut snapshot = Snapshot::new(&paths, &no_tools());
        let presets = |s: &Snapshot| info(&s.info(), Scope::Project, "sandbox.presets").clone();
        assert!(presets(&snapshot).editable);
        let change = validate(Scope::Project, "sandbox.presets", &json!(["rust", "swift"]), None).unwrap();
        assert!(snapshot.check_editable(&change).is_ok());

        std::fs::write(
            &config,
            "[sandbox]\npresets = [\"rust\", { name = \"pi-claude-code\", login = \"keychain\" }]\n",
        )
        .unwrap();
        snapshot.reload(&paths);
        let shown = presets(&snapshot);
        assert!(!shown.editable);
        assert_eq!(
            shown.value,
            json!(["rust", {"login": "keychain", "name": "pi-claude-code"}])
        );
        assert!(matches!(
            snapshot.check_editable(&change),
            Err(SetError::NotEditable { .. })
        ));
    }
}
