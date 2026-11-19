//! Piton's built-in sandbox policy, as data: what a sandboxed pi may write with no configuration, what it may never
//! read, which entries of pi's agent directory are runtime state, and the toolchain presets.
//!
//! The model is the one other harnesses converged on: read everything, write narrowly, deny credentials. The
//! credential and private-data denies are nono's `deny_*` groups, vendored verbatim (`nono-deny-groups.json`); the
//! `cli:nono-policy-check` task reports upstream changes.

use serde::Deserialize;

use crate::config::{InstallMode, Preset};

/// Where a deny or a grant comes from: what `piton sandbox why` reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// The workspace pi runs in.
    Workspace,
    /// A write root every launch gets (temp folders, the workflow artifacts).
    BuiltIn { what: String },
    /// One of nono's vendored deny groups.
    NonoGroup { group: String },
    /// Piton's own deny: the daemon's state folder or control socket.
    Piton { what: String },
    /// pi's runtime state in its agent directory.
    AgentDir,
    /// A shipped toolchain preset.
    Preset { preset: String },
    /// A `[sandbox]` key in a configuration file.
    Config { key: String },
}

/// The vendored copy of nono-cli's deny groups (`data/policy.json`, Apache-2.0).
const NONO_DENY_GROUPS: &str = include_str!("nono-deny-groups.json");

#[derive(Debug, Deserialize)]
struct Vendored {
    source: VendoredSource,
    groups: std::collections::BTreeMap<String, Group>,
}

#[derive(Debug, Deserialize)]
struct VendoredSource {
    version: String,
}

#[derive(Debug, Deserialize)]
struct Group {
    #[serde(default)]
    platform: Option<String>,
    deny: GroupDeny,
}

#[derive(Debug, Deserialize)]
struct GroupDeny {
    access: Vec<String>,
}

/// The nono version the deny groups were copied from.
pub fn nono_version() -> String {
    vendored().source.version
}

fn vendored() -> Vendored {
    serde_json::from_str(NONO_DENY_GROUPS).expect("the vendored nono deny groups are valid JSON (checked by tests)")
}

/// Every built-in deny for macOS: nono's macOS and cross-platform deny groups, as written (`~/...`).
pub fn nono_denies() -> Vec<(String, Source)> {
    vendored()
        .groups
        .into_iter()
        .filter(|(_, group)| matches!(group.platform.as_deref(), None | Some("macos")))
        .flat_map(|(name, group)| {
            group
                .deny
                .access
                .into_iter()
                .map(move |path| (path, Source::NonoGroup { group: name.clone() }))
        })
        .collect()
}

/// Folders every sandboxed launch may write besides the workspace: the system temp folders (`$TMPDIR` is under
/// `/private/var/folders`) and the device nodes programs write to (nono's `system_write_macos`).
pub const WRITE_ROOTS: &[&str] = &["/private/tmp", "/private/var/folders", "/dev"];

/// One entry of pi's agent directory a sandboxed pi may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDirWrite {
    /// A folder of runtime state and everything in it.
    Folder(&'static str),
    /// A runtime file pi writes in place.
    File(&'static str),
    /// The `proper-lockfile` lock of a file (`<file>.lock`): an empty folder pi creates, touches, and removes. pi takes
    /// it to read the file too, so a read-only file still needs its lock.
    Lock(&'static str),
    /// Every entry whose name starts with this: locks named per key.
    NamePrefix(&'static str),
}

/// What a sandboxed pi writes in its agent directory, from pi 1.5's source (docs/pi-findings.md, "pi's agent
/// directory"): runtime state (sessions, credentials, the models store, logs), none of which decides what pi loads, and
/// the locks pi takes. Everything else there (`settings.json`, `trust.json`, `mcp.json`, `extensions/`, skills,
/// prompts, themes, installed packages, managed binaries) is code or configuration a later pi loads, host sessions
/// included, so it stays read-only.
pub const AGENT_DIR_WRITES: &[AgentDirWrite] = &[
    AgentDirWrite::Folder("sessions"),
    AgentDirWrite::File("auth.json"),
    AgentDirWrite::Lock("auth.json"),
    AgentDirWrite::File("models-store.json"),
    AgentDirWrite::Lock("models-store.json"),
    AgentDirWrite::File("mcp-auth.json"),
    AgentDirWrite::Lock("mcp-auth.json"),
    // One lock per MCP server while its OAuth token refreshes: `mcp-auth-refresh-<hash>.lock`.
    AgentDirWrite::NamePrefix("mcp-auth-refresh-"),
    AgentDirWrite::File("crashes.json"),
    AgentDirWrite::File("mcp.log"),
    // `mcp.log` rotated.
    AgentDirWrite::File("mcp.log.1"),
    // Read at startup: without its lock pi runs without the global settings (packages, extensions, default model).
    AgentDirWrite::Lock("settings.json"),
    // Read at startup when the workspace has `.pi/` resources or `.agents/skills`: without its lock pi exits.
    AgentDirWrite::Lock("trust.json"),
];

/// The folders mise installs tools into: what the host runs. Written only with `install = "allow"`.
pub const MISE_INSTALL_DIRS: &[&str] = &["~/.local/share/mise/installs", "~/.local/share/mise/downloads"];

/// What a preset grants: folders, then files, as written (`~/...`). Each toolchain's are its caches and state; host
/// builds reuse what the sandbox writes there, which is why none is on by default. None of them, without options,
/// writes what the host executes or trusts (checked below): mise's installed tools and trusted configs, cargo's
/// installed binaries, rustup's toolchains. `git` grants no path: it leaves out the Git rule instead (`resolve.rs`).
pub fn preset_grants(preset: &Preset) -> (Vec<&'static str>, &'static [&'static str]) {
    match preset {
        Preset::Git => (Vec::new(), &[]),
        Preset::Mise(options) => {
            let mut dirs = vec![
                "~/Library/Caches/mise",
                "~/.local/state/mise/incomplete-installs",
                "~/.local/state/mise/tracked-configs",
            ];
            match options.install {
                Some(InstallMode::Allow) => dirs.extend(MISE_INSTALL_DIRS),
                // Installs that ask go through the host (#241); until then they fail in the sandbox, like `deny`.
                Some(InstallMode::Ask | InstallMode::Deny) | None => {}
            }
            (dirs, &[])
        }
        Preset::Rust => (
            vec![
                "~/.cargo/registry",
                "~/.cargo/git",
                "~/.rustup/tmp",
                "~/.rustup/downloads",
                "~/.rustup/update-hashes",
            ],
            &[
                "~/.cargo/.package-cache",
                "~/.cargo/.package-cache-mutate",
                "~/.cargo/.global-cache",
            ],
        ),
        Preset::Node => (
            vec![
                "~/.npm",
                "~/Library/pnpm",
                "~/.local/share/pnpm",
                "~/Library/Caches/pnpm",
                "~/.bun/install/cache",
                "~/Library/Caches/Yarn",
            ],
            &[],
        ),
        Preset::Swift => (
            vec![
                "~/Library/Caches/org.swift.swiftpm",
                "~/Library/org.swift.swiftpm",
                "~/.cache/clang/ModuleCache",
                "~/Library/Developer/Xcode/DerivedData",
            ],
            &[],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendored_groups_parse_and_cover_credentials() {
        let denies = nono_denies();
        let paths: Vec<&str> = denies.iter().map(|(p, _)| p.as_str()).collect();
        for path in ["~/.ssh", "~/.aws", "~/Library/Keychains", "~/.zsh_history", "~/.zshrc"] {
            assert!(paths.contains(&path), "{path}");
        }
        // Linux groups are left out.
        assert!(!paths.contains(&"~/.local/share/keyrings"));
        assert!(denies.iter().all(|(p, _)| p.starts_with("~/") || p.starts_with('/')));
    }

    /// Bumping the `nono` crate without re-vendoring its deny groups fails here (`mise run cli:nono-policy-check`).
    #[test]
    fn the_vendored_groups_match_the_nono_dependency() {
        let lock = include_str!("../../../Cargo.lock");
        let version = lock
            .split("[[package]]")
            .find(|p| p.contains("name = \"nono\"\n"))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
            .and_then(|v| v.strip_suffix('"'))
            .expect("nono is in Cargo.lock");
        assert_eq!(
            nono_version(),
            version,
            "re-vendor nono's deny groups: mise run cli:nono-policy-check"
        );
    }

    #[test]
    fn presets_grant_paths_under_home() {
        for preset in Preset::ALL {
            let (dirs, files) = preset_grants(&preset);
            match preset {
                Preset::Git => assert!(dirs.is_empty() && files.is_empty(), "git grants no path"),
                Preset::Mise(_) | Preset::Rust | Preset::Node | Preset::Swift => {
                    assert!(!dirs.is_empty(), "{}", preset.name());
                }
            }
            assert!(dirs.iter().chain(files).all(|p| p.starts_with("~/")));
        }
    }

    /// No preset, with its default options, writes what the host executes or trusts: a sandboxed agent could then run
    /// code on the host. Only `mise`'s `install = "allow"` (global configuration only) writes installed tools.
    #[test]
    fn presets_never_write_what_the_host_runs() {
        use std::path::Path;
        let host_runs = [
            "~/.local/share/mise/installs",
            "~/.local/share/mise/downloads",
            "~/.local/state/mise/trusted-configs",
            "~/.local/share/mise/shims",
            "~/.cargo/bin",
            "~/.rustup/toolchains",
            "~/.config/mise",
        ];
        let covers = |grant: &str, path: &str| Path::new(path).starts_with(grant) || Path::new(grant).starts_with(path);
        let preset = |text: &str| -> Preset {
            toml::from_str::<toml::Table>(text).unwrap()["p"]
                .clone()
                .try_into()
                .unwrap()
        };
        let mut presets = Preset::ALL.to_vec();
        for install in ["ask", "deny"] {
            presets.push(preset(&format!("p = {{ name = \"mise\", install = \"{install}\" }}")));
        }
        for preset in &presets {
            let (dirs, files) = preset_grants(preset);
            for grant in dirs.iter().chain(files) {
                for path in host_runs {
                    assert!(
                        !covers(grant, path),
                        "{} grants {grant}, which covers {path}",
                        preset.name()
                    );
                }
            }
        }
        let allowed = preset("p = { name = \"mise\", install = \"allow\" }");
        assert!(MISE_INSTALL_DIRS.iter().all(|d| preset_grants(&allowed).0.contains(d)));
    }
}
