//! The settings list the apps show and change: keys of the global `config.toml` / `config.local.toml` (configuration)
//! and the host's state values (in the state database). One catalog says, for each key, where it lives, its type,
//! whether a change applies to the next launch or needs a restart, and whether the apps may change it. Reads and
//! writes go through [`crate::config`] and [`crate::state`]; this module only describes the list.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};
use toml_edit::Value as Toml;

use crate::config::write::edit::{EditError, Placements, Target};
use crate::config::{DaemonConfig, GlobalConfig, Paths, Sharing};

/// Where a value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The global `config.toml` pair.
    Config,
    State(crate::state::Setting),
}

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
    pub key: &'static str,
    pub kind: Kind,
    pub value_type: ValueType,
    pub applies: Applies,
    /// `socket` is a test and development override the apps don't change.
    pub editable: bool,
    /// The value when no file sets it, from the daemon's defaults.
    default: fn(&DaemonConfig) -> Value,
}

const fn config(
    key: &'static str,
    value_type: ValueType,
    applies: Applies,
    default: fn(&DaemonConfig) -> Value,
) -> Entry {
    Entry {
        key,
        kind: Kind::Config,
        value_type,
        applies,
        editable: true,
        default,
    }
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
    Entry {
        key: "keep_awake",
        kind: Kind::State(crate::state::Setting::KeepAwake),
        value_type: Bool,
        applies: Live,
        editable: true,
        default: |_| json!(true),
    },
    config("host.roots", TextList, Live, empty),
    config("host.favorites", TextList, Live, empty),
    config(
        "host.logging",
        ValueType::Choice {
            options: &["info", "debug"],
        },
        Live,
        |c| json!(c.logging.mode),
    ),
    config(
        "host.attach_mode",
        ValueType::Choice {
            options: &["sandbox", "host"],
        },
        Live,
        |c| json!(c.attach_mode),
    ),
    config("host.empty_grace_ms", Integer, Live, |c| json!(c.empty_grace_ms)),
    config("client.text_font", Text, Live, null),
    config("client.code_font", Text, Live, null),
    Entry {
        key: "socket",
        kind: Kind::Config,
        value_type: Text,
        applies: Restart,
        editable: false,
        default: |c| json!(c.socket),
    },
    config("log", Text, Restart, |c| json!(c.log)),
    config("pi.command", TextList, Live, |c| json!(c.pi.command)),
    config("pi.args", TextList, Live, |c| json!(c.pi.args)),
    config("env.login_shell", Bool, Restart, |c| json!(c.env.login_shell)),
    config("env.mise", Bool, Restart, |c| json!(c.env.mise)),
    config("stop.grace_ms", Integer, Live, |c| json!(c.stop.grace_ms)),
    config("stop.term_grace_ms", Integer, Live, |c| json!(c.stop.term_grace_ms)),
    config("tailnet.enabled", Bool, Restart, |c| json!(c.tailnet.enabled)),
    config("tailnet.port", Integer, Restart, |c| json!(c.tailnet.port)),
    config("tailnet.cli", Text, Restart, |c| json!(c.tailnet.cli)),
    config("tailnet.heartbeat_ms", Integer, Restart, |c| {
        json!(c.tailnet.heartbeat_ms)
    }),
    config("tailnet.retry_ms", Integer, Restart, |c| json!(c.tailnet.retry_ms)),
    config("sandbox.command", TextList, Live, |c| json!(c.sandbox.command)),
    config("sandbox.base", Text, Live, |_| json!(crate::sandbox::DEFAULT_BASE)),
    config("sandbox.groups", TextList, Live, empty),
    config("sandbox.allow", TextList, Live, empty),
    config("sandbox.read", TextList, Live, empty),
    config("sandbox.read_file", TextList, Live, empty),
    config("sandbox.allow_file", TextList, Live, empty),
    config("sandbox.deny", TextList, Live, empty),
    config("sandbox.bypass", TextList, Live, empty),
    config("sandbox.unix_socket", TextList, Live, empty),
    config("sandbox.network.open_port", IntegerList, Live, empty),
    config("sandbox.network.profile", ValueType::NetworkProfile, Live, null),
    config("workers.skip", TextList, Live, empty),
];

pub fn entry(key: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|entry| entry.key == key)
}

/// One setting as the apps see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingInfo {
    pub key: &'static str,
    /// The value in effect: the file's (or the database's), else the default.
    pub value: Value,
    pub kind: KindInfo,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KindInfo {
    Config {
        /// Which file of the pair sets it; `None` when neither does (the default applies).
        placement: Option<Sharing>,
    },
    State,
}

/// What the files said at the last good read, and what they say now if they can't be used.
#[derive(Debug)]
pub struct Snapshot {
    /// Configuration values in effect, by key.
    values: BTreeMap<&'static str, Value>,
    placements: BTreeMap<&'static str, Option<Sharing>>,
    /// Restart-only values the daemon started with.
    started: BTreeMap<&'static str, Value>,
    /// Why the files' latest version can't be used (the values are the last good ones).
    pub error: Option<String>,
}

/// The outcome of rereading the global files.
#[derive(Debug)]
pub enum Read {
    /// The new configuration.
    Good(Box<DaemonConfig>),
    /// Invalid: the previous configuration stays in effect, and the snapshot's `error` says why.
    Kept,
}

impl Snapshot {
    /// Reads the global files for a daemon that runs with `config`.
    pub fn new(config: &DaemonConfig) -> Self {
        let mut snapshot = Self {
            values: BTreeMap::new(),
            placements: BTreeMap::new(),
            started: BTreeMap::new(),
            error: None,
        };
        let _ = snapshot.reload(config.paths.clone());
        snapshot.started = snapshot.restart_values();
        snapshot
    }

    fn restart_values(&self) -> BTreeMap<&'static str, Value> {
        CATALOG
            .iter()
            .filter(|entry| entry.applies == Restart)
            .filter_map(|entry| self.values.get(entry.key).map(|value| (entry.key, value.clone())))
            .collect()
    }

    /// Rereads the global files: on success the new configuration (`socket` stays the running one), else the error is
    /// kept for the apps and the previous values stay.
    pub fn reload(&mut self, paths: Paths) -> Read {
        let read = || -> Result<_, String> {
            let global = crate::config::global(&paths).map_err(|e| e.to_string())?;
            let placements = paths.global_files().read().map_err(|e| e.to_string())?;
            Ok((global, placements))
        };
        match read() {
            Ok((global, placements)) => {
                let config = DaemonConfig::new(&global, paths.clone());
                self.values = values(&global, &paths);
                self.placements = placement_map(&placements);
                self.error = None;
                Read::Good(Box::new(config))
            }
            Err(error) => {
                self.error = Some(error);
                Read::Kept
            }
        }
    }

    pub fn info(&self, keep_awake: bool) -> Vec<SettingInfo> {
        CATALOG
            .iter()
            .map(|entry| {
                let (value, kind) = match entry.kind {
                    Kind::State(crate::state::Setting::KeepAwake) => (json!(keep_awake), KindInfo::State),
                    Kind::Config => (
                        self.values.get(entry.key).cloned().unwrap_or(Value::Null),
                        KindInfo::Config {
                            placement: self.placements.get(entry.key).copied().flatten(),
                        },
                    ),
                };
                let applies = match entry.applies {
                    Live => AppliesInfo::Live,
                    Restart => AppliesInfo::Restart {
                        pending: self.started.get(entry.key).is_some_and(|started| *started != value),
                    },
                };
                SettingInfo {
                    key: entry.key,
                    value,
                    kind,
                    value_type: entry.value_type,
                    applies,
                    editable: entry.editable,
                }
            })
            .collect()
    }
}

/// Every configuration key's value in effect: the file's, else the daemon's default.
fn values(global: &GlobalConfig, paths: &Paths) -> BTreeMap<&'static str, Value> {
    // The file's shape: the run keys at the top, each table under its name.
    let GlobalConfig {
        run,
        sandbox,
        workers,
        host,
        client,
    } = global;
    let mut merged = serde_json::to_value(run).unwrap_or_else(|_| json!({}));
    if let Value::Object(object) = &mut merged {
        for (name, table) in [
            ("sandbox", serde_json::to_value(sandbox)),
            ("workers", serde_json::to_value(workers)),
            ("host", serde_json::to_value(host)),
            ("client", serde_json::to_value(client)),
        ] {
            if let Ok(table) = table {
                object.insert(name.into(), table);
            }
        }
    }
    let defaults = DaemonConfig::defaults(paths.clone());
    CATALOG
        .iter()
        .filter_map(|entry| match entry.kind {
            Kind::State(_) => None,
            Kind::Config => {
                let value = lookup(&merged, entry.key)
                    .cloned()
                    .unwrap_or_else(|| (entry.default)(&defaults));
                Some((entry.key, value))
            }
        })
        .collect()
}

fn placement_map(placements: &Placements) -> BTreeMap<&'static str, Option<Sharing>> {
    CATALOG
        .iter()
        .filter_map(|entry| match entry.kind {
            Kind::State(_) => None,
            Kind::Config => Some((entry.key, placements.placement(&path(entry.key)))),
        })
        .collect()
}

fn path(key: &str) -> Vec<&str> {
    key.split('.').collect()
}

fn lookup<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(value, |value, part| value.get(part))
}

#[derive(Debug, thiserror::Error)]
pub enum SetError {
    #[error("unknown setting {key:?}")]
    Unknown { key: String },
    #[error("{key} can't be changed from the apps")]
    NotEditable { key: &'static str },
    #[error("{key}: expected {expected}")]
    WrongType { key: &'static str, expected: &'static str },
    #[error("{key} is this host's state; it has no Local choice")]
    NoLocal { key: &'static str },
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error(transparent)]
    State(#[from] crate::state::StateError),
}

/// A validated change.
#[derive(Debug)]
pub enum Change {
    /// A state value (`None`: back to the default).
    State(crate::state::Setting, Option<bool>),
    /// A configuration key, written to the global file pair.
    Config {
        key: &'static str,
        value: Option<Toml>,
        target: Target,
    },
}

/// Checks a change from the apps: the key exists and may change, the value has its type (`null` resets it), and
/// `local` is given only for configuration.
pub fn validate(key: &str, value: &Value, local: Option<bool>) -> Result<Change, SetError> {
    let entry = entry(key).ok_or_else(|| SetError::Unknown { key: key.to_string() })?;
    if !entry.editable {
        return Err(SetError::NotEditable { key: entry.key });
    }
    let value = match value {
        Value::Null => None,
        Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            Some(toml(entry, value)?)
        }
    };
    match entry.kind {
        Kind::State(setting) => {
            if local.is_some() {
                return Err(SetError::NoLocal { key: entry.key });
            }
            let value = value.map(|value| {
                value.as_bool().ok_or(SetError::WrongType {
                    key: entry.key,
                    expected: "true or false",
                })
            });
            Ok(Change::State(setting, value.transpose()?))
        }
        Kind::Config => Ok(Change::Config {
            key: entry.key,
            value,
            target: match local {
                None => Target::Current,
                Some(true) => Target::Move(Sharing::Local),
                Some(false) => Target::Move(Sharing::Shared),
            },
        }),
    }
}

fn toml(entry: &Entry, value: &Value) -> Result<Toml, SetError> {
    let wrong = |expected| SetError::WrongType {
        key: entry.key,
        expected,
    };
    let integer = |value: &Value| value.as_i64().filter(|n| *n >= 0);
    match entry.value_type {
        Bool => value.as_bool().map(Toml::from).ok_or(wrong("true or false")),
        Text => value.as_str().map(Toml::from).ok_or(wrong("a string")),
        TextList => value
            .as_array()
            .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
            .map(|items| Toml::Array(items.into_iter().collect()))
            .ok_or(wrong("a list of strings")),
        Integer => integer(value).map(Toml::from).ok_or(wrong("a whole number")),
        IntegerList => value
            .as_array()
            .and_then(|items| items.iter().map(integer).collect::<Option<Vec<_>>>())
            .map(|items| Toml::Array(items.into_iter().collect()))
            .ok_or(wrong("a list of whole numbers")),
        ValueType::Choice { options } => value
            .as_str()
            .filter(|choice| options.contains(choice))
            .map(Toml::from)
            .ok_or(wrong("one of the listed choices")),
        ValueType::NetworkProfile => match value {
            Value::String(name) => Ok(Toml::from(name.as_str())),
            Value::Bool(false) => Ok(Toml::from(false)),
            Value::Null | Value::Bool(true) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
                Err(wrong("a profile name, or false"))
            }
        },
    }
}

/// Writes a configuration change to the global pair.
pub fn write(paths: &Paths, key: &str, value: Option<Toml>, target: Target) -> Result<(), SetError> {
    crate::config::write::target::set_global(paths, &path(key), value.map(toml_edit::Item::Value), target)?;
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

    fn config(paths: &Paths) -> DaemonConfig {
        DaemonConfig::defaults(paths.clone())
    }

    fn info<'a>(list: &'a [SettingInfo], key: &str) -> &'a SettingInfo {
        list.iter().find(|s| s.key == key).unwrap()
    }

    #[test]
    fn every_catalog_key_is_a_key_of_its_file() {
        // Writing each key's default to an empty file must parse: the catalog can't name a key the schema lacks. And
        // the catalog shows what the file sets: every part it reads reaches the apps.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let defaults = config(&paths);
        for entry in CATALOG {
            let Kind::Config = entry.kind else { continue };
            // Not the default, so reading it back proves the file's value reached the catalog.
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
            // `toml` is what `validate` uses; calling it directly covers the non-editable `socket` too.
            let value = toml(entry, &sample).unwrap_or_else(|e| panic!("{}: {e}", entry.key));
            let pair = paths.global_files();
            let _ = std::fs::remove_file(&pair.shared);
            write(&paths, entry.key, Some(value), Target::Current).unwrap_or_else(|e| panic!("{}: {e}", entry.key));
            let mut snapshot = Snapshot::new(&defaults);
            assert!(matches!(snapshot.reload(paths.clone()), Read::Good(_)), "{}", entry.key);
            assert_eq!(info(&snapshot.info(true), entry.key).value, sample, "{}", entry.key);
        }
    }

    #[test]
    fn changes_are_typed_and_reported_with_their_placement() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.global).unwrap();
        std::fs::write(paths.global.join("config.toml"), "[host]\nroots = [\"~/a\"]\n").unwrap();
        let mut snapshot = Snapshot::new(&config(&paths));

        assert!(matches!(
            validate("nope", &json!(1), None),
            Err(SetError::Unknown { .. })
        ));
        assert!(matches!(
            validate("socket", &json!("/s"), None),
            Err(SetError::NotEditable { .. })
        ));
        assert!(matches!(
            validate("host.roots", &json!("~/a"), None),
            Err(SetError::WrongType { .. })
        ));
        assert!(matches!(
            validate("host.logging", &json!("loud"), None),
            Err(SetError::WrongType { .. })
        ));
        assert!(matches!(
            validate("keep_awake", &json!(true), Some(true)),
            Err(SetError::NoLocal { .. })
        ));
        assert!(matches!(
            validate("keep_awake", &json!(false), None),
            Ok(Change::State(crate::state::Setting::KeepAwake, Some(false)))
        ));

        let Change::Config { key, value, target } = validate("host.roots", &json!(["~/b"]), Some(true)).unwrap() else {
            unreachable!()
        };
        write(&paths, key, value, target).unwrap();
        let Change::Config { key, value, target } = validate("tailnet.port", &json!(9000), None).unwrap() else {
            unreachable!()
        };
        write(&paths, key, value, target).unwrap();
        let Read::Good(new) = snapshot.reload(paths.clone()) else {
            panic!("{:?}", snapshot.error)
        };
        assert_eq!(new.roots, ["~/b"]);

        let list = snapshot.info(true);
        let roots = info(&list, "host.roots");
        assert_eq!(roots.value, json!(["~/b"]));
        assert_eq!(
            roots.kind,
            KindInfo::Config {
                placement: Some(Sharing::Local)
            }
        );
        assert_eq!(
            info(&list, "tailnet.port").applies,
            AppliesInfo::Restart { pending: true }
        );
        assert_eq!(
            info(&list, "tailnet.enabled").applies,
            AppliesInfo::Restart { pending: false }
        );
        assert_eq!(info(&list, "host.roots").applies, AppliesInfo::Live);
        assert_eq!(info(&list, "tailnet.enabled").value, json!(true));
        assert_eq!(info(&list, "keep_awake").kind, KindInfo::State);

        // A value the schema refuses never reaches the file.
        let Change::Config { key, value, target } = validate("tailnet.port", &json!(70000), None).unwrap() else {
            unreachable!()
        };
        assert!(write(&paths, key, value, target).is_err());
        assert_eq!(
            std::fs::read_to_string(paths.global.join("config.toml")).unwrap(),
            "[tailnet]\nport = 9000\n"
        );

        // A broken hand edit keeps the last good values and says why.
        std::fs::write(paths.global.join("config.toml"), "[host\n").unwrap();
        assert!(matches!(snapshot.reload(paths.clone()), Read::Kept));
        assert!(snapshot.error.is_some());
        assert_eq!(info(&snapshot.info(true), "host.roots").value, json!(["~/b"]));
    }
}
