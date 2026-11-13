//! `piton setup`: what a host needs, checked and (where piton can do it) fixed. The Mac app's onboarding runs it as
//! `piton setup check --json` and `piton setup apply <step> --json` and shows each step with its fix; a terminal gets
//! the same steps as text.
//!
//! Steps are piton's, named for what they give the user, not for the agent runtime underneath: the agent runtime is one
//! prerequisite among others. Daemon registration belongs to the Mac app (`SMAppService` with the bundle's
//! LaunchAgent), so setup only reports it; `piton service install` is the development path.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;

/// Version of the JSON output. The Mac app refuses one it doesn't know.
pub const FORMAT: u32 = 1;

/// A setup step, in the order onboarding shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The tools a host runs: the agent runtime (pi), mise, tailscale, nono.
    Prerequisites,
    /// The global configuration folder exists and its files are valid, in the current layout.
    Configuration,
    /// `piton` on PATH for terminals and editors.
    Cli,
    /// The daemon runs at login.
    Daemon,
    /// The host's state database.
    State,
    /// How the phone and other clients reach this host.
    RemoteAccess,
}

pub const STEPS: &[Step] = &[
    Step::Prerequisites,
    Step::Configuration,
    Step::Cli,
    Step::Daemon,
    Step::State,
    Step::RemoteAccess,
];

impl Step {
    fn parse(name: &str) -> Option<Step> {
        STEPS.iter().copied().find(|step| step.name() == name)
    }

    fn name(self) -> &'static str {
        match self {
            Step::Prerequisites => "prerequisites",
            Step::Configuration => "configuration",
            Step::Cli => "cli",
            Step::Daemon => "daemon",
            Step::State => "state",
            Step::RemoteAccess => "remote_access",
        }
    }
}

/// A step's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Ok {
        detail: String,
    },
    /// Not done yet; `fix` says how. `apply` is true when `piton setup apply <step>` does it.
    Missing {
        problems: Vec<String>,
        fix: Fix,
    },
    /// Couldn't be checked or done.
    Failed {
        message: String,
    },
}

/// How a missing step gets done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fix {
    /// `piton setup apply <step>` does it.
    Apply { summary: String },
    /// The Mac app does it (daemon registration).
    App { summary: String },
    /// The user does it, following `instructions`.
    Manual { instructions: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepReport {
    pub step: Step,
    #[serde(flatten)]
    pub status: Status,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub format: u32,
    pub steps: Vec<StepReport>,
}

/// What setup looks at, so tests can run over fixture homes.
pub struct Host<'a> {
    pub config: &'a Config,
    /// The environment tools are looked up in (the login shell's).
    pub env: &'a crate::launch::Env,
    /// The `piton` binary the CLI link points to.
    pub binary: PathBuf,
    /// Where the CLI link goes.
    pub link: PathBuf,
    /// Whether the daemon answers on its socket.
    pub daemon_running: bool,
    /// This host's tailnet identity, or why it's unknown.
    pub tailnet: Result<crate::tailnet::SelfNode, crate::tailnet::TailnetError>,
}

/// A tool a host needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    /// The agent runtime.
    Pi,
    Mise,
    Nono,
    Tailscale,
}

const TOOLS: &[Tool] = &[Tool::Pi, Tool::Mise, Tool::Nono, Tool::Tailscale];

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Pi => "pi",
            Tool::Mise => "mise",
            Tool::Nono => "nono",
            Tool::Tailscale => "tailscale",
        }
    }

    /// The command the daemon runs: the configured one where piton has a setting for it.
    fn program(self, config: &Config) -> String {
        let configured = match self {
            Tool::Pi => config.pi.command.first().cloned(),
            Tool::Nono => config.sandbox.command.first().cloned(),
            Tool::Tailscale => Some(config.tailnet.cli.clone()),
            Tool::Mise => None,
        };
        configured.unwrap_or_else(|| self.name().to_string())
    }

    fn install(self) -> &'static str {
        match self {
            Tool::Pi => {
                "the agent runtime: `npm install -g @earendil-works/pi-coding-agent` (or `mise use -g npm:@earendil-works/pi-coding-agent`)"
            }
            Tool::Mise => "per-project tool versions: `brew install mise`",
            Tool::Nono => "the sandbox: `brew install nono` (or `mise use -g nono`)",
            Tool::Tailscale => "remote access for the phone: install Tailscale from tailscale.com and sign in",
        }
    }
}

pub fn check(host: &Host) -> Report {
    Report {
        format: FORMAT,
        steps: STEPS
            .iter()
            .map(|&step| StepReport {
                step,
                status: check_step(host, step),
            })
            .collect(),
    }
}

fn check_step(host: &Host, step: Step) -> Status {
    match step {
        Step::Prerequisites => prerequisites(host),
        Step::Configuration => configuration(host.config),
        Step::Cli => cli(host),
        Step::Daemon => match host.daemon_running {
            true => Status::Ok {
                detail: format!("running on {}", host.config.socket.display()),
            },
            false => Status::Missing {
                problems: vec!["the daemon isn't running".into()],
                fix: Fix::App {
                    summary: "Start piton at login (the Mac app registers it)".into(),
                },
            },
        },
        Step::State => {
            let db = host.config.state_db();
            // Missing only when the file system says so: a file this process can't see (a sandbox without access to
            // the state folder) exists as far as anyone knows, and fails the check with the reason instead.
            match std::fs::metadata(&db) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Status::Missing {
                    problems: vec![format!("{} doesn't exist yet", db.display())],
                    fix: Fix::Apply {
                        summary: "Create this host's state database".into(),
                    },
                },
                Err(e) => Status::Failed {
                    message: format!("can't check {}: {e}", db.display()),
                },
                // The daemon holds it open; a read-only look at its version is enough here.
                Ok(_) => match state_version(&db) {
                    Ok(()) => Status::Ok {
                        detail: db.display().to_string(),
                    },
                    Err(message) => Status::Failed { message },
                },
            }
        }
        Step::RemoteAccess => remote_access(host),
    }
}

/// Whether the database is one this piton can open (not newer, not corrupt).
fn state_version(db: &Path) -> Result<(), String> {
    crate::state::check(db).map_err(|e| e.to_string())
}

fn prerequisites(host: &Host) -> Status {
    let mut problems = Vec::new();
    let mut instructions = Vec::new();
    let mut found = Vec::new();
    for &tool in TOOLS {
        let name = tool.name();
        let program = tool.program(host.config);
        let resolved = crate::launch::resolve_program(&program, host.env);
        if is_executable(Path::new(&resolved)) {
            found.push(format!("{name}: {resolved}"));
        } else {
            problems.push(format!(
                "{name} isn't installed (looked for {program:?} on the login shell's PATH)"
            ));
            instructions.push(format!("{name}: {}", tool.install()));
        }
    }
    match problems.is_empty() {
        true => Status::Ok {
            detail: found.join("\n"),
        },
        false => Status::Missing {
            problems,
            fix: Fix::Manual { instructions },
        },
    }
}

fn configuration(config: &Config) -> Status {
    let paths = &config.paths;
    if !paths.global.is_dir() {
        return Status::Missing {
            problems: vec![format!("{} doesn't exist", paths.global.display())],
            fix: Fix::Apply {
                summary: format!(
                    "Create {} (or link it from your dotfiles first)",
                    paths.global.display()
                ),
            },
        };
    }
    let mut problems = Vec::new();
    match crate::settings::resolve_global(paths) {
        Ok(resolved) => {
            for (part, error) in resolved.errors() {
                problems.push(format!("{part}: {error}"));
            }
        }
        Err(e) => problems.push(e.to_string()),
    }
    if let Some(problem) = local_files_in_git(&paths.global) {
        problems.push(problem);
    }
    match problems.is_empty() {
        true => Status::Ok {
            detail: paths.global.display().to_string(),
        },
        false => Status::Missing {
            problems,
            fix: Fix::Manual {
                instructions: vec![
                    "Fix the files named above; `piton config check` shows each one.".into(),
                    "Files from the layout before .piton/: `piton config check` prints where each goes.".into(),
                ],
            },
        },
    }
}

/// `.local` files hold this host's values and secrets: when the global folder is in a Git repository, they must be
/// ignored there.
fn local_files_in_git(global: &Path) -> Option<String> {
    let dir = std::fs::canonicalize(global).ok()?;
    let inside = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()?;
    if !inside.status.success() {
        return None;
    }
    let local: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".local."))
        .collect();
    let exposed: Vec<&String> = local
        .iter()
        .filter(|name| {
            !std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["check-ignore", "-q", "--", name])
                .status()
                .is_ok_and(|status| status.success())
        })
        .collect();
    (!exposed.is_empty()).then(|| {
        format!(
            "{} is in a Git repository and doesn't ignore {}; add `*.local.*` to its .gitignore",
            global.display(),
            exposed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        )
    })
}

fn cli(host: &Host) -> Status {
    match std::fs::read_link(&host.link) {
        Ok(target) if target == host.binary => {
            let dir = host.link.parent().unwrap_or(Path::new("/"));
            match on_path(dir, host.env) {
                true => Status::Ok {
                    detail: format!("{} -> {}", host.link.display(), host.binary.display()),
                },
                false => Status::Missing {
                    problems: vec![format!("{} isn't on your shell's PATH", dir.display())],
                    fix: Fix::Manual {
                        instructions: vec![format!("Add {} to PATH in your shell's startup file", dir.display())],
                    },
                },
            }
        }
        Ok(target) => Status::Missing {
            problems: vec![format!(
                "{} points to {}, not this piton",
                host.link.display(),
                target.display()
            )],
            fix: Fix::Apply {
                summary: format!("Point {} to {}", host.link.display(), host.binary.display()),
            },
        },
        Err(_) if host.link.exists() => Status::Missing {
            problems: vec![format!(
                "{} exists and isn't a link; move it aside",
                host.link.display()
            )],
            fix: Fix::Manual {
                instructions: vec![format!("Remove or rename {}", host.link.display())],
            },
        },
        Err(_) => Status::Missing {
            problems: vec![format!("{} doesn't exist", host.link.display())],
            fix: Fix::Apply {
                summary: format!("Link {} to {}", host.link.display(), host.binary.display()),
            },
        },
    }
}

fn on_path(dir: &Path, env: &crate::launch::Env) -> bool {
    env.get("PATH")
        .is_some_and(|path| path.split(':').any(|entry| Path::new(entry) == dir))
}

fn remote_access(host: &Host) -> Status {
    let port = host.config.tailnet.port;
    if !host.config.tailnet.enabled {
        return Status::Ok {
            detail: "turned off ([tailnet] enabled = false): only this Mac connects".into(),
        };
    }
    match &host.tailnet {
        Ok(me) => {
            let address = me
                .dns_name
                .clone()
                .or_else(|| me.addresses.first().map(ToString::to_string))
                .unwrap_or_else(|| "this Mac's tailnet address".into());
            Status::Ok {
                detail: format!("clients connect to {address}, port {port}"),
            }
        }
        Err(reason) => Status::Missing {
            problems: vec![reason.to_string()],
            fix: Fix::Manual {
                instructions: vec!["Start Tailscale and sign in with the account your phone uses".into()],
            },
        },
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("unknown step {0:?}; steps: prerequisites, configuration, cli, daemon, state, remote_access")]
    Unknown(String),
    #[error("{step} isn't something piton setup does itself; follow its fix")]
    NotApplicable { step: &'static str },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error(transparent)]
    State(#[from] crate::state::StateError),
}

/// Does a step piton can do itself, then reports it again.
pub fn apply(host: &Host, name: &str) -> Result<StepReport, ApplyError> {
    let step = Step::parse(name).ok_or_else(|| ApplyError::Unknown(name.to_string()))?;
    let io = |path: &Path, source| ApplyError::Io {
        path: path.to_path_buf(),
        source,
    };
    match step {
        Step::Configuration => {
            let global = &host.config.paths.global;
            std::fs::create_dir_all(global).map_err(|e| io(global, e))?;
        }
        Step::Cli => {
            let dir = host.link.parent().unwrap_or(Path::new("/"));
            std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            match std::fs::read_link(&host.link) {
                // A link elsewhere is replaced; anything else stays (the check says to move it).
                Ok(_) => std::fs::remove_file(&host.link).map_err(|e| io(&host.link, e))?,
                Err(_) if host.link.exists() => {
                    return Err(ApplyError::NotApplicable { step: step.name() });
                }
                Err(_) => {}
            }
            std::os::unix::fs::symlink(&host.binary, &host.link).map_err(|e| io(&host.link, e))?;
        }
        Step::State => {
            crate::state::State::open(&host.config.state_db())?;
        }
        Step::Prerequisites | Step::Daemon | Step::RemoteAccess => {
            return Err(ApplyError::NotApplicable { step: step.name() });
        }
    }
    Ok(StepReport {
        step,
        status: check_step(host, step),
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for report in &self.steps {
            write!(f, "{}", report)?;
        }
        Ok(())
    }
}

impl std::fmt::Display for StepReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.step.name();
        match &self.status {
            Status::Ok { detail } => {
                writeln!(f, "ok       {name}")?;
                for line in detail.lines() {
                    writeln!(f, "           {line}")?;
                }
            }
            Status::Missing { problems, fix } => {
                writeln!(f, "missing  {name}")?;
                for problem in problems {
                    writeln!(f, "           {problem}")?;
                }
                match fix {
                    Fix::Apply { summary } => writeln!(f, "           fix: `piton setup apply {name}`: {summary}")?,
                    Fix::App { summary } => writeln!(f, "           fix (the Mac app): {summary}")?,
                    Fix::Manual { instructions } => {
                        for line in instructions {
                            writeln!(f, "           fix: {line}")?;
                        }
                    }
                }
            }
            Status::Failed { message } => writeln!(f, "failed   {name}: {message}")?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalSettings;
    use crate::settings::Paths;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        config: Config,
        env: crate::launch::Env,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in ["pi", "mise", "nono", "tailscale"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let paths = Paths {
            home: root.clone(),
            global: root.join(".config/piton"),
        };
        let mut config = Config::from_settings(GlobalSettings::default(), paths);
        config.socket = root.join("state/piton.sock");
        let env = [(
            "PATH".to_string(),
            format!("{}:{}", bin.display(), root.join(".local/bin").display()),
        )]
        .into_iter()
        .collect();
        Fixture {
            _dir: dir,
            root,
            config,
            env,
        }
    }

    fn host(f: &Fixture) -> Host<'_> {
        Host {
            config: &f.config,
            env: &f.env,
            binary: f.root.join("Piton.app/Contents/MacOS/piton"),
            link: f.root.join(".local/bin/piton"),
            daemon_running: false,
            tailnet: Err(crate::tailnet::TailnetError::NotRunning("Stopped".into())),
        }
    }

    fn status(report: &Report, step: Step) -> &Status {
        &report.steps.iter().find(|s| s.step == step).unwrap().status
    }

    #[test]
    fn a_fresh_host_and_its_fixes() {
        let f = fixture();
        let host = host(&f);
        let report = check(&host);
        assert!(matches!(status(&report, Step::Prerequisites), Status::Ok { .. }));
        assert!(matches!(
            status(&report, Step::Configuration),
            Status::Missing {
                fix: Fix::Apply { .. },
                ..
            }
        ));
        assert!(matches!(
            status(&report, Step::Cli),
            Status::Missing {
                fix: Fix::Apply { .. },
                ..
            }
        ));
        assert!(matches!(
            status(&report, Step::Daemon),
            Status::Missing {
                fix: Fix::App { .. },
                ..
            }
        ));
        assert!(matches!(
            status(&report, Step::State),
            Status::Missing {
                fix: Fix::Apply { .. },
                ..
            }
        ));
        assert!(matches!(
            status(&report, Step::RemoteAccess),
            Status::Missing {
                fix: Fix::Manual { .. },
                ..
            }
        ));

        for step in ["configuration", "cli", "state"] {
            let done = apply(&host, step).unwrap();
            assert!(matches!(done.status, Status::Ok { .. }), "{step}: {done}");
        }
        assert_eq!(std::fs::read_link(&host.link).unwrap(), host.binary);
        assert!(f.config.state_db().is_file());
        // Applying again is harmless.
        assert!(matches!(apply(&host, "cli").unwrap().status, Status::Ok { .. }));
        assert!(matches!(apply(&host, "daemon"), Err(ApplyError::NotApplicable { .. })));

        // A database from a newer piton fails the step, read-only.
        let conn = rusqlite::Connection::open(f.config.state_db()).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(matches!(status(&check(&host), Step::State), Status::Failed { message } if message.contains("newer")));
        assert!(matches!(apply(&host, "nope"), Err(ApplyError::Unknown(_))));
    }

    #[test]
    fn reports_what_to_do() {
        let f = fixture();
        let mut host = host(&f);
        // A missing tool names itself and how to install it.
        std::fs::remove_file(f.root.join("bin/nono")).unwrap();
        // The CLI link points to another piton.
        std::fs::create_dir_all(f.root.join(".local/bin")).unwrap();
        std::os::unix::fs::symlink("/elsewhere/piton", &host.link).unwrap();
        // An invalid global file.
        std::fs::create_dir_all(&f.config.paths.global).unwrap();
        std::fs::write(f.config.paths.global.join("config.toml"), "[host\n").unwrap();
        host.tailnet = Ok(crate::tailnet::SelfNode {
            user: 1,
            node: "n".into(),
            addresses: vec!["100.64.0.1".parse().unwrap()],
            dns_name: Some("my-mac.example.ts.net".into()),
        });
        host.daemon_running = true;

        let report = check(&host);
        let Status::Missing {
            problems,
            fix: Fix::Manual { instructions },
        } = status(&report, Step::Prerequisites)
        else {
            panic!("{report}")
        };
        assert!(problems[0].contains("nono"));
        assert!(instructions[0].starts_with("nono:"));
        assert!(matches!(
            status(&report, Step::Cli),
            Status::Missing {
                fix: Fix::Apply { .. },
                ..
            }
        ));
        assert!(matches!(
            status(&report, Step::Configuration),
            Status::Missing {
                fix: Fix::Manual { .. },
                ..
            }
        ));
        assert!(matches!(status(&report, Step::Daemon), Status::Ok { .. }));
        let Status::Ok { detail } = status(&report, Step::RemoteAccess) else {
            panic!()
        };
        assert_eq!(detail, "clients connect to my-mac.example.ts.net, port 7357");
        // The link is repaired.
        assert!(matches!(apply(&host, "cli").unwrap().status, Status::Ok { .. }));
    }

    #[test]
    fn a_global_folder_in_git_must_ignore_local_files() {
        let f = fixture();
        let global = &f.config.paths.global;
        std::fs::create_dir_all(global).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(global)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        git(&["init", "-q"]);
        std::fs::write(global.join("config.local.toml"), "").unwrap();
        let Status::Missing { problems, .. } = configuration(&f.config) else {
            panic!()
        };
        assert!(problems[0].contains("config.local.toml"), "{problems:?}");
        std::fs::write(global.join(".gitignore"), "*.local.*\n").unwrap();
        assert!(matches!(configuration(&f.config), Status::Ok { .. }));
    }

    /// The JSON the Mac app reads; a fixture PitonKit decodes.
    #[test]
    fn json_fixture() {
        let report = Report {
            format: FORMAT,
            steps: vec![
                StepReport {
                    step: Step::Prerequisites,
                    status: Status::Ok {
                        detail: "pi: /opt/bin/pi".into(),
                    },
                },
                StepReport {
                    step: Step::Cli,
                    status: Status::Missing {
                        problems: vec!["/Users/me/.local/bin/piton doesn't exist".into()],
                        fix: Fix::Apply {
                            summary: "Link it".into(),
                        },
                    },
                },
                StepReport {
                    step: Step::Daemon,
                    status: Status::Missing {
                        problems: vec!["the daemon isn't running".into()],
                        fix: Fix::App {
                            summary: "Start piton at login".into(),
                        },
                    },
                },
                StepReport {
                    step: Step::RemoteAccess,
                    status: Status::Missing {
                        problems: vec!["tailscale isn't running".into()],
                        fix: Fix::Manual {
                            instructions: vec!["Start Tailscale".into()],
                        },
                    },
                },
                StepReport {
                    step: Step::State,
                    status: Status::Failed {
                        message: "disk full".into(),
                    },
                },
            ],
        };
        let json = serde_json::to_string_pretty(&report).unwrap() + "\n";
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../apps/packages/PitonKit/Tests/Fixtures/setup.json");
        if std::env::var_os("PITON_WRITE_FIXTURES").is_some() {
            std::fs::write(&path, &json).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            json,
            "stale setup fixture: run `PITON_WRITE_FIXTURES=1 cargo test json_fixture` in daemon/"
        );
    }
}
