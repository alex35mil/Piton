//! The settings list the apps show and change: keys of the global tier's files (configuration). One catalog says, for
//! each key, which file it lives in (its scope), its type, whether a change applies to the next launch or needs a
//! restart, and whether the apps may change it. Reads and writes go through [`crate::config`]; this module only
//! describes the list.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{DaemonConfig, GlobalConfig, Paths, Resolved, Scope, Sharing};
use crate::config::{EditError, KeyChange, Placement};

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
    IntegerList,
    Choice {
        options: &'static [&'static str],
    },
    /// A network profile name, or `false` for no proxy filtering.
    NetworkProfile,
}

#[derive(Debug)]
pub struct Entry {
    /// The key as written in its file (`tailnet.port` is `[tailnet] port`).
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
use ValueType::{Bool, Integer, IntegerList, Text, TextList};

pub static CATALOG: &[Entry] = &[
    host("keep_awake", Bool, Live, |c| json!(c.keep_awake)),
    host("roots", TextList, Live, empty),
    host("favorites", TextList, Live, empty),
    host(
        "logging",
        ValueType::Choice {
            options: &["info", "debug"],
        },
        Live,
        |c| json!(c.logging.mode),
    ),
    host(
        "default_attach_mode",
        ValueType::Choice {
            options: &["sandbox", "host"],
        },
        Live,
        |c| json!(c.default_attach_mode),
    ),
    host("empty_grace_ms", Integer, Live, |c| json!(c.empty_grace_ms)),
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
    host("sandbox.command", TextList, Live, |c| json!(c.sandbox.command)),
    host("env.login_shell", Bool, Restart, |c| json!(c.env.login_shell)),
    host("env.mise", Bool, Restart, |c| json!(c.env.mise)),
    host("stop.grace_ms", Integer, Live, |c| json!(c.stop.grace_ms)),
    host("stop.term_grace_ms", Integer, Live, |c| json!(c.stop.term_grace_ms)),
    host("tailnet.enabled", Bool, Restart, |c| json!(c.tailnet.enabled)),
    host("tailnet.port", Integer, Restart, |c| json!(c.tailnet.port)),
    host("tailnet.cli", Text, Restart, |c| json!(c.tailnet.cli)),
    host("tailnet.heartbeat_ms", Integer, Restart, |c| {
        json!(c.tailnet.heartbeat_ms)
    }),
    host("tailnet.retry_ms", Integer, Restart, |c| json!(c.tailnet.retry_ms)),
    entry_of(Scope::Client, "text_font", Text, Live, null),
    entry_of(Scope::Client, "code_font", Text, Live, null),
    project("sandbox.base", Text, |_| json!(crate::launch::DEFAULT_SANDBOX_BASE)),
    project("sandbox.groups", TextList, empty),
    project("sandbox.allow", TextList, empty),
    project("sandbox.read", TextList, empty),
    project("sandbox.read_file", TextList, empty),
    project("sandbox.allow_file", TextList, empty),
    project("sandbox.deny", TextList, empty),
    project("sandbox.bypass", TextList, empty),
    project("sandbox.unix_socket", TextList, empty),
    project("sandbox.network.open_port", IntegerList, empty),
    project("sandbox.network.profile", ValueType::NetworkProfile, null),
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
    /// Why some file's latest version can't be used (its values are the last good ones).
    pub error: Option<String>,
}

impl Snapshot {
    /// Reads the global files as the daemon starts.
    pub fn new(paths: &Paths) -> Self {
        let mut snapshot = Self {
            values: BTreeMap::new(),
            placements: BTreeMap::new(),
            started: BTreeMap::new(),
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
                    value,
                    placement: self.placements.get(&id).copied().flatten(),
                    local_choice: entry.scope.local_name().is_some(),
                    value_type: entry.value_type,
                    applies,
                    editable: entry.editable,
                }
            })
            .collect()
    }
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
            key: entry.key.to_string(),
            value,
        },
        placement,
    })
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
        IntegerList => (
            value.as_array().is_some_and(|items| items.iter().all(integer)),
            "a list of whole numbers",
        ),
        ValueType::Choice { options } => (
            value.as_str().is_some_and(|choice| options.contains(&choice)),
            "one of the listed choices",
        ),
        ValueType::NetworkProfile => (
            match value {
                Value::String(_) | Value::Bool(false) => true,
                Value::Null | Value::Bool(true) | Value::Number(_) | Value::Array(_) | Value::Object(_) => false,
            },
            "a profile name, or false",
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
                ValueType::NetworkProfile => json!(false),
                Bool => json!(default != json!(true)),
                Text => json!("/x"),
                // `workers.skip` takes relative paths, `[sandbox]` absolute ones.
                TextList if entry.key == "workers.skip" => json!(["x"]),
                TextList => json!(["~/x"]),
                Integer => json!(default.as_u64().unwrap_or(0) + 1),
                IntegerList => json!([1]),
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
                    key: entry.key.into(),
                    value: Some(sample.clone()),
                },
                placement: Placement::Current,
            };
            write(&paths, &change).unwrap_or_else(|e| panic!("{}: {e}", entry.key));
            let snapshot = Snapshot::new(&paths);
            assert_eq!(snapshot.error, None, "{}", entry.key);
            assert_eq!(
                info(&snapshot.info(), entry.scope, entry.key).value,
                sample,
                "{}",
                entry.key
            );
        }
    }

    #[test]
    fn changes_are_typed_and_reported_with_their_placement() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.global).unwrap();
        std::fs::write(paths.global.join("config.host.toml"), "roots = [\"~/a\"]\n").unwrap();
        let mut snapshot = Snapshot::new(&paths);

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
            validate(Scope::Host, "logging", &json!("loud"), None),
            Err(SetError::WrongType { .. })
        ));
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
            &validate(Scope::Host, "tailnet.port", &json!(9000), None).unwrap(),
        )
        .unwrap();
        write(
            &paths,
            &validate(Scope::Host, "keep_awake", &json!(false), None).unwrap(),
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
            info(&list, Scope::Host, "tailnet.port").applies,
            AppliesInfo::Restart { pending: true }
        );
        assert_eq!(
            info(&list, Scope::Host, "tailnet.enabled").applies,
            AppliesInfo::Restart { pending: false }
        );
        assert_eq!(info(&list, Scope::Host, "keep_awake").value, json!(false));
        assert_eq!(info(&list, Scope::Host, "tailnet.enabled").value, json!(true));

        // A value the schema refuses never reaches the file.
        let before = std::fs::read_to_string(paths.global.join("config.host.toml")).unwrap();
        assert!(
            before.contains("port = 9000") && before.contains("keep_awake = false"),
            "{before}"
        );
        assert!(
            write(
                &paths,
                &validate(Scope::Host, "tailnet.port", &json!(70000), None).unwrap()
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
}
