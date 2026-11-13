//! The settings the apps see and change: every key of the global `settings` and `daemon` files (configuration, written
//! to the files by [`crate::settings::edit`]) and the host's state values (in the state database). One catalog says,
//! for each key, where it lives, its type, whether a change applies to the next launch or needs a restart, and whether
//! the apps may change it.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::{Value, json};
use toml_edit::Value as Toml;

use crate::config::Config;
use crate::settings::edit::{EditError, FilePair, Placement, Placements, Target};
use crate::settings::{DaemonFile, Format, LevelKind, Paths, SettingsError, SettingsFile};

/// The global file a configuration key is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum File {
    Settings,
    Daemon,
}

/// Where a value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Config(File),
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
    default: fn(&Config) -> Value,
}

const fn config(
    key: &'static str,
    file: File,
    value_type: ValueType,
    applies: Applies,
    default: fn(&Config) -> Value,
) -> Entry {
    Entry {
        key,
        kind: Kind::Config(file),
        value_type,
        applies,
        editable: true,
        default,
    }
}

fn empty(_: &Config) -> Value {
    json!([])
}

fn null(_: &Config) -> Value {
    Value::Null
}

use Applies::{Live, Restart};
use File::{Daemon, Settings};
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
    config("host.roots", Settings, TextList, Live, empty),
    config("host.favorites", Settings, TextList, Live, empty),
    config(
        "host.logging",
        Settings,
        ValueType::Choice {
            options: &["info", "debug"],
        },
        Live,
        |c| json!(c.logging.mode),
    ),
    config(
        "host.attach_mode",
        Settings,
        ValueType::Choice {
            options: &["sandbox", "host"],
        },
        Live,
        |c| json!(c.attach_mode),
    ),
    config("host.empty_grace_ms", Settings, Integer, Live, |c| {
        json!(c.empty_grace_ms)
    }),
    config("client.text_font", Settings, Text, Live, null),
    config("client.code_font", Settings, Text, Live, null),
    Entry {
        key: "socket",
        kind: Kind::Config(Daemon),
        value_type: Text,
        applies: Restart,
        editable: false,
        default: |c| json!(c.socket),
    },
    config("log", Daemon, Text, Restart, |c| json!(c.log)),
    config("pi.command", Daemon, TextList, Live, |c| json!(c.pi.command)),
    config("pi.args", Daemon, TextList, Live, |c| json!(c.pi.args)),
    config("env.login_shell", Daemon, Bool, Restart, |c| json!(c.env.login_shell)),
    config("env.mise", Daemon, Bool, Restart, |c| json!(c.env.mise)),
    config("stop.grace_ms", Daemon, Integer, Live, |c| json!(c.stop.grace_ms)),
    config("stop.term_grace_ms", Daemon, Integer, Live, |c| {
        json!(c.stop.term_grace_ms)
    }),
    config("tailnet.enabled", Daemon, Bool, Restart, |c| json!(c.tailnet.enabled)),
    config("tailnet.port", Daemon, Integer, Restart, |c| json!(c.tailnet.port)),
    config("tailnet.cli", Daemon, Text, Restart, |c| json!(c.tailnet.cli)),
    config("tailnet.heartbeat_ms", Daemon, Integer, Restart, |c| {
        json!(c.tailnet.heartbeat_ms)
    }),
    config("tailnet.retry_ms", Daemon, Integer, Restart, |c| {
        json!(c.tailnet.retry_ms)
    }),
    config("sandbox.command", Daemon, TextList, Live, |c| json!(c.sandbox.command)),
    config("sandbox.base", Daemon, Text, Live, |_| {
        json!(crate::sandbox::DEFAULT_BASE)
    }),
    config("sandbox.groups", Daemon, TextList, Live, empty),
    config("sandbox.allow", Daemon, TextList, Live, empty),
    config("sandbox.read", Daemon, TextList, Live, empty),
    config("sandbox.read_file", Daemon, TextList, Live, empty),
    config("sandbox.allow_file", Daemon, TextList, Live, empty),
    config("sandbox.deny", Daemon, TextList, Live, empty),
    config("sandbox.bypass", Daemon, TextList, Live, empty),
    config("sandbox.unix_socket", Daemon, TextList, Live, empty),
    config("sandbox.network.open_port", Daemon, IntegerList, Live, empty),
    config("sandbox.network.profile", Daemon, ValueType::NetworkProfile, Live, null),
    config("workers.skip", Daemon, TextList, Live, empty),
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
        file: File,
        /// Which file of the pair sets it; `None` when neither does (the default applies).
        placement: Option<PlacementInfo>,
    },
    State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementInfo {
    Shared,
    Local,
}

/// What the files said at the last good read, and what they say now if they can't be used.
#[derive(Debug)]
pub struct Snapshot {
    /// Configuration values in effect, by key.
    values: BTreeMap<&'static str, Value>,
    placements: BTreeMap<&'static str, Option<Placement>>,
    /// Restart-only values the daemon started with.
    started: BTreeMap<&'static str, Value>,
    /// Why the files' latest version can't be used (the values are the last good ones).
    pub error: Option<String>,
}

/// The outcome of rereading the global files.
#[derive(Debug)]
pub enum Read {
    /// The new configuration.
    Good(Box<Config>),
    /// Invalid: the previous configuration stays in effect, and the snapshot's `error` says why.
    Kept,
}

impl Snapshot {
    /// Reads the global files for a daemon that runs with `config`.
    pub fn new(config: &Config) -> Self {
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
            let resolved = crate::settings::resolve_global(&paths).map_err(|e| e.to_string())?;
            let daemon = resolved.daemon.map_err(|e| e.to_string())?;
            let settings = resolved.settings.map_err(|e| e.to_string())?;
            let settings_files = files(&paths, Settings).read().map_err(|e| e.to_string())?;
            let daemon_files = files(&paths, Daemon).read().map_err(|e| e.to_string())?;
            Ok((daemon, settings, (settings_files, daemon_files)))
        };
        match read() {
            Ok((daemon, settings, placements)) => {
                let config = Config::from_files(daemon.clone(), settings.clone(), paths.clone());
                self.values = values(&daemon, &settings, &config);
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
                    Kind::Config(file) => (
                        self.values.get(entry.key).cloned().unwrap_or(Value::Null),
                        KindInfo::Config {
                            file,
                            placement: self.placements.get(entry.key).copied().flatten().map(|p| match p {
                                Placement::Shared => PlacementInfo::Shared,
                                Placement::Local => PlacementInfo::Local,
                            }),
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

fn files(paths: &Paths, file: File) -> FilePair {
    let stem = match file {
        Settings => "settings",
        Daemon => "daemon",
    };
    FilePair {
        shared: paths.global.join(format!("{stem}.toml")),
        local: paths.global.join(format!("{stem}.local.toml")),
    }
}

/// Every configuration key's value in effect.
fn values(daemon: &DaemonFile, settings: &SettingsFile, config: &Config) -> BTreeMap<&'static str, Value> {
    let daemon = serde_json::to_value(daemon).unwrap_or(Value::Null);
    let settings = serde_json::to_value(settings).unwrap_or(Value::Null);
    let defaults = Config::from_files(DaemonFile::default(), SettingsFile::default(), config.paths.clone());
    CATALOG
        .iter()
        .filter_map(|entry| {
            let Kind::Config(file) = entry.kind else { return None };
            let source = match file {
                Settings => &settings,
                Daemon => &daemon,
            };
            let value = lookup(source, entry.key)
                .cloned()
                .unwrap_or_else(|| (entry.default)(&defaults));
            Some((entry.key, value))
        })
        .collect()
}

fn placement_map((settings, daemon): &(Placements, Placements)) -> BTreeMap<&'static str, Option<Placement>> {
    CATALOG
        .iter()
        .filter_map(|entry| {
            let Kind::Config(file) = entry.kind else { return None };
            let placements = match file {
                Settings => settings,
                Daemon => daemon,
            };
            Some((entry.key, placements.placement(&path(entry.key))))
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
    /// A configuration key, written to its file pair.
    Config {
        file: File,
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
        Kind::Config(file) => Ok(Change::Config {
            file,
            key: entry.key,
            value,
            target: match local {
                None => Target::Current,
                Some(true) => Target::Move(Placement::Local),
                Some(false) => Target::Move(Placement::Shared),
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

/// Writes a configuration change, refusing a result the daemon wouldn't accept.
pub fn write(paths: &Paths, file: File, key: &str, value: Option<Toml>, target: Target) -> Result<(), SetError> {
    let validate = move |text: &str, path: &Path| -> Result<(), SettingsError> {
        match file {
            Settings => crate::settings::parse::<SettingsFile>(text, path, LevelKind::Global, Format::Toml).map(drop),
            Daemon => {
                let parsed = crate::settings::parse::<DaemonFile>(text, path, LevelKind::Global, Format::Toml)?;
                crate::settings::validate_daemon(&parsed, path)
            }
        }
    };
    files(paths, file).set(&path(key), value, target, &validate)?;
    Ok(())
}

/// Every path whose change can change the global configuration: the global folder, and the folders of the files'
/// link targets (a dotfiles repository).
pub fn watched(paths: &Paths) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![paths.global.clone()];
    for name in [
        "settings.toml",
        "settings.local.toml",
        "daemon.toml",
        "daemon.local.toml",
    ] {
        let path = paths.global.join(name);
        if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
            && let Ok(target) = std::fs::canonicalize(&path)
            && let Some(dir) = target.parent()
            && !dirs.iter().any(|d| d == dir)
        {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        Paths {
            home: dir.to_path_buf(),
            global: dir.join("piton"),
            pi_agent_dir: dir.join("agent"),
        }
    }

    fn config(paths: &Paths) -> Config {
        Config::from_files(DaemonFile::default(), SettingsFile::default(), paths.clone())
    }

    fn info<'a>(list: &'a [SettingInfo], key: &str) -> &'a SettingInfo {
        list.iter().find(|s| s.key == key).unwrap()
    }

    #[test]
    fn every_catalog_key_is_a_key_of_its_file() {
        // Writing each key's default to an empty file must parse: the catalog can't name a key the schema lacks.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let defaults = config(&paths);
        for entry in CATALOG {
            let Kind::Config(file) = entry.kind else { continue };
            let sample = match entry.value_type {
                ValueType::NetworkProfile => json!(false),
                Bool | Text | TextList | Integer | IntegerList | ValueType::Choice { .. } => {
                    let default = (entry.default)(&defaults);
                    match (entry.value_type, default) {
                        (Text, Value::Null) => json!("x"),
                        (_, default) => default,
                    }
                }
            };
            // `toml` is what `validate` uses; calling it directly covers the non-editable `socket` too.
            let value = toml(entry, &sample).unwrap_or_else(|e| panic!("{}: {e}", entry.key));
            let pair = files(&paths, file);
            let _ = std::fs::remove_file(&pair.shared);
            write(&paths, file, entry.key, Some(value), Target::Current)
                .unwrap_or_else(|e| panic!("{}: {e}", entry.key));
        }
    }

    #[test]
    fn changes_are_typed_and_reported_with_their_placement() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.global).unwrap();
        std::fs::write(paths.global.join("settings.toml"), "[host]\nroots = [\"~/a\"]\n").unwrap();
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

        let Change::Config {
            file,
            key,
            value,
            target,
        } = validate("host.roots", &json!(["~/b"]), Some(true)).unwrap()
        else {
            unreachable!()
        };
        write(&paths, file, key, value, target).unwrap();
        let Change::Config {
            file,
            key,
            value,
            target,
        } = validate("tailnet.port", &json!(9000), None).unwrap()
        else {
            unreachable!()
        };
        write(&paths, file, key, value, target).unwrap();
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
                file: Settings,
                placement: Some(PlacementInfo::Local)
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
        let Change::Config {
            file,
            key,
            value,
            target,
        } = validate("tailnet.port", &json!(70000), None).unwrap()
        else {
            unreachable!()
        };
        assert!(write(&paths, file, key, value, target).is_err());
        assert_eq!(
            std::fs::read_to_string(paths.global.join("daemon.toml")).unwrap(),
            "[tailnet]\nport = 9000\n"
        );

        // A broken hand edit keeps the last good values and says why.
        std::fs::write(paths.global.join("settings.toml"), "[host\n").unwrap();
        assert!(matches!(snapshot.reload(paths.clone()), Read::Kept));
        assert!(snapshot.error.is_some());
        assert_eq!(info(&snapshot.info(true), "host.roots").value, json!(["~/b"]));
    }
}
