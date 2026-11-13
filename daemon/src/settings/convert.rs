//! Converting files of earlier layouts into tables of `config.toml`: nono profiles into `[sandbox]`, `.piton.toml`
//! into `[workers]`, and the separate files of a `.piton/` folder (`daemon.toml`, `settings.toml`, `permissions*.json`,
//! `workflows.json`, `integrations.json`, `web-search.json`) into their tables, with JSON's camelCase keys in
//! snake_case. Every key is mapped explicitly; one the current file doesn't model is refused by name, never dropped.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::Runtime;
use super::files::{NetworkProfile, SandboxTable, WorkersTable};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConvertError {
    #[error("{path}: `{key}` has no equivalent in piton's configuration; convert it by hand or remove it")]
    Unmodeled { path: PathBuf, key: String },
    #[error("{path}: `{key}` belongs in the global tier's config.toml [sandbox]; move it there by hand")]
    GlobalOnly { path: PathBuf, key: String },
    #[error("invalid {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

/// A project's nono overlay (`.nono.json`) as a `[sandbox]` table: only grants, since the base profile, groups, bypasses,
/// and the network profile belong to the global tier.
pub fn nono_profile(text: &str, path: &Path) -> Result<SandboxTable, ConvertError> {
    let invalid = |reason: String| ConvertError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let unmodeled = |key: String| ConvertError::Unmodeled {
        path: path.to_path_buf(),
        key,
    };
    let global_only = |key: &str| -> Result<(), ConvertError> {
        Err(ConvertError::GlobalOnly {
            path: path.to_path_buf(),
            key: key.to_string(),
        })
    };
    let value: Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
    let Value::Object(profile) = value else {
        return Err(invalid("must be a JSON object".into()));
    };
    let mut table = SandboxTable::default();
    for (key, value) in profile {
        match key.as_str() {
            // A description only.
            "meta" => {}
            "extends" => {
                global_only("extends")?;
                table.base = Some(string(&value, "extends").map_err(invalid)?);
            }
            "groups" => {
                global_only("groups")?;
                for (key, value) in object(&value, "groups").map_err(invalid)? {
                    match key.as_str() {
                        "include" => table.groups = strings(value, "groups.include").map_err(invalid)?,
                        _ => return Err(unmodeled(format!("groups.{key}"))),
                    }
                }
            }
            // What piton's generated profile always sets.
            "workdir" => {
                for (key, value) in object(&value, "workdir").map_err(invalid)? {
                    match (key.as_str(), value.as_str()) {
                        ("access", Some("readwrite")) => {}
                        _ => return Err(unmodeled(format!("workdir.{key}"))),
                    }
                }
            }
            "filesystem" => {
                for (key, value) in object(&value, "filesystem").map_err(invalid)? {
                    let name = format!("filesystem.{key}");
                    let paths = strings(value, &name).map_err(invalid)?;
                    let list = match key.as_str() {
                        "allow" => &mut table.allow,
                        "read" => &mut table.read,
                        "read_file" => &mut table.read_file,
                        "allow_file" => &mut table.allow_file,
                        "deny" => &mut table.deny,
                        "unix_socket" => &mut table.unix_socket,
                        "bypass_protection" => {
                            global_only(&name)?;
                            &mut table.bypass
                        }
                        _ => return Err(unmodeled(name)),
                    };
                    list.extend(paths);
                }
            }
            "network" => {
                for (key, value) in object(&value, "network").map_err(invalid)? {
                    match key.as_str() {
                        "open_port" => {
                            table.network.open_port = ports(value).map_err(invalid)?;
                        }
                        "network_profile" => {
                            global_only("network.network_profile")?;
                            table.network.profile = Some(match value {
                                Value::Null => NetworkProfile::Off(false),
                                Value::String(name) => NetworkProfile::Named(name.clone()),
                                Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
                                    return Err(invalid("network.network_profile: a name or null".into()));
                                }
                            });
                        }
                        _ => return Err(unmodeled(format!("network.{key}"))),
                    }
                }
            }
            // The daemon sets PITON_RUNTIME for every sandboxed pi itself; nothing else is modeled.
            "environment" => {
                for (key, value) in object(&value, "environment").map_err(invalid)? {
                    if key != "set_vars" {
                        return Err(unmodeled(format!("environment.{key}")));
                    }
                    for (name, value) in object(value, "environment.set_vars").map_err(invalid)? {
                        match (name.as_str(), value.as_str()) {
                            ("PITON_RUNTIME", Some("sandbox")) => {}
                            _ => return Err(unmodeled(format!("environment.set_vars.{name}"))),
                        }
                    }
                }
            }
            _ => return Err(unmodeled(key)),
        }
    }
    Ok(table)
}

/// A source checkout's `.piton.toml`: only `[workers] skip`.
pub fn project_toml(text: &str, path: &Path) -> Result<WorkersTable, ConvertError> {
    let table: toml::Table = toml::from_str(text).map_err(|e| ConvertError::Invalid {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let mut workers = WorkersTable::default();
    for (key, value) in table {
        match (key.as_str(), value) {
            ("workers", toml::Value::Table(table)) => {
                for (key, value) in table {
                    match key.as_str() {
                        "skip" => {
                            workers.skip = value.try_into().map_err(|e: toml::de::Error| ConvertError::Invalid {
                                path: path.to_path_buf(),
                                reason: format!("workers.skip: {e}"),
                            })?;
                        }
                        _ => {
                            return Err(ConvertError::Unmodeled {
                                path: path.to_path_buf(),
                                key: format!("workers.{key}"),
                            });
                        }
                    }
                }
            }
            (key, _) => {
                return Err(ConvertError::Unmodeled {
                    path: path.to_path_buf(),
                    key: key.to_string(),
                });
            }
        }
    }
    Ok(workers)
}

/// A separate settings file of the layout before `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeparateFile {
    /// `daemon.toml`: already in the current tables.
    Daemon,
    /// `settings.toml`: `[host]` and `[client]`.
    Settings,
    /// `permissions.json`, or `permissions.<runtime>.json`.
    Permissions(Option<Runtime>),
    Workflows,
    Integrations,
    WebSearch,
}

impl SeparateFile {
    /// The file's kind and whether it's a `.local` file, from its name.
    pub fn from_name(name: &str) -> Option<(SeparateFile, bool)> {
        let (stem, local) = match name
            .strip_suffix(".local.toml")
            .or_else(|| name.strip_suffix(".local.json"))
        {
            Some(stem) => (stem, true),
            None => (
                name.strip_suffix(".toml").or_else(|| name.strip_suffix(".json"))?,
                false,
            ),
        };
        let toml = name.ends_with(".toml");
        let kind = match (stem, toml) {
            ("daemon", true) => SeparateFile::Daemon,
            ("settings", true) => SeparateFile::Settings,
            ("permissions", false) => SeparateFile::Permissions(None),
            ("permissions.host", false) => SeparateFile::Permissions(Some(Runtime::Host)),
            ("permissions.sandbox", false) => SeparateFile::Permissions(Some(Runtime::Sandbox)),
            ("workflows", false) => SeparateFile::Workflows,
            ("integrations", false) => SeparateFile::Integrations,
            ("web-search", false) => SeparateFile::WebSearch,
            _ => return None,
        };
        Some((kind, local))
    }
}

/// A file's tables: those for `config.toml`, those for `config.local.toml` (personal keys), and notes for the user
/// about keys that no longer exist.
#[derive(Debug, Default, PartialEq)]
pub struct Tables {
    pub shared: toml::Table,
    pub local: toml::Table,
    pub notes: Vec<String>,
}

/// One separate file as tables of `config.toml`. `local`: it's a `.local` file, so everything goes to
/// `config.local.toml`.
pub fn separate_file(kind: SeparateFile, text: &str, path: &Path, local: bool) -> Result<Tables, ConvertError> {
    let invalid = |reason: String| ConvertError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let unmodeled = |key: String| ConvertError::Unmodeled {
        path: path.to_path_buf(),
        key,
    };
    let json = || -> Result<serde_json::Map<String, Value>, ConvertError> {
        match serde_json::from_str(text).map_err(|e| invalid(e.to_string()))? {
            Value::Object(object) => Ok(object),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => {
                Err(invalid("must be a JSON object".into()))
            }
        }
    };
    let mut out = Tables::default();
    let mut shared = toml::Table::new();
    let mut personal = toml::Table::new();
    match kind {
        SeparateFile::Daemon | SeparateFile::Settings => {
            shared = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
        }
        SeparateFile::Permissions(runtime) => {
            let table = toml_table(&Value::Object(json()?), path, "")?;
            let table = match runtime {
                None => table,
                Some(runtime) => {
                    let name = match runtime {
                        Runtime::Host => "host",
                        Runtime::Sandbox => "sandbox",
                    };
                    toml::Table::from_iter([(name.to_string(), toml::Value::Table(table))])
                }
            };
            shared.insert("permissions".into(), toml::Value::Table(table));
        }
        SeparateFile::Workflows => {
            for (key, value) in json()? {
                match key.as_str() {
                    "branches" => {
                        let mut branches = toml_table(&value, path, "branches")?;
                        // Personal: config.local.toml only.
                        if let Some(username) = branches.remove("username") {
                            personal.insert(
                                "branches".into(),
                                toml::Value::Table(toml::Table::from_iter([("username".to_string(), username)])),
                            );
                        }
                        if !branches.is_empty() {
                            shared.insert("branches".into(), toml::Value::Table(branches));
                        }
                    }
                    "sessionAnalysis" => {
                        let on = value.as_bool().ok_or_else(|| invalid("sessionAnalysis must be true or false".into()))?;
                        shared.insert(
                            "development".into(),
                            toml::Value::Table(toml::Table::from_iter([(
                                "session_analysis".to_string(),
                                toml::Value::Boolean(on),
                            )])),
                        );
                    }
                    // Removed by the bounded-reviews change (#196); the workflows already ignore it.
                    "reviews" => out.notes.push(format!(
                        "{}: reviews is gone (reviews are bounded now); dropped",
                        path.display()
                    )),
                    "workflowProject" => out.notes.push(format!(
                        "{}: workflowProject is gone: a development install (the pi package running from a git checkout of piton) reports to that checkout",
                        path.display()
                    )),
                    _ => return Err(unmodeled(key)),
                }
            }
        }
        SeparateFile::Integrations => {
            for (key, value) in json()? {
                match key.as_str() {
                    "tracker" | "forge" => {
                        shared.insert(key.clone(), toml::Value::Table(toml_table(&value, path, &key)?));
                    }
                    _ => return Err(unmodeled(key)),
                }
            }
        }
        SeparateFile::WebSearch => {
            shared.insert(
                "web_search".into(),
                toml::Value::Table(toml_table(&Value::Object(json()?), path, "")?),
            );
        }
    }
    if local {
        merge_tables(&mut shared, personal);
        out.local = shared;
    } else {
        out.shared = shared;
        out.local = personal;
    }
    Ok(out)
}

/// Adds `from`'s keys to `into`: tables both have merge, lists both have combine (each item once), other values
/// from `from` win. Lists only add, as the sandbox grants and permission rules they hold do across levels.
pub fn merge_tables(into: &mut toml::Table, from: toml::Table) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(value)) => merge_tables(existing, value),
            (Some(toml::Value::Array(existing)), toml::Value::Array(value)) => {
                for item in value {
                    if !existing.contains(&item) {
                        existing.push(item);
                    }
                }
            }
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

/// A JSON object as a TOML table, its camelCase keys in snake_case.
fn toml_table(value: &Value, path: &Path, key: &str) -> Result<toml::Table, ConvertError> {
    match toml_value(value, path, key)? {
        toml::Value::Table(table) => Ok(table),
        toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_)
        | toml::Value::Array(_) => Err(ConvertError::Invalid {
            path: path.to_path_buf(),
            reason: format!("{key} must be an object"),
        }),
    }
}

fn toml_value(value: &Value, path: &Path, key: &str) -> Result<toml::Value, ConvertError> {
    Ok(match value {
        Value::Null => {
            return Err(ConvertError::Invalid {
                path: path.to_path_buf(),
                reason: format!("{key}: null has no TOML form; remove it"),
            });
        }
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => toml::Value::Integer(i),
            (None, Some(f)) => toml::Value::Float(f),
            (None, None) => {
                return Err(ConvertError::Invalid {
                    path: path.to_path_buf(),
                    reason: format!("{key}: {n} doesn't fit TOML"),
                });
            }
        },
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => toml::Value::Array(
            items
                .iter()
                .map(|item| toml_value(item, path, key))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(object) => {
            let mut table = toml::Table::new();
            for (name, value) in object {
                let inner = if key.is_empty() {
                    name.clone()
                } else {
                    format!("{key}.{name}")
                };
                table.insert(snake_case(name), toml_value(value, path, &inner)?);
            }
            toml::Value::Table(table)
        }
    })
}

/// `mcpServer` -> `mcp_server`. Keys already lowercase stay as they are.
pub fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn object<'a>(value: &'a Value, key: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    value.as_object().ok_or_else(|| format!("{key} must be an object"))
}

fn string(value: &Value, key: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn strings(value: &Value, key: &str) -> Result<Vec<String>, String> {
    let list = value.as_array().ok_or_else(|| format!("{key} must be a list"))?;
    list.iter().map(|item| string(item, key)).collect()
}

fn ports(value: &Value) -> Result<Vec<u16>, String> {
    let list = value.as_array().ok_or("network.open_port must be a list")?;
    list.iter()
        .map(|port| {
            port.as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .ok_or_else(|| "network.open_port: ports are 0-65535".to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAY: &str = r#"{
        "meta": {"name": "repo", "description": "d"},
        "workdir": {"access": "readwrite"},
        "filesystem": {
            "allow": ["$HOME/.cache/x"],
            "read": ["$HOME/.config"],
            "deny": ["$HOME/secrets.sh"],
            "read_file": ["$HOME/.ssh/config"],
            "unix_socket": ["$SSH_AUTH_SOCK"]
        },
        "network": {"open_port": [0]},
        "environment": {"set_vars": {"PITON_RUNTIME": "sandbox"}}
    }"#;

    /// A project overlay, converted to `[sandbox]`, generates a nono profile with the same grants (the daemon expands
    /// `$HOME` and `$VAR`s itself, so paths compare expanded).
    #[test]
    fn an_overlay_round_trips_through_the_generated_profile() {
        let table = nono_profile(OVERLAY, Path::new(".nono.json")).unwrap();
        assert_eq!(table.network.open_port, [0]);
        assert_eq!(table.network.profile, None::<NetworkProfile>);
        let settings = crate::settings::DaemonSettings {
            daemon: crate::settings::DaemonFile {
                sandbox: table,
                ..Default::default()
            },
            source: PathBuf::from("/nonexistent/repo"),
            checkout: PathBuf::from("/nonexistent/repo"),
            levels: Vec::new(),
        };
        let env = [("SSH_AUTH_SOCK".to_string(), "/tmp/agent.sock".to_string())]
            .into_iter()
            .collect();
        let generated =
            crate::sandbox::profile(&settings, Path::new("/Users/a"), &env, Path::new("/s/agent.sock")).unwrap();
        let original: Value = serde_json::from_str(OVERLAY).unwrap();
        let expand = |value: &Value| -> Value {
            let text = value
                .to_string()
                .replace("$HOME", "/Users/a")
                .replace("$SSH_AUTH_SOCK", "/tmp/agent.sock");
            serde_json::from_str(&text).unwrap()
        };
        assert_eq!(generated["workdir"], original["workdir"]);
        assert_eq!(generated["network"]["open_port"], original["network"]["open_port"]);
        for key in ["allow", "read", "read_file", "deny"] {
            assert_eq!(
                generated["filesystem"][key],
                expand(&original["filesystem"][key]),
                "{key}"
            );
        }
        // The profile's own socket grants, then the daemon's socket for sandboxed pis.
        let mut sockets = expand(&original["filesystem"]["unix_socket"]);
        sockets.as_array_mut().unwrap().push(Value::from("/s/agent.sock"));
        assert_eq!(generated["filesystem"]["unix_socket"], sockets);
    }

    #[test]
    fn unmodeled_and_global_only_keys_are_refused_by_name() {
        let path = Path::new(".nono.json");
        for (text, key) in [
            (r#"{"filesystem":{"write":["/x"]}}"#, "filesystem.write"),
            (
                r#"{"environment":{"set_vars":{"FOO":"1"}}}"#,
                "environment.set_vars.FOO",
            ),
            (r#"{"workdir":{"access":"read"}}"#, "workdir.access"),
            (r#"{"security":{}}"#, "security"),
        ] {
            assert_eq!(
                nono_profile(text, path),
                Err(ConvertError::Unmodeled {
                    path: path.into(),
                    key: key.into()
                })
            );
        }
        for (text, key) in [
            (r#"{"extends":"default"}"#, "extends"),
            (
                r#"{"filesystem":{"bypass_protection":["/x"]}}"#,
                "filesystem.bypass_protection",
            ),
            (r#"{"network":{"network_profile":"x"}}"#, "network.network_profile"),
        ] {
            assert_eq!(
                nono_profile(text, path),
                Err(ConvertError::GlobalOnly {
                    path: path.into(),
                    key: key.into()
                })
            );
        }
        // A project overlay's grants convert.
        let table = nono_profile(r#"{"filesystem":{"allow":["$HOME/a"],"read_file":["$HOME/b"]}}"#, path).unwrap();
        assert_eq!(
            (table.allow, table.read_file),
            (vec!["$HOME/a".into()], vec!["$HOME/b".into()])
        );
    }

    #[test]
    fn separate_files_convert() {
        let p = Path::new("f");
        let t = separate_file(
            SeparateFile::Integrations,
            r#"{"tracker":{"provider":"github","mcpServer":"gh","fields":{"status":{"values":{"inProgress":"In Progress"}}},"labels":{"groups":{"pi-x":{"appliesTo":["epic"]}}}}}"#,
            p,
            false,
        )
        .unwrap();
        let tracker = t.shared["tracker"].as_table().unwrap();
        assert_eq!(tracker["mcp_server"].as_str(), Some("gh"));
        assert_eq!(
            tracker["fields"]["status"]["values"]["in_progress"].as_str(),
            Some("In Progress")
        );
        assert!(
            tracker["labels"]["groups"]["pi-x"].get("applies_to").is_some(),
            "label keys stay"
        );

        let t = separate_file(
            SeparateFile::Workflows,
            r#"{"branches":{"format":"tracker","username":"me"},"sessionAnalysis":true,"workflowProject":"~/p","reviews":{"userConfirmationAfter":5}}"#,
            p,
            false,
        )
        .unwrap();
        assert_eq!(t.shared["branches"]["format"].as_str(), Some("tracker"));
        assert_eq!(
            t.local["branches"]["username"].as_str(),
            Some("me"),
            "the username is personal"
        );
        assert_eq!(t.shared["development"]["session_analysis"].as_bool(), Some(true));
        assert_eq!(t.notes.len(), 2);

        let t = separate_file(
            SeparateFile::Permissions(Some(Runtime::Sandbox)),
            r#"{"defaultMode":"allow","ask":["write"]}"#,
            p,
            true,
        )
        .unwrap();
        assert!(t.shared.is_empty());
        assert_eq!(
            t.local["permissions"]["sandbox"]["default_mode"].as_str(),
            Some("allow")
        );

        assert_eq!(
            SeparateFile::from_name("permissions.host.local.json"),
            Some((SeparateFile::Permissions(Some(Runtime::Host)), true))
        );
        assert_eq!(SeparateFile::from_name("mcp.json"), None);
        assert!(matches!(
            separate_file(SeparateFile::Workflows, r#"{"x":1}"#, p, false),
            Err(ConvertError::Unmodeled { .. })
        ));
        assert!(separate_file(SeparateFile::Integrations, r#"{"tracker":{"a":null}}"#, p, false).is_err());
    }

    #[test]
    fn project_toml_converts() {
        let workers = project_toml("[workers]\nskip = [\"a/b\"]\n", Path::new(".piton.toml")).unwrap();
        assert_eq!(workers.skip, ["a/b"]);
        assert!(matches!(
            project_toml("[other]\n", Path::new(".piton.toml")),
            Err(ConvertError::Unmodeled { .. })
        ));
    }
}
