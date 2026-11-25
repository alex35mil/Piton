//! `piton config resolve|get|set|check`: the resolved configuration for a workspace, for the pi package, the Neovim
//! plugin, and the user, and changes to it. Reads and writes files only; the daemon needn't run.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use serde::Serialize;

use super::ConfigAction;
use crate::config::{ConfigError, Paths, Resolved, Runtime};

/// Version of `piton config resolve --json`. Clients refuse a version they don't know.
pub const RESOLVE_FORMAT: u32 = 1;

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
            #[derive(Serialize)]
            struct GlobalOutput<'a> {
                format: u32,
                #[serde(flatten)]
                resolved: &'a crate::config::GlobalResolved,
            }
            let resolved = crate::config::resolve_global(&Paths::current());
            let output = GlobalOutput {
                format: RESOLVE_FORMAT,
                resolved: &resolved,
            };
            println!("{}", serde_json::to_string(&output).unwrap_or_default());
            ExitCode::SUCCESS
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
            scope,
            local,
            unset,
            dry_run,
        } => set(&SetRequest {
            key,
            value: value.as_deref(),
            changes: *changes,
            workspace: workspace.as_deref(),
            global: *global,
            scope: scope.as_deref(),
            local: *local,
            unset: *unset,
            dry_run: *dry_run,
        }),
        ConfigAction::Check { workspace } => match resolve(workspace.as_deref(), runtime_or_host()) {
            Ok(resolved) => {
                // The workspace's chain, and the global tier's own files (host and client).
                let paths = Paths::current();
                let mut errors: Vec<(String, String)> = resolved
                    .errors()
                    .into_iter()
                    .map(|(part, e)| (part.to_string(), e.to_string()))
                    .collect();
                let mut ignored = None;
                match crate::config::host(&paths) {
                    Ok(host) => ignored = crate::config::DaemonConfig::new(&host, paths.clone()).ignored_socket_note(),
                    Err(e) => errors.push(("host".into(), e.to_string())),
                }
                if let Err(e) = crate::config::client(&paths) {
                    errors.push(("client".into(), e.to_string()));
                }
                for (part, error) in &errors {
                    eprintln!("piton: {part}: {error}");
                }
                let files: Vec<_> = resolved
                    .files
                    .iter()
                    .cloned()
                    .chain(crate::config::own_files(&paths))
                    .collect();
                println!("workspace: {}", resolved.workspace.display());
                println!("global tier: {}", resolved.global.display());
                for file in &files {
                    println!("  {:?}  {}", file.level, file.path.display());
                }
                // Portability: warnings only, the files still work here.
                for warning in crate::config::portability_warnings(&files, &resolved.home) {
                    eprintln!("piton: warning: {warning}");
                }
                if let Some(note) = ignored {
                    eprintln!("piton: warning: {note}");
                }
                print_trust(&resolved);
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
        ConfigAction::Validate { table, value } => match validate(table, value) {
            Ok(value) => {
                let output = serde_json::json!({"format": RESOLVE_FORMAT, "ok": value});
                println!("{output}");
                ExitCode::SUCCESS
            }
            Err(message) => print_error(ErrorBody::Invalid { message }),
        },
        ConfigAction::Protected { path, workspace } => {
            let dir = workspace
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            let workspace = match crate::launch::resolve_workspace(&dir) {
                Ok(workspace) => workspace,
                Err(e) => return print_error(ErrorBody::Workspace { message: e.to_string() }),
            };
            let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
            let protected = match protected_of(
                &workspace,
                &Paths::current(),
                env(crate::agent::PROTECTED_ENV).as_deref(),
                env("PI_CODING_AGENT_DIR").as_deref(),
            ) {
                Ok(protected) => protected,
                Err(message) => return print_error(ErrorBody::Invalid { message }),
            };
            let target = crate::agent::canonical_target(&workspace.path.join(path));
            let protected = protected.check(&target).is_some();
            let output = serde_json::json!({"format": RESOLVE_FORMAT, "path": target, "protected": protected});
            println!("{output}");
            ExitCode::SUCCESS
        }
    }
}

/// The protected files `piton config protected` answers from: the session's (`PITON_PROTECTED`, set by the daemon), or
/// outside one, the workspace's.
fn protected_of(
    workspace: &crate::project::Workspace,
    paths: &Paths,
    session: Option<&str>,
    agent_dir: Option<&str>,
) -> Result<crate::agent::Protected, String> {
    match session {
        Some(json) => serde_json::from_str(json).map_err(|e| {
            format!(
                "{} isn't a description of protected files: {e}",
                crate::agent::PROTECTED_ENV
            )
        }),
        None => workspace_protected(workspace, paths, agent_dir).map_err(|e| e.to_string()),
    }
}

/// The protected files of a pi outside a session the daemon launched: the checkout's, the workspace's configuration
/// levels, and pi's agent directory (`PI_CODING_AGENT_DIR`, else `~/.pi/agent`). No fnox files: only a launch knows
/// which fnox uses. The credential denies and the daemon's state are left out, as in a session; the user's `[sandbox]
/// deny` isn't read (a broken file mustn't stop the answer), so a link to a path only it closes still routes to the
/// review.
fn workspace_protected(
    workspace: &crate::project::Workspace,
    paths: &Paths,
    agent_dir: Option<&str>,
) -> Result<crate::agent::Protected, ConfigError> {
    let levels = crate::config::launch_levels(workspace, paths)?;
    let agent = crate::launch::sandbox::agent_dir(agent_dir, &paths.home, &workspace.path);
    let no_lists = crate::config::LaunchConfig {
        sandbox: Default::default(),
        workers: Default::default(),
        source: workspace.source().to_path_buf(),
        checkout: workspace.checkout.clone(),
        levels: levels.clone(),
        untrusted: Vec::new(),
    };
    let mut excluded = crate::launch::sandbox::never_configuration(&no_lists, &paths.home, &Default::default());
    excluded.extend(
        crate::config::default_socket(&paths.home)
            .parent()
            .map(Path::to_path_buf),
    );
    Ok(crate::agent::Protected::new(&crate::agent::ProtectedFacts {
        source: workspace.source(),
        checkout: &workspace.checkout,
        levels: &levels,
        agent_dirs: &[agent],
        fnox_files: &[],
        global: &paths.global,
        home: &paths.home,
        excluded: &excluded,
    }))
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

/// `piton config check`'s trust line: whether the repository's committed grants apply, and which are left out.
fn print_trust(resolved: &Resolved) {
    let Ok(workspace) = crate::launch::resolve_workspace(&resolved.workspace) else {
        return;
    };
    let paths = Paths::current();
    let Ok(inputs) = crate::config::trust_inputs(&workspace, &paths) else {
        return;
    };
    let state = crate::trust::read_state(&state_db(&paths), &inputs);
    let line = match &state {
        Ok(crate::trust::TrustState::NothingToTrust) => "nothing to trust".to_string(),
        Ok(crate::trust::TrustState::Trusted { .. }) => "trusted".to_string(),
        Ok(crate::trust::TrustState::Declined { .. }) => "declined: its grants are left out".to_string(),
        Ok(crate::trust::TrustState::Needed { added, .. }) => format!(
            "not decided ({} grant(s) to decide): the next launch asks; `piton trust allow` answers ahead",
            added.len()
        ),
        Err(e) => format!("unknown ({e})"),
    };
    println!("trust: {} {line}", inputs.source.display());
    for grant in &resolved.untrusted {
        println!("  untrusted  {grant}");
    }
}

/// The state database, from the host file's socket (the daemon's state folder), as the daemon finds it.
fn state_db(paths: &Paths) -> PathBuf {
    let host = crate::config::host(paths).unwrap_or_default();
    crate::config::DaemonConfig::new(&host, paths.clone()).state_db()
}

fn resolve(workspace: Option<&Path>, runtime: Runtime) -> Result<Resolved, ErrorBody> {
    let path = workspace
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let workspace =
        crate::launch::resolve_workspace(&path).map_err(|e| ErrorBody::Workspace { message: e.to_string() })?;
    let paths = Paths::current();
    // Inside a session: what its launch kept; outside: the user's recorded decision.
    let filter = crate::config::trust_inputs(&workspace, &paths).map_or(crate::config::GrantFilter::None, |inputs| {
        crate::trust::current_filter(&state_db(&paths), &inputs)
    });
    crate::config::resolve(&workspace, &paths, runtime, &filter).map_err(|e| match e {
        ConfigError::Read { .. }
        | ConfigError::Invalid { .. }
        | ConfigError::NotInProjectConfig { .. }
        | ConfigError::ProjectOnly { .. }
        | ConfigError::LocalOnly { .. }
        | ConfigError::Conflict { .. } => ErrorBody::Invalid { message: e.to_string() },
    })
}

/// `piton config validate`: `value` read and checked as `table`, and printed back as the file would hold it. Its errors
/// name the key; a key that isn't a table, or text that isn't JSON, is an error too.
fn validate(table: &str, value: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(value).map_err(|e| format!("VALUE isn't JSON: {e}"))?;
    crate::config::validate_table(table, &value).map_err(|e| e.to_string())
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
    scope: Option<&'a str>,
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
    use crate::config::{ConfigFile, KeyChange, KeyPath, Sharing, WriteLevel};
    let invalid = |message: String| print_error(ErrorBody::Invalid { message });
    let paths = Paths::current();
    let level = match (request.scope, request.global) {
        (Some("host"), _) => WriteLevel::Host,
        (Some("client"), _) => WriteLevel::Client,
        (Some(other), _) => return invalid(format!("unknown --scope {other:?}: host or client")),
        (None, true) => WriteLevel::Global,
        (None, false) => {
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
        }
    };
    let sharing = if request.local { Sharing::Local } else { Sharing::Shared };
    let target = ConfigFile { level, sharing };
    // Each change: a TOML dotted key (a part with dots or slashes quoted), and its JSON value (`None`: remove it).
    let key = |text: &str| KeyPath::parse(text);
    let changes: Vec<KeyChange> = match (request.changes, request.unset, request.value) {
        (true, false, None) => match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(request.key) {
            Ok(map) => {
                let mut changes = Vec::new();
                for (text, value) in map {
                    let value = match value {
                        serde_json::Value::Null => None,
                        other @ (serde_json::Value::Bool(_)
                        | serde_json::Value::Number(_)
                        | serde_json::Value::String(_)
                        | serde_json::Value::Array(_)
                        | serde_json::Value::Object(_)) => Some(other),
                    };
                    match key(&text) {
                        Ok(key) => changes.push(KeyChange { key, value }),
                        Err(e) => return invalid(e),
                    }
                }
                changes
            }
            Err(e) => return invalid(format!("--changes: a JSON object of keys and values: {e}")),
        },
        (false, true, None) => match key(request.key) {
            Ok(key) => vec![KeyChange { key, value: None }],
            Err(e) => return invalid(e),
        },
        // JSON, or a bare string.
        (false, false, Some(text)) => match key(request.key) {
            Ok(key) => vec![KeyChange {
                key,
                value: Some(serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))),
            }],
            Err(e) => return invalid(e),
        },
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
        let path = match target.path(&paths) {
            Ok(path) => path,
            Err(e) => return invalid(e.to_string()),
        };
        let planned = Planned {
            format: RESOLVE_FORMAT,
            path,
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
            "[tools.permissions]\nallow = [\"read\"]\n[branches]\nformat = 1\n",
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
        let resolved =
            crate::config::resolve(&workspace, &paths, Runtime::Sandbox, &crate::config::GrantFilter::All).unwrap();
        let json = serde_json::to_value(Output {
            format: RESOLVE_FORMAT,
            resolved: &resolved,
        })
        .unwrap();
        assert_eq!(json["format"], RESOLVE_FORMAT);
        assert_eq!(json["runtime"], "sandbox");
        // Each part is `{ok}` or `{error: {message}}`.
        assert_eq!(json["tools"]["ok"]["permissions"]["merged"]["allow"][0], "read");
        assert!(json.get("permissions").is_none());
        assert_eq!(
            get(&resolved, "tools").unwrap().unwrap()["permissions"]["merged"]["allow"][0],
            "read"
        );
        let absent = serde_json::json!({"state": "off", "reason": "absent"});
        assert_eq!(json["tracker"]["ok"], absent);
        assert_eq!(get(&resolved, "tracker").unwrap().unwrap(), absent);
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

    /// `piton config protected` answers from the session's description when the daemon set it (so pi routes a fnox
    /// file it alone knows), and from the workspace outside a session: its levels (even with a broken file in one) and
    /// pi's agent directory.
    #[test]
    fn protected_answers_from_the_session_or_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let repo = home.join("repo");
        for d in [home.join(".config/piton"), repo.join(".piton"), home.join("agent")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".config/piton/config.toml"), "not toml [").unwrap();
        let paths = Paths {
            home: home.clone(),
            global: home.join(".config/piton"),
        };
        let workspace = Workspace {
            path: repo.clone(),
            checkout: repo.clone(),
            relation: Relation::Source,
        };
        let check = |session: Option<&str>, agent: Option<&str>, path: &Path| {
            protected_of(&workspace, &paths, session, agent)
                .unwrap()
                .check(path)
                .is_some()
        };
        let global = home.join(".config/piton/config.toml");
        assert!(check(None, None, &global));
        assert!(check(None, None, &home.join(".pi/agent/settings.json")));
        assert!(!check(None, None, &home.join("agent/settings.json")));
        assert!(check(None, Some("~/agent"), &home.join("agent/settings.json")));
        assert!(!check(None, None, &home.join(".pi/agent/auth.json")));
        assert!(check(None, None, &repo.join(".piton/config.toml")));
        assert!(!check(None, None, &repo.join("src/main.rs")));

        let fnox = home.join(".config/fnox/config.toml");
        let session = crate::agent::Protected::new(&crate::agent::ProtectedFacts {
            source: &repo,
            checkout: &repo,
            levels: &[],
            agent_dirs: &[],
            fnox_files: std::slice::from_ref(&fnox),
            global: &home.join(".config/piton"),
            home: &home,
            excluded: &[],
        });
        let json = serde_json::to_string(&session).unwrap();
        assert!(check(Some(&json), None, &fnox));
        assert!(!check(Some(&json), None, &global), "the session's description decides");
        assert!(protected_of(&workspace, &paths, Some("{"), None).is_err());
    }
}
