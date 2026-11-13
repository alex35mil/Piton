//! `piton config resolve|get|check`: the resolved configuration for a workspace, for the pi package, the Neovim plugin,
//! and the user. Reads files only; the daemon needn't run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use super::ConfigAction;
use crate::settings::{Paths, Resolved, Runtime, SettingsError};

/// Version of `piton config resolve --json`. Clients refuse a version they don't know.
pub const RESOLVE_FORMAT: u32 = 1;

/// Exit code for invalid or old-layout configuration, so callers can tell it from a failure to run.
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
    /// The configuration is invalid or uses the old layout: fix it, then try again.
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
            match crate::settings::resolve_global(&Paths::current()) {
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
        ConfigAction::Get { file, workspace } => match resolve(workspace.as_deref(), runtime_or_host()) {
            Ok(resolved) => match get(&resolved, file) {
                Some(Ok(value)) => {
                    println!("{}", serde_json::to_string(&value).unwrap_or_default());
                    ExitCode::SUCCESS
                }
                Some(Err(message)) => print_error(ErrorBody::Invalid { message }),
                None => {
                    eprintln!("piton: unknown configuration {file:?}");
                    ExitCode::from(1)
                }
            },
            Err(body) => print_error(body),
        },
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
    crate::settings::resolve(&workspace, &Paths::current(), runtime).map_err(|e| match e {
        SettingsError::Project(e) => ErrorBody::Workspace { message: e.to_string() },
        SettingsError::Read { .. }
        | SettingsError::Invalid { .. }
        | SettingsError::GlobalOnly { .. }
        | SettingsError::Conflict { .. }
        | SettingsError::OldLayout(_) => ErrorBody::Invalid { message: e.to_string() },
    })
}

/// One resolved part, for tools that need only that (the Neovim plugin's tracker queue): its value, or why its files
/// can't be used. `None` for an unknown name.
fn get(resolved: &Resolved, file: &str) -> Option<Result<serde_json::Value, String>> {
    fn value<T: Serialize>(section: &crate::settings::Section<T>) -> Result<serde_json::Value, String> {
        match section {
            Ok(value) => serde_json::to_value(value).map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }
    Some(match file {
        "integrations" => value(&resolved.integrations),
        "workflows" => value(&resolved.workflows),
        "permissions" => value(&resolved.permissions),
        "mcp" => value(&resolved.mcp),
        "daemon" => value(&resolved.daemon),
        "settings" => value(&resolved.settings),
        _ => return None,
    })
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
        std::fs::write(repo.join(".piton/permissions.json"), r#"{"allow":["read"]}"#).unwrap();
        std::fs::write(repo.join(".piton/workflows.json"), "{ broken").unwrap();
        let paths = Paths {
            home: home.clone(),
            global: home.join(".config/piton"),
            pi_agent_dir: home.join(".pi/agent"),
        };
        let workspace = Workspace {
            path: repo.clone(),
            checkout: repo.clone(),
            relation: Relation::Source,
        };
        let resolved = crate::settings::resolve(&workspace, &paths, Runtime::Sandbox).unwrap();
        let json = serde_json::to_value(Output {
            format: RESOLVE_FORMAT,
            resolved: &resolved,
        })
        .unwrap();
        assert_eq!(json["format"], RESOLVE_FORMAT);
        assert_eq!(json["runtime"], "sandbox");
        // Each part is `{ok}` or `{error: {message}}`.
        assert_eq!(json["permissions"]["ok"]["merged"]["allow"][0], "read");
        assert!(json["integrations"]["ok"].is_null());
        assert!(
            json["workflows"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("workflows.json")
        );
        assert!(json["workflows"].get("ok").is_none());
        assert_eq!(json["artifacts"], repo.join(".piton/.local").to_string_lossy().as_ref());
        assert!(json["watch"].as_array().unwrap().len() > 10);

        let error = serde_json::to_value(ErrorOutput {
            format: RESOLVE_FORMAT,
            error: ErrorBody::Invalid { message: "m".into() },
        })
        .unwrap();
        assert_eq!(error["error"]["kind"], "invalid");
        assert_eq!(error["error"]["message"], "m");
    }
}
