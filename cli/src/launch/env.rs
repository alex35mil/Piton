//! The environment tools the daemon runs to build pi's environment (mise today; direnv and Nix later), and the
//! environment of a sandboxed launch with where each value came from.
//!
//! A workspace's tool configuration is written by whoever works in it, a sandboxed agent included, so for a sandboxed
//! launch the tools run inside the launch's sandbox, and a value that widens the sandbox is read only through
//! [`SandboxEnv::widening`], with the file that set it. Only this module builds a [`SandboxEnv`]: the policy can't be
//! resolved from an environment the tools built on the host.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use tokio::process::Command;

use super::{
    Env, LaunchError, resolve_program,
    sandbox::{Contained, Containment, Policy, contained},
};
use crate::integrations::{Integration, Integrations, Program, Unavailable};

/// A tool the daemon runs to add a workspace's environment to pi's. Every operation matches it exhaustively, so a new
/// tool can't be added without saying how it runs, contained, and how its values' sources are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvTool {
    Mise,
}

/// Every [`EnvTool`], in the order their environments are applied.
pub const ENV_TOOLS: [EnvTool; 1] = [EnvTool::Mise];

impl std::fmt::Display for EnvTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.program())
    }
}

impl EnvTool {
    fn program(self) -> &'static str {
        match self {
            Self::Mise => "mise",
        }
    }

    /// The integration it is, whose `[integrations.<name>]` setting decides whether it runs.
    fn integration(self) -> Integration {
        match self {
            Self::Mise => Integration::Mise,
        }
    }

    /// The arguments that print the workspace's environment with each value's source.
    fn args(self, workspace: &Path) -> Vec<OsString> {
        match self {
            // `--json-extended`: `{ NAME: { value, source?, tool? } }`; `source` is the file that set it, as written.
            Self::Mise => vec![
                "env".into(),
                "--json-extended".into(),
                "-C".into(),
                workspace.as_os_str().to_os_string(),
            ],
        }
    }

    /// The tool's output as values with their sources.
    fn parse(self, stdout: &[u8]) -> Result<Vec<(String, String, ToolSource)>, String> {
        match self {
            Self::Mise => {
                #[derive(serde::Deserialize)]
                struct Entry {
                    value: String,
                    #[serde(default)]
                    source: Option<String>,
                }
                let entries: BTreeMap<String, Entry> =
                    serde_json::from_slice(stdout).map_err(|e| format!("unexpected mise output: {e}"))?;
                Ok(entries
                    .into_iter()
                    .map(|(name, entry)| {
                        let source = match entry.source.filter(|s| !s.is_empty()) {
                            Some(file) => ToolSource::File(PathBuf::from(file)),
                            None => ToolSource::Unknown,
                        };
                        (name, entry.value, source)
                    })
                    .collect())
            }
        }
    }
}

/// An environment tool that runs for a launch, with its command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enabled {
    pub tool: EnvTool,
    pub program: Program,
}

/// The environment tools that run for every launch, by their `[integrations]` state; an error when one is enabled but
/// not installed.
pub fn enabled(integrations: &Integrations) -> Result<Vec<Enabled>, Unavailable> {
    let mut out = Vec::new();
    for tool in ENV_TOOLS {
        if let Some(program) = integrations.get(tool.integration()).optional()? {
            out.push(Enabled {
                tool,
                program: program.clone(),
            });
        }
    }
    Ok(out)
}

/// Which file a tool says set a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    File(PathBuf),
    /// The tool named none (a tool's own variables, `PATH`): treated as written by the agent.
    Unknown,
}

/// Where a value of pi's environment came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The daemon's base environment (the login shell's): never the workspace's.
    Daemon,
    Tool {
        tool: EnvTool,
        source: ToolSource,
    },
}

/// A value the sandbox policy derives grants from. The policy reads these, and only these, from the launch's
/// environment, each with its [`Origin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widening {
    /// pi's agent directory, whose runtime state the sandbox may write.
    AgentDir,
    /// The pi-claude-code extension's Claude Code config folder, which the `pi-claude-code` preset makes writable.
    ClaudeCodeConfig,
    /// Where `claude` puts its sockets first (`<dir>/cc-socks/<pid>.sock`); the preset opens it when the user sets it.
    XdgRuntime,
    /// Claude Code's temp root, where `claude` puts its sockets when `XDG_RUNTIME_DIR` isn't set; the preset opens it
    /// when the user sets it, else sets it to a folder of the session's own.
    ClaudeCodeTmp,
}

impl Widening {
    pub fn var(self) -> &'static str {
        match self {
            Self::AgentDir => "PI_CODING_AGENT_DIR",
            Self::ClaudeCodeConfig => "PI_CLAUDE_CODE_CONFIG_DIR",
            Self::XdgRuntime => "XDG_RUNTIME_DIR",
            Self::ClaudeCodeTmp => "CLAUDE_CODE_TMPDIR",
        }
    }
}

/// Variables of pi's environment set by one file the agent could write (`None`: a tool named no file).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WritableVars {
    pub source: Option<PathBuf>,
    pub names: Vec<String>,
}

/// pi's environment for a sandboxed launch, each value with its [`Origin`]. Built only here: from the daemon's base
/// environment, or by [`sandboxed`], which runs the tools inside the launch's sandbox.
#[derive(Debug, Clone)]
pub struct SandboxEnv {
    vars: BTreeMap<String, (String, Origin)>,
}

impl SandboxEnv {
    /// The daemon's base environment, before any tool: what the bootstrap policy (the one the tools run under) uses.
    pub fn daemon(base: &Env) -> Self {
        Self {
            vars: base
                .iter()
                .map(|(name, value)| (name.clone(), (value.clone(), Origin::Daemon)))
                .collect(),
        }
    }

    /// A widening value and where it came from. Empty counts as unset.
    pub fn widening(&self, widening: Widening) -> Option<(&str, &Origin)> {
        self.vars
            .get(widening.var())
            .filter(|(value, _)| !value.is_empty())
            .map(|(value, origin)| (value.as_str(), origin))
    }

    /// A value that decides no grant, as pi gets it: a variable the policy only checks a widening value against.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(|(value, _)| value.as_str())
    }

    /// The variables a file `writable` says the agent could write set, grouped by that file, in name order: what a host
    /// run gets that the agent may have chosen. A value whose tool named no file counts as writable, as for widening
    /// values; the daemon's base environment never does.
    pub fn agent_writable(&self, writable: impl Fn(&Path) -> bool) -> Vec<WritableVars> {
        let mut groups: Vec<WritableVars> = Vec::new();
        for (name, (_, origin)) in &self.vars {
            let source = match origin {
                Origin::Daemon => continue,
                Origin::Tool {
                    source: ToolSource::File(path),
                    ..
                } => match writable(path) {
                    true => Some(path.clone()),
                    false => continue,
                },
                Origin::Tool {
                    source: ToolSource::Unknown,
                    ..
                } => None,
            };
            match groups.iter_mut().find(|g| g.source == source) {
                Some(group) => group.names.push(name.clone()),
                None => groups.push(WritableVars {
                    source,
                    names: vec![name.clone()],
                }),
            }
        }
        groups
    }

    /// The plain environment pi is started with.
    pub fn into_env(self) -> Env {
        self.vars.into_iter().map(|(name, (value, _))| (name, value)).collect()
    }

    #[cfg(test)]
    pub fn for_tests(vars: &[(&str, &str, Origin)]) -> Self {
        Self {
            vars: vars
                .iter()
                .map(|(name, value, origin)| (name.to_string(), (value.to_string(), origin.clone())))
                .collect(),
        }
    }
}

/// How a tool is started.
enum Runner<'a> {
    /// On the host, as the user would: host launches trust the project by choice.
    Host,
    /// Inside the launch's sandbox.
    Sandbox {
        helper: &'a Path,
        policy: &'a Policy,
        containment: Containment,
    },
}

type Invocation = Contained;

fn invocation(tool: &Enabled, base: &Env, workspace: &Path, runner: &Runner) -> Result<Invocation, String> {
    let program = resolve_program(&tool.program.program, base);
    let mut args: Vec<OsString> = tool.program.args.iter().map(OsString::from).collect();
    args.extend(tool.tool.args(workspace));
    match runner {
        Runner::Host => Ok(Invocation {
            program,
            args,
            env: base.clone(),
        }),
        Runner::Sandbox {
            helper,
            policy,
            containment,
        } => contained(helper, policy, *containment, program, args, base),
    }
}

async fn run(
    tool: &Enabled,
    base: &Env,
    workspace: &Path,
    runner: &Runner<'_>,
) -> Result<Vec<(String, String, ToolSource)>, LaunchError> {
    let failed = |stderr: String| LaunchError::EnvTool {
        tool: tool.tool,
        workspace: workspace.to_path_buf(),
        stderr,
    };
    let call = invocation(tool, base, workspace, runner).map_err(failed)?;
    let out = Command::new(&call.program)
        .args(&call.args)
        .current_dir(workspace)
        .env_clear()
        .envs(&call.env)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|source| LaunchError::Spawn {
            program: call.program.clone(),
            source,
        })?;
    if !out.status.success() {
        return Err(failed(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    tool.tool.parse(&out.stdout).map_err(failed)
}

/// pi's environment for a host launch: the base environment, then each tool's, run on the host.
pub async fn host(tools: &[Enabled], base: &Env, workspace: &Path) -> Result<Env, LaunchError> {
    let mut env = base.clone();
    for tool in tools {
        for (name, value, _) in run(tool, base, workspace, &Runner::Host).await? {
            env.insert(name, value);
        }
    }
    Ok(env)
}

/// pi's environment for a sandboxed launch: the base environment, then each tool's, run inside `policy` (or the
/// sandbox this process already runs in), each value with its source.
pub async fn sandboxed(
    tools: &[Enabled],
    base: &Env,
    workspace: &Path,
    policy: &Policy,
    containment: Containment,
    helper: &Path,
) -> Result<SandboxEnv, LaunchError> {
    let mut env = SandboxEnv::daemon(base);
    let runner = Runner::Sandbox {
        helper,
        policy,
        containment,
    };
    for tool in tools {
        for (name, value, source) in run(tool, base, workspace, &runner).await? {
            env.vars.insert(
                name,
                (
                    value,
                    Origin::Tool {
                        tool: tool.tool,
                        source,
                    },
                ),
            );
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::sandbox::POLICY_ENV;

    /// A sample of each tool's output, with what it must parse to.
    fn fixture(tool: EnvTool) -> (&'static str, Vec<(&'static str, &'static str, ToolSource)>) {
        match tool {
            EnvTool::Mise => (
                r#"{
                    "A": {"source": "/w/mise.toml", "value": "1"},
                    "B": {"source": "", "tool": "rust", "value": "2"},
                    "PATH": {"value": "/bin"}
                }"#,
                vec![
                    ("A", "1", ToolSource::File("/w/mise.toml".into())),
                    ("B", "2", ToolSource::Unknown),
                    ("PATH", "/bin", ToolSource::Unknown),
                ],
            ),
        }
    }

    /// Fails to compile when a tool is added, until it's listed in [`ENV_TOOLS`].
    fn listed(tool: EnvTool) -> bool {
        match tool {
            EnvTool::Mise => ENV_TOOLS.contains(&EnvTool::Mise),
        }
    }

    /// Every tool runs through `sandbox-exec` with the launch's policy, runs directly inside a sandbox it inherits,
    /// and gives each value its source.
    #[test]
    fn every_tool_runs_in_the_sandbox_and_reports_sources() {
        let policy = Policy {
            rules: Vec::new(),
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
            tag: String::new(),
        };
        let base: Env = [("PATH".to_string(), "/nonexistent".to_string())].into();
        let workspace = Path::new("/w");
        for tool in ENV_TOOLS {
            assert!(listed(tool));
            // A configured command with leading arguments: they go before the tool's own.
            let enabled = Enabled {
                tool,
                program: Program {
                    program: tool.program().into(),
                    args: vec!["--quiet".into()],
                },
            };
            let applied = invocation(
                &enabled,
                &base,
                workspace,
                &Runner::Sandbox {
                    helper: Path::new("/app/piton"),
                    policy: &policy,
                    containment: Containment::Apply,
                },
            )
            .unwrap();
            assert_eq!(applied.program, "/app/piton", "{tool}");
            assert_eq!(
                applied.args[..4],
                [
                    "sandbox-exec".into(),
                    "--".into(),
                    OsString::from(tool.program()),
                    "--quiet".into()
                ]
            );
            assert_eq!(applied.args[4..], tool.args(workspace)[..]);
            assert!(applied.env.contains_key(POLICY_ENV), "{tool}");

            let inherited = invocation(
                &enabled,
                &base,
                workspace,
                &Runner::Sandbox {
                    helper: Path::new("/app/piton"),
                    policy: &policy,
                    containment: Containment::Inherited,
                },
            )
            .unwrap();
            assert_eq!(inherited.program, tool.program());
            assert!(!inherited.env.contains_key(POLICY_ENV));

            let (output, expected) = fixture(tool);
            let parsed = tool.parse(output.as_bytes()).unwrap();
            let expected: Vec<(String, String, ToolSource)> = expected
                .into_iter()
                .map(|(n, v, s)| (n.to_string(), v.to_string(), s))
                .collect();
            assert_eq!(parsed, expected, "{tool}");
        }
    }

    /// Each environment tool runs when its `[integrations]` state uses it; enabled but not installed refuses every
    /// launch.
    #[test]
    fn integration_state_decides_which_run() {
        use crate::integrations::{IntegrationState, OffReason};
        let used = Program {
            program: "/bin/mise".into(),
            args: vec![],
        };
        let tools = |mise: IntegrationState| Integrations {
            mise,
            fnox: IntegrationState::Off(OffReason::Disabled),
            tailscale: IntegrationState::Off(OffReason::Disabled),
        };
        assert_eq!(
            enabled(&tools(IntegrationState::Used(used.clone()))).unwrap(),
            [Enabled {
                tool: EnvTool::Mise,
                program: used
            }]
        );
        assert_eq!(
            enabled(&tools(IntegrationState::Off(OffReason::NotInstalled))).unwrap(),
            []
        );
        let unavailable = Unavailable {
            integration: Integration::Mise,
            program: "mise".into(),
        };
        assert_eq!(
            enabled(&tools(IntegrationState::Unavailable(unavailable.clone()))),
            Err(unavailable)
        );
    }

    #[test]
    fn tool_values_override_the_daemons_and_keep_their_origin() {
        let mut env = SandboxEnv::daemon(&[("PI_CODING_AGENT_DIR".to_string(), "/host".to_string())].into());
        assert_eq!(env.widening(Widening::AgentDir), Some(("/host", &Origin::Daemon)));
        let origin = Origin::Tool {
            tool: EnvTool::Mise,
            source: ToolSource::File("/w/mise.toml".into()),
        };
        env.vars
            .insert("PI_CODING_AGENT_DIR".into(), ("/w/agent".into(), origin.clone()));
        assert_eq!(env.widening(Widening::AgentDir), Some(("/w/agent", &origin)));
        env.vars.insert("PI_CODING_AGENT_DIR".into(), (String::new(), origin));
        assert_eq!(env.widening(Widening::AgentDir), None, "empty is unset");
        assert_eq!(env.into_env()["PI_CODING_AGENT_DIR"], "");
    }

    /// What a host run gets that the agent may have chosen: values from a file the launch's sandbox can write, and
    /// values whose tool named no file; never the daemon's own or a value from a file the agent can't write.
    #[test]
    fn agent_writable_variables_are_grouped_by_their_file() {
        let tool = |source| Origin::Tool {
            tool: EnvTool::Mise,
            source,
        };
        let workspace = || ToolSource::File("/w/mise.toml".into());
        let env = SandboxEnv::for_tests(&[
            ("HOME", "/h", Origin::Daemon),
            ("BASH_ENV", "./x.sh", tool(workspace())),
            ("RUST_LOG", "debug", tool(workspace())),
            ("SHARED", "1", tool(ToolSource::File("/p/mise.toml".into()))),
            ("PATH", "/m/bin:/usr/bin", tool(ToolSource::Unknown)),
        ]);
        let groups = env.agent_writable(|path| path.starts_with("/w"));
        assert_eq!(
            groups,
            [
                WritableVars {
                    source: Some("/w/mise.toml".into()),
                    names: vec!["BASH_ENV".into(), "RUST_LOG".into()],
                },
                WritableVars {
                    source: None,
                    names: vec!["PATH".into()],
                },
            ]
        );
    }

    #[test]
    fn containment_follows_this_process() {
        let expected = match std::env::var("PITON_RUNTIME").as_deref() {
            Ok("sandbox") => Containment::Inherited,
            Ok(_) | Err(_) => Containment::Apply,
        };
        assert_eq!(super::super::sandbox::containment(), expected);
    }
}
