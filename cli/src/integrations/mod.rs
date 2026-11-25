//! The external programs piton integrates with (mise, fnox, tailscale), and whether each is used.
//!
//! One rule for every integration: unset in `[integrations.<name>]`, it's used when it's installed and off otherwise;
//! `enabled = false` turns it off; `enabled = true` uses it, and when it isn't installed it's never run: the Settings
//! list warns and every operation that needs it fails with [`Unavailable`]. A machine without any of them works with
//! all of them off. Every decision about an integration matches [`Integration`] exhaustively, so a new one must answer
//! each.
//!
//! The daemon decides each integration's state once at start (the settings apply on restart).

use serde::{Deserialize, Serialize};

use crate::{
    config::{DaemonConfig, IntegrationChoice, IntegrationConfig},
    launch::{Env, resolve_program},
};

/// An external program piton can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integration {
    /// Per-project tool versions and environments: `mise env` for every launch, trust for workers.
    Mise,
    /// The credential proxy every launch starts behind (experimental): pi holds placeholders, fnox puts the real secrets
    /// into the requests its rules allow.
    Fnox,
    /// Remote access: identifies this Mac and the devices that connect over the tailnet, and while it's used the apps
    /// are served there.
    Tailscale,
}

impl Integration {
    pub const ALL: [Integration; 3] = [Integration::Mise, Integration::Fnox, Integration::Tailscale];

    pub fn name(self) -> &'static str {
        match self {
            Integration::Mise => "mise",
            Integration::Fnox => "fnox",
            Integration::Tailscale => "tailscale",
        }
    }

    /// Its `enabled` key, as written in the host file.
    pub fn enabled_key(self) -> &'static str {
        match self {
            Integration::Mise => "integrations.mise.enabled",
            Integration::Fnox => "integrations.fnox.enabled",
            Integration::Tailscale => "integrations.tailscale.enabled",
        }
    }

    /// What piton uses it for, in setup's report.
    pub fn purpose(self) -> &'static str {
        match self {
            Integration::Mise => "per-project tool versions and environments",
            Integration::Fnox => "a credential proxy, so pi holds placeholders instead of secrets (experimental)",
            Integration::Tailscale => "remote access for the phone",
        }
    }

    pub fn install(self) -> &'static str {
        match self {
            Integration::Mise => "`brew install mise`",
            Integration::Fnox => "`mise use -g fnox`",
            Integration::Tailscale => "install Tailscale from tailscale.com and sign in",
        }
    }

    fn config(self, config: &DaemonConfig) -> &IntegrationConfig {
        match self {
            Integration::Mise => &config.integrations.mise,
            Integration::Fnox => &config.integrations.fnox,
            Integration::Tailscale => &config.integrations.tailscale,
        }
    }
}

/// An integration's executable (resolved through the login shell's PATH) and its leading arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub program: String,
    pub args: Vec<String>,
}

/// How an integration is used, decided from its setting and whether it's installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationState {
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

/// An integration the configuration turns on that isn't installed: what every operation needing it fails with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error(
    "{} is enabled ([integrations.{}] enabled = true) but {program:?} isn't installed on the login shell's PATH; \
     install it ({}) or turn it off",
    integration.name(),
    integration.name(),
    integration.install()
)]
pub struct Unavailable {
    pub integration: Integration,
    pub program: String,
}

impl IntegrationState {
    /// For an operation that runs the integration when it's used and does without it when it's off.
    pub fn optional(&self) -> Result<Option<&Program>, Unavailable> {
        match self {
            IntegrationState::Used(program) => Ok(Some(program)),
            IntegrationState::Off(_) => Ok(None),
            IntegrationState::Unavailable(unavailable) => Err(unavailable.clone()),
        }
    }
}

/// Decides `integration`'s state. `env` is the environment its command is found in: the login shell's.
pub fn resolve(integration: Integration, config: &DaemonConfig, env: &Env) -> IntegrationState {
    let setting = integration.config(config);
    let (name, args) = setting
        .command
        .split_first()
        .map(|(name, args)| (name.as_str(), args.to_vec()))
        .unwrap_or((integration.name(), Vec::new()));
    let found = || {
        let program = resolve_program(name, env);
        is_executable(std::path::Path::new(&program)).then(|| Program {
            program,
            args: args.clone(),
        })
    };
    match setting.choice {
        IntegrationChoice::Off => IntegrationState::Off(OffReason::Disabled),
        IntegrationChoice::Auto => match found() {
            Some(program) => IntegrationState::Used(program),
            None => IntegrationState::Off(OffReason::NotInstalled),
        },
        IntegrationChoice::On => match found() {
            Some(program) => IntegrationState::Used(program),
            None => IntegrationState::Unavailable(Unavailable {
                integration,
                program: name.to_string(),
            }),
        },
    }
}

/// The state of each integration, decided once from the configuration and the login shell's PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrations {
    pub mise: IntegrationState,
    pub fnox: IntegrationState,
    pub tailscale: IntegrationState,
}

impl Integrations {
    pub fn resolve(config: &DaemonConfig, env: &Env) -> Self {
        Self {
            mise: resolve(Integration::Mise, config, env),
            fnox: resolve(Integration::Fnox, config, env),
            tailscale: resolve(Integration::Tailscale, config, env),
        }
    }

    pub fn get(&self, integration: Integration) -> &IntegrationState {
        match integration {
            Integration::Mise => &self.mise,
            Integration::Fnox => &self.fnox,
            Integration::Tailscale => &self.tailscale,
        }
    }

    /// One entry per integration for the daemon's log.
    pub fn describe(&self) -> String {
        Integration::ALL
            .iter()
            .map(|&integration| {
                let state = match self.get(integration) {
                    IntegrationState::Used(program) => program.program.clone(),
                    IntegrationState::Off(OffReason::Disabled) => "off".into(),
                    IntegrationState::Off(OffReason::NotInstalled) => "not installed (off)".into(),
                    IntegrationState::Unavailable(_) => "ENABLED BUT NOT INSTALLED".into(),
                };
                format!("{}={state}", integration.name())
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

    /// Every setting against installed and missing, for every integration.
    #[test]
    fn every_setting_and_availability() {
        let dir = tempfile::tempdir().unwrap();
        for integration in Integration::ALL {
            let installed = path_with(dir.path(), integration.name());
            let missing = Env::new();
            let used = IntegrationState::Used(Program {
                program: dir.path().join(integration.name()).to_string_lossy().into_owned(),
                args: vec![],
            });
            let table = |enabled: &str| format!("[integrations.{}]\n{enabled}", integration.name());
            let cases = [
                (table(""), &installed, used.clone()),
                (table(""), &missing, IntegrationState::Off(OffReason::NotInstalled)),
                (
                    table("enabled = false\n"),
                    &installed,
                    IntegrationState::Off(OffReason::Disabled),
                ),
                (
                    table("enabled = false\n"),
                    &missing,
                    IntegrationState::Off(OffReason::Disabled),
                ),
                (table("enabled = true\n"), &installed, used.clone()),
                (
                    table("enabled = true\n"),
                    &missing,
                    IntegrationState::Unavailable(Unavailable {
                        integration,
                        program: integration.name().into(),
                    }),
                ),
            ];
            for (host, env, expected) in cases {
                let config = DaemonConfig::from_host_toml(&host);
                assert_eq!(
                    resolve(integration, &config, env),
                    expected,
                    "{} {host:?}",
                    integration.name()
                );
            }
        }
    }

    #[test]
    fn a_command_with_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let env = path_with(dir.path(), "my-mise");
        let config = DaemonConfig::from_host_toml("[integrations.mise]\ncommand = [\"my-mise\", \"--quiet\"]\n");
        assert_eq!(
            resolve(Integration::Mise, &config, &env),
            IntegrationState::Used(Program {
                program: dir.path().join("my-mise").to_string_lossy().into_owned(),
                args: vec!["--quiet".into()],
            })
        );
    }

    /// An operation that can do without an integration runs it when used, skips it when off, and fails when
    /// unavailable.
    #[test]
    fn optional_use() {
        let integrations = Integrations::resolve(
            &DaemonConfig::from_host_toml("[integrations.tailscale]\nenabled = true\n"),
            &Env::new(),
        );
        assert_eq!(integrations.mise.optional(), Ok(None));
        let error = integrations.tailscale.optional().unwrap_err();
        assert_eq!(error.integration, Integration::Tailscale);
        assert!(error.to_string().contains("[integrations.tailscale] enabled = true"));
    }
}
