//! Piton's built-in sandbox policy, as data: what a sandboxed pi may write with no configuration, what it may never
//! read, which entries of pi's agent directory are runtime state, and the toolchain presets.
//!
//! The model is the one other harnesses converged on: read everything, write narrowly, deny credentials. The
//! credential and private-data denies are nono's `deny_*` groups, vendored verbatim (`nono-deny-groups.json`); the
//! `cli:nono-policy-check` task reports upstream changes.

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{ClaudeLogin, Connect, Preset, Profile, ServiceEntry};

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
    /// A file fnox loads for this launch (`fnox config-files`), or a folder on its path: which secrets go where. Never
    /// writable, and never lifted.
    FnoxConfig,
    /// pi's runtime state in its agent directory.
    AgentDir,
    /// A preset, and where it was listed.
    Preset { preset: String, from: Provenance },
    /// A `[sandbox]` list entry, and where it was listed.
    Config { key: String, from: Provenance },
}

/// Where a `[sandbox]` entry came from: the profile in effect, or a configuration file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Provenance {
    Profile { profile: Profile },
    File { path: PathBuf },
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Provenance::Profile { profile } => write!(f, "from the {} profile", profile.name()),
            Provenance::File { path } => write!(f, "listed in {}", path.display()),
        }
    }
}

/// A profile's entries, as written: added before the user's own lists. Profiles only add: `strict` adds nothing, and
/// each profile holds the one before it. The network filter (#228) enforces what `connect` here grants to hosts.
pub struct ProfileEntries {
    pub read: Vec<ReadEntry>,
    pub connect: Vec<Connect>,
    pub unix_bind: &'static [&'static str],
    /// `tcp_listen = "*"`: listening on TCP, on every address.
    pub tcp_listen: bool,
    pub services: Vec<ServiceEntry>,
    pub presets: Vec<Preset>,
}

/// A profile's `read` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadEntry {
    /// A path, as written (`~/...`).
    Path(&'static str),
    /// Every SSH public key (`*.pub`) under `~/.ssh`, found when the launch is resolved: with `IdentitiesOnly yes`,
    /// ssh picks the agent's key through the public half beside the configured identity.
    SshPublicKeys,
}

pub fn profile_entries(profile: Profile) -> ProfileEntries {
    let entry = |text: &str| Connect::try_from(text.to_string()).expect("a profile's connect entries are valid");
    // Git over SSH: the user's SSH agent (keys used, never read), the host names and keys SSH checks, and the public
    // halves of the keys. Creating sockets anywhere and listening on TCP: test servers, dev servers, IPC, language
    // servers. Every Mach service but the keychain's, as the library allows: toolchains and GUI-adjacent tools reach
    // services no fixed list foresees.
    let standard = ProfileEntries {
        read: vec![
            ReadEntry::Path("~/.ssh/config"),
            ReadEntry::Path("~/.ssh/known_hosts"),
            ReadEntry::SshPublicKeys,
        ],
        connect: vec![entry("$SSH_AUTH_SOCK"), entry("*")],
        unix_bind: &["/"],
        tcp_listen: true,
        services: vec![ServiceEntry::Every],
        presets: Vec::new(),
    };
    match profile {
        Profile::Strict => ProfileEntries {
            read: Vec::new(),
            connect: Vec::new(),
            unix_bind: &[],
            tcp_listen: false,
            services: Vec::new(),
            presets: Vec::new(),
        },
        Profile::Standard => standard,
        Profile::Yolo => {
            let mut yolo = standard;
            // Every Unix socket, Git's control files, and launching apps.
            yolo.connect.push(entry("/"));
            yolo.presets.push(Preset::Git);
            yolo.presets.push(Preset::Apps);
            yolo
        }
    }
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

/// Whether `relative` (a path inside pi's agent directory, relative to it) is runtime state pi writes itself
/// ([`AGENT_DIR_WRITES`]), compared without case as the sandbox compares. Everything else there is configuration or
/// code: a protected file (`agent/protected.rs`).
pub fn agent_dir_runtime(relative: &std::path::Path) -> bool {
    let mut parts = relative.components().filter_map(|c| match c {
        std::path::Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
        std::path::Component::Prefix(_)
        | std::path::Component::RootDir
        | std::path::Component::CurDir
        | std::path::Component::ParentDir => None,
    });
    let Some(first) = parts.next() else {
        return false;
    };
    let nested = parts.next().is_some();
    AGENT_DIR_WRITES.iter().any(|write| match write {
        AgentDirWrite::Folder(name) => first == *name,
        AgentDirWrite::File(name) => first == *name && !nested,
        AgentDirWrite::Lock(name) => first == format!("{name}.lock"),
        AgentDirWrite::NamePrefix(prefix) => first.starts_with(prefix),
    })
}

/// The Mach services every launch may look up, each with why: what command-line tools reach without a window. Claude
/// Code's list for its command sandbox, plus what Piton's whole-process sandbox needs for pi, its MCP servers, and
/// their tools (TLS, proxies and reachability, preferences). Anything else is a `services` entry, or `standard`'s `*`.
pub const BASE_SERVICES: &[(&str, &str)] = &[
    ("com.apple.logd", "logging"),
    ("com.apple.system.logger", "logging (syslog)"),
    ("com.apple.diagnosticd", "logging (os_log activity)"),
    ("com.apple.analyticsd", "crash and usage reports the runtime files"),
    (
        "com.apple.system.opendirectoryd.libinfo",
        "user and group names (getpwuid, id)",
    ),
    ("com.apple.system.opendirectoryd.membership", "group membership"),
    ("com.apple.bsd.dirhelper", "the per-user temp and cache folders"),
    (
        "com.apple.system.notification_center",
        "notify(3): time zone and network changes",
    ),
    ("com.apple.distributed_notifications@Uv3", "distributed notifications"),
    ("com.apple.FontObjectsServer", "fonts"),
    ("com.apple.fonts", "fonts"),
    (
        "com.apple.PowerManagement.control",
        "power assertions (keeping the Mac awake)",
    ),
    ("com.apple.audio.systemsoundserver", "system sounds"),
    ("com.apple.securityd.xpc", "code signature checks"),
    ("com.apple.trustd", "TLS certificate checks"),
    ("com.apple.trustd.agent", "TLS certificate checks (Go, curl, git)"),
    (
        "com.apple.SystemConfiguration.configd",
        "proxy settings and reachability",
    ),
    (
        "com.apple.SystemConfiguration.DNSConfiguration",
        "the DNS configuration name resolvers read (Node's at startup)",
    ),
    ("com.apple.cfprefsd.daemon", "preferences"),
    ("com.apple.cfprefsd.agent", "preferences"),
    (
        "com.apple.lsd.mapdb",
        "Launch Services' read-only database: what app handles a type",
    ),
    (
        "com.apple.coreservices.launchservicesd",
        "Launch Services lookups (not launching: that's `lsopen`)",
    ),
];

/// The Mach services the `apps` preset adds besides `lsopen` and Apple Events: what `open` and `osascript` reach.
pub const APP_SERVICES: &[&str] = &[
    // What `open` looked up in `strict`, from the kernel's record (#249's host probe).
    "com.apple.lsd.open",
    "com.apple.DiskArbitration.diskarbitrationd",
    "com.apple.coreservices.sharedfilelistd.xpc",
    "com.apple.metadata.mds.legacy",
    "com.apple.coreservices.appleevents",
    "com.apple.CoreServices.coreservicesd",
    "com.apple.coreservices.quarantine-resolver",
];

/// What a preset grants, as written (`~/...`): added to the `[sandbox]` lists of the same names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetGrants {
    pub write: Vec<&'static str>,
    pub write_file: Vec<&'static str>,
    pub read: Vec<&'static str>,
    pub connect: Vec<Connect>,
    /// Mach services, by exact name.
    pub services: Vec<&'static str>,
    /// Listening on TCP, on every address.
    pub tcp_listen: bool,
}

/// The Mach services that broker keychain access (the library denies them).
pub const KEYCHAIN_TARGETS: [&str; 5] = [
    "com.apple.SecurityServer",
    "com.apple.securityd",
    "com.apple.security.keychaind",
    "com.apple.secd",
    "com.apple.security.agent",
];

/// The keychains: what `login = "keychain"` reads (which reopens the keychain's services).
pub const KEYCHAINS: &str = "~/Library/Keychains";

/// Where `claude` caches its logs (env-paths' cache folder, per working folder): it doesn't follow `CLAUDE_CONFIG_DIR`.
pub const CLAUDE_CODE_CACHE: &str = "~/Library/Caches/claude-cli-nodejs";

/// What a preset grants. Each toolchain's are its caches and state; host builds reuse what the sandbox writes there,
/// which is why none is on by default. None of them writes what the host executes or trusts (checked below): mise's
/// installed tools and trusted configs, cargo's installed binaries, rustup's toolchains. An install that needs them is
/// asked about and run by the daemon (#241). `git` grants no path: it leaves out the Git rule instead (`resolve.rs`).
/// `apps` grants its services here, and `resolve.rs` opens launching and Apple Events.
/// `pi-claude-code`'s config folder and its lock (`<folder>.lock` beside it: `claude` locks the folder itself with
/// `proper-lockfile`) come from the launch's environment, so `resolve.rs` grants them; here are its fixed parts.
pub fn preset_grants(preset: &Preset) -> PresetGrants {
    let writes = |write: &[&'static str], write_file: &[&'static str]| PresetGrants {
        write: write.to_vec(),
        write_file: write_file.to_vec(),
        ..PresetGrants::default()
    };
    match preset {
        Preset::Git => PresetGrants::default(),
        Preset::Apps => PresetGrants {
            services: APP_SERVICES.to_vec(),
            ..PresetGrants::default()
        },
        Preset::Mise => writes(
            &[
                "~/Library/Caches/mise",
                "~/.local/state/mise/incomplete-installs",
                "~/.local/state/mise/tracked-configs",
            ],
            &[],
        ),
        Preset::Rust => writes(
            &[
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
        Preset::Node => writes(
            &[
                "~/.npm",
                "~/Library/pnpm",
                "~/.local/share/pnpm",
                "~/Library/Caches/pnpm",
                "~/.bun/install/cache",
                "~/Library/Caches/Yarn",
            ],
            &[],
        ),
        Preset::Swift => writes(
            &[
                "~/Library/Caches/org.swift.swiftpm",
                "~/Library/org.swift.swiftpm",
                "~/.cache/clang/ModuleCache",
                "~/Library/Developer/Xcode/DerivedData",
            ],
            &[],
        ),
        Preset::PiClaudeCode(options) => PresetGrants {
            write: vec![CLAUDE_CODE_CACHE],
            // The extension's relay listens on 127.0.0.1 at a random port in pi's process (listening can't be limited to
            // loopback: every address), and `claude` connects to it; it reaches Anthropic from pi's process.
            connect: vec![Connect::Network(crate::config::NetworkTarget::LocalAnyPort)],
            tcp_listen: true,
            // Keychain sign-in goes through the keychain's services, not file writes: the files stay unwritable.
            read: match options.login() {
                ClaudeLogin::Token => Vec::new(),
                ClaudeLogin::Keychain => vec![KEYCHAINS],
            },
            write_file: Vec::new(),
            services: Vec::new(),
        },
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
            let grants = preset_grants(&preset);
            match preset {
                Preset::Git => assert_eq!(grants, PresetGrants::default(), "git grants no path"),
                Preset::Apps => {
                    assert_eq!(grants.services, APP_SERVICES, "apps grants services, no path");
                    assert!(grants.write.is_empty() && grants.read.is_empty());
                }
                Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift | Preset::PiClaudeCode(_) => {
                    assert!(!grants.write.is_empty(), "{}", preset.name());
                }
            }
            match preset {
                Preset::PiClaudeCode(_) => assert!(grants.tcp_listen),
                Preset::Git | Preset::Apps | Preset::Mise | Preset::Rust | Preset::Node | Preset::Swift => {
                    assert!(!grants.tcp_listen, "{}", preset.name());
                }
            }
            let paths = grants.write.iter().chain(&grants.write_file).chain(&grants.read);
            assert!(paths.into_iter().all(|p| p.starts_with("~/")));
        }
    }

    /// `pi-claude-code` reads the keychain only with `login = "keychain"`, and never writes it.
    #[test]
    fn the_claude_code_preset_opens_the_keychain_only_for_its_keychain_login() {
        use crate::config::ClaudeCodeOptions;
        let preset = |login| preset_grants(&Preset::PiClaudeCode(ClaudeCodeOptions { login }));
        for login in [None, Some(ClaudeLogin::Token)] {
            assert!(preset(login).read.is_empty());
        }
        let keychain = preset(Some(ClaudeLogin::Keychain));
        assert_eq!(keychain.read, [KEYCHAINS]);
        for login in [None, Some(ClaudeLogin::Token), Some(ClaudeLogin::Keychain)] {
            let grants = preset(login);
            assert_eq!(grants.write, [CLAUDE_CODE_CACHE]);
            assert!(grants.write_file.is_empty());
            assert_eq!(grants.connect, [Connect::try_from("localhost:*".to_string()).unwrap()]);
            assert!(grants.tcp_listen, "its relay listens");
        }
    }

    /// No preset writes what the host executes or trusts: a sandboxed agent could then run code on the host.
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
        for preset in &Preset::ALL {
            let grants = preset_grants(preset);
            for grant in grants.write.iter().chain(&grants.write_file) {
                for path in host_runs {
                    assert!(
                        !covers(grant, path),
                        "{} grants {grant}, which covers {path}",
                        preset.name()
                    );
                }
            }
        }
    }
}
