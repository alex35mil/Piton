//! Converting the files of the layout before `.piton/` into the current ones: nono profiles into `[sandbox]`,
//! `.piton.toml` into `[workers]`, and the global `config.toml` into `daemon.toml` and `settings.toml`. Every key is
//! mapped explicitly; one the current files don't model is refused by name, never dropped.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::files::{DaemonFile, HostPrefs, NetworkProfile, SandboxTable, SettingsFile, WorkersTable};

/// Where a converted table goes: the global tier may set every `[sandbox]` key, a project only the grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConvertError {
    #[error("{path}: `{key}` has no equivalent in piton's configuration; convert it by hand or remove it")]
    Unmodeled { path: PathBuf, key: String },
    #[error("{path}: `{key}` belongs in the global tier's daemon.toml [sandbox]; move it there by hand")]
    GlobalOnly { path: PathBuf, key: String },
    #[error("invalid {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

/// A nono profile (a project's `.nono.json` overlay or the user's base profile) as a `[sandbox]` table.
pub fn nono_profile(text: &str, path: &Path, scope: Scope) -> Result<SandboxTable, ConvertError> {
    let invalid = |reason: String| ConvertError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let unmodeled = |key: String| ConvertError::Unmodeled {
        path: path.to_path_buf(),
        key,
    };
    let global_only = |key: &str| -> Result<(), ConvertError> {
        match scope {
            Scope::Global => Ok(()),
            Scope::Project => Err(ConvertError::GlobalOnly {
                path: path.to_path_buf(),
                key: key.to_string(),
            }),
        }
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

/// The old global `config.toml`, converted.
#[derive(Debug)]
pub struct OldConfig {
    pub daemon: DaemonFile,
    pub settings: SettingsFile,
    /// `keep_awake`, now state in the host's database: the user sets it in the apps.
    pub keep_awake: Option<bool>,
}

/// The global `config.toml` split into `daemon.toml` (how the daemon runs) and `settings.toml` (`[host]`
/// preferences). Only keys the old file set are carried over, so the current defaults still apply to the rest.
pub fn old_config(text: &str, path: &Path) -> Result<OldConfig, ConvertError> {
    let invalid = |reason: String| ConvertError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let unmodeled = |key: String| ConvertError::Unmodeled {
        path: path.to_path_buf(),
        key,
    };
    let table: toml::Table = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;
    let mut daemon = toml::Table::new();
    let mut host = HostPrefs::default();
    let mut keep_awake = None;
    for (key, value) in table {
        match key.as_str() {
            "socket" | "log" | "pi" | "env" | "stop" | "tailnet" => {
                daemon.insert(key, value);
            }
            "keep_awake" => keep_awake = Some(field(value, "keep_awake").map_err(invalid)?),
            "sandbox" => {
                let toml::Value::Table(sandbox) = value else {
                    return Err(invalid("sandbox must be a table".into()));
                };
                let mut converted = toml::Table::new();
                for (key, value) in sandbox {
                    match key.as_str() {
                        "command" => {
                            converted.insert(key, value);
                        }
                        "profile" => {
                            converted.insert("base".into(), value);
                        }
                        _ => return Err(unmodeled(format!("sandbox.{key}"))),
                    }
                }
                daemon.insert("sandbox".into(), toml::Value::Table(converted));
            }
            "roots" => host.roots = Some(field(value, "roots").map_err(invalid)?),
            "attach_mode" => host.attach_mode = Some(field(value, "attach_mode").map_err(invalid)?),
            "empty_grace_ms" => host.empty_grace_ms = Some(field(value, "empty_grace_ms").map_err(invalid)?),
            "logging" => {
                let toml::Value::Table(logging) = value else {
                    return Err(invalid("logging must be a table".into()));
                };
                for (key, value) in logging {
                    match key.as_str() {
                        "mode" => host.logging = Some(field(value, "logging.mode").map_err(invalid)?),
                        _ => return Err(unmodeled(format!("logging.{key}"))),
                    }
                }
            }
            _ => return Err(unmodeled(key)),
        }
    }
    let daemon: DaemonFile = toml::Value::Table(daemon)
        .try_into()
        .map_err(|e: toml::de::Error| invalid(e.to_string()))?;
    Ok(OldConfig {
        daemon,
        settings: SettingsFile {
            host,
            ..SettingsFile::default()
        },
        keep_awake,
    })
}

fn field<T: serde::de::DeserializeOwned>(value: toml::Value, key: &str) -> Result<T, String> {
    value.try_into().map_err(|e: toml::de::Error| format!("{key}: {e}"))
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

    const BASE: &str = r#"{
        "meta": {"name": "piton", "description": "d"},
        "extends": "mise-dev",
        "groups": {"include": ["git_config"]},
        "workdir": {"access": "readwrite"},
        "filesystem": {
            "allow": ["$HOME/.config/pi/agent", "$HOME/Library/Keychains"],
            "read": ["$HOME/.config"],
            "deny": ["$HOME/secrets.sh"],
            "read_file": ["$HOME/.ssh/config"],
            "unix_socket": ["$SSH_AUTH_SOCK"],
            "bypass_protection": ["$HOME/Library/Keychains", "$HOME/.ssh/config"]
        },
        "network": {"network_profile": null, "open_port": [0]},
        "environment": {"set_vars": {"PITON_RUNTIME": "sandbox"}}
    }"#;

    #[test]
    fn the_base_profile_converts() {
        let table = nono_profile(BASE, Path::new("piton.json"), Scope::Global).unwrap();
        assert_eq!(table.base.as_deref(), Some("mise-dev"));
        assert_eq!(table.groups, ["git_config"]);
        assert_eq!(table.allow, ["$HOME/.config/pi/agent", "$HOME/Library/Keychains"]);
        assert_eq!(table.bypass, ["$HOME/Library/Keychains", "$HOME/.ssh/config"]);
        assert_eq!(table.unix_socket, ["$SSH_AUTH_SOCK"]);
        assert_eq!(table.network.open_port, [0]);
        assert_eq!(table.network.profile, Some(NetworkProfile::Off(false)));
    }

    /// The base profile, converted to `[sandbox]`, generates a nono profile with the same settings (the daemon expands
    /// `$HOME` and `$VAR`s itself, so paths compare expanded).
    #[test]
    fn the_base_profile_round_trips_through_the_generated_profile() {
        let table = nono_profile(BASE, Path::new("piton.json"), Scope::Global).unwrap();
        let settings = crate::settings::DaemonSettings {
            daemon: crate::settings::DaemonFile {
                sandbox: table,
                ..Default::default()
            },
            source: PathBuf::from("/nonexistent/repo"),
            levels: Vec::new(),
        };
        let env = [("SSH_AUTH_SOCK".to_string(), "/tmp/agent.sock".to_string())]
            .into_iter()
            .collect();
        let generated = crate::sandbox::profile(&settings, Path::new("/Users/a"), &env).unwrap();
        let original: Value = serde_json::from_str(BASE).unwrap();
        let expand = |value: &Value| -> Value {
            let text = value
                .to_string()
                .replace("$HOME", "/Users/a")
                .replace("$SSH_AUTH_SOCK", "/tmp/agent.sock");
            serde_json::from_str(&text).unwrap()
        };
        assert_eq!(generated["extends"], original["extends"]);
        assert_eq!(generated["groups"], original["groups"]);
        assert_eq!(generated["workdir"], original["workdir"]);
        assert_eq!(generated["network"], original["network"]);
        for key in ["allow", "read", "read_file", "deny", "unix_socket", "bypass_protection"] {
            assert_eq!(
                generated["filesystem"][key],
                expand(&original["filesystem"][key]),
                "{key}"
            );
        }
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
                nono_profile(text, path, Scope::Global),
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
                nono_profile(text, path, Scope::Project),
                Err(ConvertError::GlobalOnly {
                    path: path.into(),
                    key: key.into()
                })
            );
        }
        // A project overlay's grants convert.
        let table = nono_profile(
            r#"{"filesystem":{"allow":["$HOME/a"],"read_file":["$HOME/b"]}}"#,
            path,
            Scope::Project,
        )
        .unwrap();
        assert_eq!(
            (table.allow, table.read_file),
            (vec!["$HOME/a".into()], vec!["$HOME/b".into()])
        );
    }

    #[test]
    fn project_toml_and_old_config_convert() {
        let workers = project_toml("[workers]\nskip = [\"a/b\"]\n", Path::new(".piton.toml")).unwrap();
        assert_eq!(workers.skip, ["a/b"]);
        assert!(matches!(
            project_toml("[other]\n", Path::new(".piton.toml")),
            Err(ConvertError::Unmodeled { .. })
        ));

        let OldConfig {
            daemon,
            settings,
            keep_awake,
        } = old_config(
            "roots = [\"~/Projects\"]\nkeep_awake = false\nattach_mode = \"host\"\n[sandbox]\nprofile = \"mine\"\ncommand = [\"/bin/nono\"]\n[logging]\nmode = \"debug\"\n[stop]\ngrace_ms = 5\n",
            Path::new("config.toml"),
        )
        .unwrap();
        assert_eq!(keep_awake, Some(false));
        assert_eq!(daemon.sandbox.base.as_deref(), Some("mine"));
        assert_eq!(daemon.sandbox.command.as_deref(), Some(&["/bin/nono".to_string()][..]));
        assert_eq!(daemon.stop.grace_ms, Some(5));
        assert_eq!(settings.host.roots.as_deref(), Some(&["~/Projects".to_string()][..]));
        assert_eq!(settings.host.logging, Some(crate::diagnostics::Mode::Debug));
        assert_eq!(settings.host.attach_mode, Some(crate::wire::LaunchMode::Host));
        assert!(matches!(
            old_config("unknown = 1\n", Path::new("config.toml")),
            Err(ConvertError::Unmodeled { .. })
        ));
    }
}
