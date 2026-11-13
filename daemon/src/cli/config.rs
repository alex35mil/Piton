//! `piton config resolve|get|set|check`: the resolved configuration for a workspace, for the pi package, the Neovim
//! plugin, and the user, and changes to it. Reads and writes files only; the daemon needn't run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use super::ConfigAction;
use crate::config::{ConfigError, Paths, Resolved, Runtime};

/// Version of `piton config resolve --json`. Clients refuse a version they don't know.
pub const RESOLVE_FORMAT: u32 = 3;

/// Exit code for invalid configuration, so callers can tell it from a failure to run.
const EXIT_INVALID: u8 = 3;

#[derive(Serialize)]
struct Output<'a> {
    format: u32,
    #[serde(flatten)]
    resolved: &'a Resolved,
}

#[derive(Serialize)]
struct ErrorOutput {
    format: u32,
    error: ErrorBody,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ErrorBody {
    /// The configuration is invalid: fix it, then try again.
    Invalid { message: String },
    /// The workspace isn't a folder, or a worker's layout is wrong.
    Workspace { message: String },
}

pub fn run(action: &ConfigAction) -> ExitCode {
    match action {
        ConfigAction::Resolve {
            workspace,
            runtime,
            global: true,
        } => {
            let _ = (workspace, runtime);
            match crate::config::resolve_global(&Paths::current()) {
                Ok(resolved) => {
                    let output = Output {
                        format: RESOLVE_FORMAT,
                        resolved: &resolved,
                    };
                    println!("{}", serde_json::to_string(&output).unwrap_or_default());
                    ExitCode::SUCCESS
                }
                Err(e) => print_error(ErrorBody::Invalid { message: e.to_string() }),
            }
        }
        ConfigAction::Resolve {
            workspace,
            runtime,
            global: false,
        } => {
            let runtime = match runtime.as_deref() {
                None => runtime_from_env(),
                Some("host") => Ok(Runtime::Host),
                Some("sandbox") => Ok(Runtime::Sandbox),
                Some(other) => Err(format!("unknown runtime {other:?}: host or sandbox")),
            };
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(message) => return print_error(ErrorBody::Invalid { message }),
            };
            match resolve(workspace.as_deref(), runtime) {
                Ok(resolved) => {
                    let output = Output {
                        format: RESOLVE_FORMAT,
                        resolved: &resolved,
                    };
                    println!("{}", serde_json::to_string(&output).unwrap_or_default());
                    ExitCode::SUCCESS
                }
                Err(body) => print_error(body),
            }
        }
        ConfigAction::Get { part, workspace } => match resolve(workspace.as_deref(), runtime_or_host()) {
            Ok(resolved) => match get(&resolved, part) {
                Some(Ok(value)) => {
                    println!("{}", serde_json::to_string(&value).unwrap_or_default());
                    ExitCode::SUCCESS
                }
                Some(Err(message)) => print_error(ErrorBody::Invalid { message }),
                None => {
                    eprintln!("piton: unknown configuration part {part:?}");
                    ExitCode::from(1)
                }
            },
            Err(body) => print_error(body),
        },
        ConfigAction::Set {
            key,
            value,
            changes,
            workspace,
            global,
            local,
            unset,
            dry_run,
        } => set(&SetRequest {
            key,
            value: value.as_deref(),
            changes: *changes,
            workspace: workspace.as_deref(),
            global: *global,
            local: *local,
            unset: *unset,
            dry_run: *dry_run,
        }),
        ConfigAction::Check { workspace } => match resolve(workspace.as_deref(), runtime_or_host()) {
            Ok(resolved) => {
                let errors = resolved.errors();
                for (part, error) in &errors {
                    eprintln!("piton: {part}: {error}");
                }
                println!("workspace: {}", resolved.workspace.display());
                println!("global tier: {}", resolved.global.display());
                for file in &resolved.files {
                    println!("  {:?}  {}", file.level, file.path.display());
                }
                // Portability: warnings only, the files still work here.
                for warning in crate::config::portability_warnings(&resolved.files, &resolved.home) {
                    eprintln!("piton: warning: {warning}");
                }
                if errors.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(EXIT_INVALID)
                }
            }
            Err(ErrorBody::Invalid { message } | ErrorBody::Workspace { message }) => {
                eprintln!("piton: {message}");
                ExitCode::from(EXIT_INVALID)
            }
        },
        ConfigAction::Protected {
            path,
            workspace,
            seatbelt_rules,
        } => {
            let dir = workspace
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let workspace = match crate::launch::resolve_workspace(&dir) {
                Ok(workspace) => workspace,
                Err(e) => return print_error(ErrorBody::Workspace { message: e.to_string() }),
            };
            let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            let roots = crate::protected::Roots {
                source: canonical(workspace.source()),
                checkout: canonical(&workspace.checkout),
            };
            match (path, seatbelt_rules) {
                (_, true) => {
                    for rule in roots.seatbelt_rules() {
                        println!("{rule}");
                    }
                    ExitCode::SUCCESS
                }
                (Some(path), false) => {
                    let target = crate::protected::canonical_target(&workspace.path.join(path));
                    let protected = roots.protected_root(&target).is_some();
                    let output = serde_json::json!({"format": RESOLVE_FORMAT, "path": target, "protected": protected});
                    println!("{output}");
                    ExitCode::SUCCESS
                }
                (None, false) => {
                    eprintln!("piton: give a path, or --seatbelt-rules");
                    ExitCode::from(1)
                }
            }
        }
    }
}

fn runtime_from_env() -> Result<Runtime, String> {
    match std::env::var("PITON_RUNTIME").as_deref() {
        Err(_) | Ok("") => Ok(Runtime::Host),
        Ok("sandbox") => Ok(Runtime::Sandbox),
        Ok(other) => Err(format!(
            "invalid PITON_RUNTIME {other:?}: expected \"sandbox\", or unset for the host runtime"
        )),
    }
}

fn runtime_or_host() -> Runtime {
    runtime_from_env().unwrap_or(Runtime::Host)
}

fn resolve(workspace: Option<&Path>, runtime: Runtime) -> Result<Resolved, ErrorBody> {
    let path = workspace
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let workspace =
        crate::launch::resolve_workspace(&path).map_err(|e| ErrorBody::Workspace { message: e.to_string() })?;
    crate::config::resolve(&workspace, &Paths::current(), runtime).map_err(|e| match e {
        ConfigError::Read { .. }
        | ConfigError::Invalid { .. }
        | ConfigError::GlobalOnly { .. }
        | ConfigError::LocalOnly { .. }
        | ConfigError::Conflict { .. } => ErrorBody::Invalid { message: e.to_string() },
    })
}

/// One resolved part, for tools that need only that (the Neovim plugin's tracker queue): its value, or why its files
/// can't be used. `None` for a name that isn't a part (each part serializes as `{ok}` or `{error: {message}}`).
fn get(resolved: &Resolved, part: &str) -> Option<Result<serde_json::Value, String>> {
    let mut json = serde_json::to_value(resolved).ok()?;
    let serde_json::Value::Object(mut section) = json.get_mut(part)?.take() else {
        return None;
    };
    Some(match (section.remove("ok"), section.remove("error")) {
        (Some(value), None) => Ok(value),
        (None, Some(error)) => Err(error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()),
        (Some(_), Some(_)) | (None, None) => return None,
    })
}

struct SetRequest<'a> {
    key: &'a str,
    value: Option<&'a str>,
    changes: bool,
    workspace: Option<&'a Path>,
    global: bool,
    local: bool,
    unset: bool,
    dry_run: bool,
}

/// What `set --dry-run` prints: the file to write and its new content (`null` content: no change).
#[derive(Serialize)]
struct Planned {
    format: u32,
    path: PathBuf,
    content: Option<String>,
}

/// `piton config set`: one key of a level's `config.toml` or `config.local.toml`, written with its comments and layout
/// kept, and refused when the file would no longer be valid.
fn set(request: &SetRequest) -> ExitCode {
    use crate::config::{ConfigFile, KeyChange, Sharing, WriteLevel};
    let invalid = |message: String| print_error(ErrorBody::Invalid { message });
    let paths = Paths::current();
    let level = if request.global {
        WriteLevel::Global
    } else {
        let path = request
            .workspace
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        match crate::launch::resolve_workspace(&path) {
            Ok(workspace) => WriteLevel::Repository {
                source: workspace.source().to_path_buf(),
            },
            Err(e) => return print_error(ErrorBody::Workspace { message: e.to_string() }),
        }
    };
    let sharing = if request.local { Sharing::Local } else { Sharing::Shared };
    let target = ConfigFile { level, sharing };
    // Each change: a dotted key, and its JSON value (`None`: remove it).
    let changes: Vec<KeyChange> = match (request.changes, request.unset, request.value) {
        (true, false, None) => match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(request.key) {
            Ok(map) => map
                .into_iter()
                .map(|(key, value)| {
                    let value = match value {
                        serde_json::Value::Null => None,
                        other @ (serde_json::Value::Bool(_)
                        | serde_json::Value::Number(_)
                        | serde_json::Value::String(_)
                        | serde_json::Value::Array(_)
                        | serde_json::Value::Object(_)) => Some(other),
                    };
                    KeyChange { key, value }
                })
                .collect(),
            Err(e) => return invalid(format!("--changes: a JSON object of keys and values: {e}")),
        },
        (false, true, None) => vec![KeyChange {
            key: request.key.to_string(),
            value: None,
        }],
        // JSON, or a bare string.
        (false, false, Some(text)) => vec![KeyChange {
            key: request.key.to_string(),
            value: Some(serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))),
        }],
        (true, _, _) | (false, true, Some(_)) | (false, false, None) => {
            eprintln!("piton: give KEY VALUE, KEY --unset, or a JSON object with --changes");
            return ExitCode::from(1);
        }
    };
    if request.dry_run {
        let content = match target.plan(&paths, &changes) {
            Ok(content) => content,
            Err(e) => return invalid(e.to_string()),
        };
        let planned = Planned {
            format: RESOLVE_FORMAT,
            path: target.path(&paths),
            content,
        };
        println!("{}", serde_json::to_string(&planned).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    match target.write(&paths, &changes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => invalid(e.to_string()),
    }
}

fn print_error(body: ErrorBody) -> ExitCode {
    let output = ErrorOutput {
        format: RESOLVE_FORMAT,
        error: body,
    };
    println!("{}", serde_json::to_string(&output).unwrap_or_default());
    ExitCode::from(EXIT_INVALID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Relation, Workspace};

    /// The JSON the pi package reads: the format version and the resolved fields at the top level.
    #[test]
    fn resolve_output_shape() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".piton")).unwrap();
        std::fs::write(
            repo.join(".piton/config.toml"),
            "[permissions]\nallow = [\"read\"]\n[branches]\nformat = 1\n",
        )
        .unwrap();
        let paths = Paths {
            home: home.clone(),
            global: home.join(".config/piton"),
        };
        let workspace = Workspace {
            path: repo.clone(),
            checkout: repo.clone(),
            relation: Relation::Source,
        };
        let resolved = crate::config::resolve(&workspace, &paths, Runtime::Sandbox).unwrap();
        let json = serde_json::to_value(Output {
            format: RESOLVE_FORMAT,
            resolved: &resolved,
        })
        .unwrap();
        assert_eq!(json["format"], RESOLVE_FORMAT);
        assert_eq!(json["runtime"], "sandbox");
        // Each part is `{ok}` or `{error: {message}}`.
        assert_eq!(json["permissions"]["ok"]["merged"]["allow"][0], "read");
        assert!(json["tracker"]["ok"].is_null());
        assert_eq!(get(&resolved, "tracker").unwrap().unwrap(), serde_json::Value::Null);
        assert!(get(&resolved, "branches").unwrap().is_err());
        assert!(get(&resolved, "watch").is_none());
        assert!(
            json["branches"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("config.toml")
        );
        assert!(json["branches"].get("ok").is_none());
        assert!(json["watch"].as_array().unwrap().len() > 5);

        let error = serde_json::to_value(ErrorOutput {
            format: RESOLVE_FORMAT,
            error: ErrorBody::Invalid { message: "m".into() },
        })
        .unwrap();
        assert_eq!(error["error"]["kind"], "invalid");
        assert_eq!(error["error"]["message"], "m");
    }
}
