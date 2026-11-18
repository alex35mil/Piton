//! The external tools piton runs (mise, tailscale), and whether each is used.
//!
//! One rule for every tool: unset in `[tools.<name>]`, it's used when it's installed and off otherwise; `enabled =
//! false` turns it off; `enabled = true` uses it, and when it isn't installed it's never run: the Settings list warns
//! and every operation that needs it fails with [`Unavailable`]. A machine without any of them works with all of them
//! off. Every decision about a tool matches [`Tool`] exhaustively, so a new tool must answer each one.
//!
//! The daemon decides each tool's state once at start (the settings apply on restart).

use serde::{Deserialize, Serialize};

use crate::{
    config::{DaemonConfig, ToolChoice, ToolConfig},
    launch::{Env, resolve_program},
};

/// An external tool piton can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    /// Per-project tool versions and environments: `mise env` for every launch, trust for workers.
    Mise,
    /// Remote access: identifies this Mac and the devices that connect over the tailnet.
    Tailscale,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Mise, Tool::Tailscale];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Mise => "mise",
            Tool::Tailscale => "tailscale",
        }
    }

    /// What piton uses it for, in setup's report.
    pub fn purpose(self) -> &'static str {
        match self {
            Tool::Mise => "per-project tool versions and environments",
            Tool::Tailscale => "remote access for the phone",
        }
    }

    pub fn install(self) -> &'static str {
        match self {
            Tool::Mise => "`brew install mise`",
            Tool::Tailscale => "install Tailscale from tailscale.com and sign in",
        }
    }

    fn config(self, config: &DaemonConfig) -> &ToolConfig {
        match self {
            Tool::Mise => &config.tools.mise,
            Tool::Tailscale => &config.tools.tailscale,
        }
    }
}

/// A tool's executable (resolved through the login shell's PATH) and its leading arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub program: String,
    pub args: Vec<String>,
}

/// How a tool is used, decided from its setting and whether it's installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// Turned off, or unset and not installed.
    Off(OffReason),
    Used(Program),
    /// `enabled = true`, but not installed: never run.
    Unavailable(Unavailable),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OffReason {
    /// `enabled = false`.
    Disabled,
    /// Unset, and not installed.
    NotInstalled,
}

/// A tool the configuration turns on that isn't installed: what every operation needing it fails with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error(
    "{} is enabled ([tools.{}] enabled = true) but {program:?} isn't installed on the login shell's PATH; install it \
     ({}) or turn it off",
    tool.name(),
    tool.name(),
    tool.install()
)]
pub struct Unavailable {
    pub tool: Tool,
    pub program: String,
}

impl ToolState {
    /// For an operation that runs the tool when it's used and does without it when it's off.
    pub fn optional(&self) -> Result<Option<&Program>, Unavailable> {
        match self {
            ToolState::Used(program) => Ok(Some(program)),
            ToolState::Off(_) => Ok(None),
            ToolState::Unavailable(unavailable) => Err(unavailable.clone()),
        }
    }
}

/// Decides `tool`'s state. `env` is the environment its command is found in: the login shell's.
pub fn resolve(tool: Tool, config: &DaemonConfig, env: &Env) -> ToolState {
    let setting = tool.config(config);
    let (name, args) = setting
        .command
        .split_first()
        .map(|(name, args)| (name.as_str(), args.to_vec()))
        .unwrap_or((tool.name(), Vec::new()));
    let found = || {
        let program = resolve_program(name, env);
        is_executable(std::path::Path::new(&program)).then(|| Program {
            program,
            args: args.clone(),
        })
    };
    match setting.choice {
        ToolChoice::Off => ToolState::Off(OffReason::Disabled),
        ToolChoice::Auto => match found() {
            Some(program) => ToolState::Used(program),
            None => ToolState::Off(OffReason::NotInstalled),
        },
        ToolChoice::On => match found() {
            Some(program) => ToolState::Used(program),
            None => ToolState::Unavailable(Unavailable {
                tool,
                program: name.to_string(),
            }),
        },
    }
}

/// The state of each tool, decided once from the configuration and the login shell's PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub mise: ToolState,
    pub tailscale: ToolState,
}

impl Tools {
    pub fn resolve(config: &DaemonConfig, env: &Env) -> Self {
        Self {
            mise: resolve(Tool::Mise, config, env),
            tailscale: resolve(Tool::Tailscale, config, env),
        }
    }

    pub fn get(&self, tool: Tool) -> &ToolState {
        match tool {
            Tool::Mise => &self.mise,
            Tool::Tailscale => &self.tailscale,
        }
    }

    /// One entry per tool for the daemon's log.
    pub fn describe(&self) -> String {
        Tool::ALL
            .iter()
            .map(|&tool| {
                let state = match self.get(tool) {
                    ToolState::Used(program) => program.program.clone(),
                    ToolState::Off(OffReason::Disabled) => "off".into(),
                    ToolState::Off(OffReason::NotInstalled) => "not installed (off)".into(),
                    ToolState::Unavailable(_) => "ENABLED BUT NOT INSTALLED".into(),
                };
                format!("{}={state}", tool.name())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A PATH with one executable `name` in it.
    fn path_with(dir: &Path, name: &str) -> Env {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join(name);
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        [("PATH".to_string(), dir.to_string_lossy().into_owned())].into()
    }

    /// Every setting against installed and missing, for every tool.
    #[test]
    fn every_setting_and_availability() {
        let dir = tempfile::tempdir().unwrap();
        for tool in Tool::ALL {
            let installed = path_with(dir.path(), tool.name());
            let missing = Env::new();
            let used = ToolState::Used(Program {
                program: dir.path().join(tool.name()).to_string_lossy().into_owned(),
                args: vec![],
            });
            let table = |enabled: &str| format!("[tools.{}]\n{enabled}", tool.name());
            let cases = [
                (table(""), &installed, used.clone()),
                (table(""), &missing, ToolState::Off(OffReason::NotInstalled)),
                (
                    table("enabled = false\n"),
                    &installed,
                    ToolState::Off(OffReason::Disabled),
                ),
                (
                    table("enabled = false\n"),
                    &missing,
                    ToolState::Off(OffReason::Disabled),
                ),
                (table("enabled = true\n"), &installed, used.clone()),
                (
                    table("enabled = true\n"),
                    &missing,
                    ToolState::Unavailable(Unavailable {
                        tool,
                        program: tool.name().into(),
                    }),
                ),
            ];
            for (host, env, expected) in cases {
                let config = DaemonConfig::from_host_toml(&host);
                assert_eq!(resolve(tool, &config, env), expected, "{} {host:?}", tool.name());
            }
        }
    }

    #[test]
    fn a_command_with_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let env = path_with(dir.path(), "my-mise");
        let config = DaemonConfig::from_host_toml("[tools.mise]\ncommand = [\"my-mise\", \"--quiet\"]\n");
        assert_eq!(
            resolve(Tool::Mise, &config, &env),
            ToolState::Used(Program {
                program: dir.path().join("my-mise").to_string_lossy().into_owned(),
                args: vec!["--quiet".into()],
            })
        );
    }

    /// An operation that can do without a tool runs it when used, skips it when off, and fails when unavailable.
    #[test]
    fn optional_use() {
        let tools = Tools::resolve(
            &DaemonConfig::from_host_toml("[tools.tailscale]\nenabled = true\n"),
            &Env::new(),
        );
        assert_eq!(tools.mise.optional(), Ok(None));
        let error = tools.tailscale.optional().unwrap_err();
        assert_eq!(error.tool, Tool::Tailscale);
        assert!(error.to_string().contains("[tools.tailscale] enabled = true"));
    }
}
