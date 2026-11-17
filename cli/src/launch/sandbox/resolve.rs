//! One launch's [`Policy`]: Piton's built-in policy, the merged `[sandbox]` chain, and the launch's facts (workspace,
//! agent directory, the daemon's state folder), as an ordered list of literal rules.
//!
//! Order, the last matching rule deciding:
//! 1. writes: the workspace, the source's `.piton/.local`, temp, presets, `allow`, `allow_file`;
//! 2. nono's credential denies; then `expose`, which lifts them (and reopens writes granted inside an exposed path);
//! 3. the host agent directory is read-only; the launch's agent directory's runtime state is writable;
//! 4. the user's `deny`;
//! 5. every configuration level, and what its entries link to, is read-only; protected files are read-only;
//! 6. the daemon's state folder is closed (reads, writes, connects) except its logs and the agent socket, and its
//!    control socket can't be connected to, wherever it is.

use std::path::{Component, Path, PathBuf};

use super::{
    SandboxError,
    policy::{self, AgentDirWrite, Source},
    rules::{Access, Effect, Policy, Rule, Target},
};
use crate::{
    agent::canonical_target,
    config::{LOCAL_DIR, LaunchConfig, expand_home},
    launch::Env,
};

/// What a launch is, beyond its configuration.
pub struct Facts<'a> {
    pub home: &'a Path,
    /// The daemon's base environment: `$VAR` grants and the host agent directory come from it, never from the
    /// workspace's (a repository's `mise.toml` can set variables).
    pub base_env: &'a Env,
    /// pi's environment for this launch: the agent directory pi will use.
    pub launch_env: &'a Env,
    /// The workspace pi runs in.
    pub workspace: &'a Path,
    /// The daemon's state folder (`agent.sock`, `piton.sock`, the database, `logs/`).
    pub state_dir: &'a Path,
    /// The daemon's control socket: clients only, never an agent.
    pub control_socket: &'a Path,
    pub agent_socket: &'a Path,
    pub logs: &'a Path,
    /// The temp and device folders every launch may write: [`policy::WRITE_ROOTS`] (tests pass none, since their
    /// folders are temp folders).
    pub write_roots: &'a [&'a str],
}

const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// The launch's policy, or why the launch is refused.
pub fn resolve(config: &LaunchConfig, facts: &Facts) -> Result<Policy, SandboxError> {
    let table = &config.sandbox;
    let expand = |grant: &str| expand_grant(grant, facts.home, facts.base_env);
    let mut rules = Rules::default();
    let config_source = |key: &str| Source::Config {
        key: format!("sandbox.{key}"),
    };

    // 1. Writes.
    rules.push(
        Effect::Allow,
        &[Access::Write],
        subpath(facts.workspace),
        Source::Workspace,
    );
    let artifacts = config.source.join(LOCAL_DIR);
    rules.push(
        Effect::Allow,
        &[Access::Write],
        subpath(&artifacts),
        Source::BuiltIn {
            what: "the source's workflow artifacts (.piton/.local)".into(),
        },
    );
    for root in facts.write_roots {
        rules.push(
            Effect::Allow,
            &[Access::Write],
            subpath(Path::new(root)),
            Source::BuiltIn {
                what: "temporary files and devices".into(),
            },
        );
    }
    for preset in &table.presets {
        let (dirs, files) = policy::preset_grants(preset);
        let source = Source::Preset {
            preset: preset.name().into(),
        };
        for dir in dirs {
            rules.push(Effect::Allow, &[Access::Write], subpath(&expand(dir)?), source.clone());
        }
        for file in files {
            rules.push(Effect::Allow, &[Access::Write], literal(&expand(file)?), source.clone());
        }
    }
    let allow: Vec<PathBuf> = table.allow.iter().map(|g| expand(g)).collect::<Result<_, _>>()?;
    let allow_file: Vec<PathBuf> = table.allow_file.iter().map(|g| expand(g)).collect::<Result<_, _>>()?;
    for path in &allow {
        rules.push(Effect::Allow, &[Access::Write], subpath(path), config_source("allow"));
    }
    for path in &allow_file {
        rules.push(
            Effect::Allow,
            &[Access::Write],
            literal(path),
            config_source("allow_file"),
        );
    }

    // 2. Credentials, and what the user exposes of them.
    let all = [Access::Read, Access::Write, Access::Connect];
    let mut denied = Vec::new();
    for (path, source) in policy::nono_denies() {
        let path = expand(&path)?;
        rules.push(Effect::Deny, &all, subpath(&path), source);
        denied.push(path);
    }
    let exposed: Vec<PathBuf> = table.expose.iter().map(|g| expand(g)).collect::<Result<_, _>>()?;
    for path in &exposed {
        rules.push(
            Effect::Allow,
            &[Access::Read, Access::Connect],
            subpath(path),
            config_source("expose"),
        );
    }
    for path in allow.iter().filter(|p| exposed.iter().any(|e| p.starts_with(e))) {
        rules.push(Effect::Allow, &[Access::Write], subpath(path), config_source("allow"));
    }
    for path in allow_file.iter().filter(|p| exposed.iter().any(|e| p.starts_with(e))) {
        rules.push(
            Effect::Allow,
            &[Access::Write],
            literal(path),
            config_source("allow_file"),
        );
    }
    let user_denied: Vec<PathBuf> = table.deny.iter().map(|g| expand(g)).collect::<Result<_, _>>()?;
    denied.extend(user_denied.iter().cloned());

    // 3. pi's agent directories.
    let host_agent = agent_dir(facts.base_env, facts.home);
    let agent = agent_dir(facts.launch_env, facts.home);
    check_agent_dir(&agent, &host_agent, config, facts, &denied)?;
    rules.push(
        Effect::Deny,
        &[Access::Write],
        subpath(&host_agent),
        Source::Piton {
            what: "pi's agent directory: extensions, settings, and packages host sessions load".into(),
        },
    );
    if agent != host_agent {
        // A per-folder agent directory (even one inside the workspace) is loaded by host sessions in that folder too.
        rules.push(
            Effect::Deny,
            &[Access::Write],
            subpath(&agent),
            Source::Piton {
                what: "pi's agent directory: extensions, settings, and packages pi loads".into(),
            },
        );
    }
    for write in policy::AGENT_DIR_WRITES {
        let target = match write {
            AgentDirWrite::Folder(name) => subpath(&agent.join(name)),
            AgentDirWrite::File(name) => literal(&agent.join(name)),
            AgentDirWrite::Lock(name) => subpath(&agent.join(format!("{name}.lock"))),
            AgentDirWrite::NamePrefix(name) => Target::Prefix { path: agent.join(name) },
        };
        rules.push(Effect::Allow, &[Access::Write], target, Source::AgentDir);
    }

    // 4. The user's denies.
    for path in &user_denied {
        rules.push(Effect::Deny, &all, subpath(path), config_source("deny"));
    }

    // 5. Configuration and protected files.
    let config_level = Source::Piton {
        what: "a configuration level".into(),
    };
    let own_piton = config.source.join(crate::config::PITON_DIR);
    let own_agents = config.checkout.join(crate::config::AGENTS_DIR);
    for level in &config.levels {
        // The source's `.piton/` is a protected root (below); the workspace's own `.agents/` is agent-editable.
        if *level == own_piton || *level == own_agents {
            continue;
        }
        rules.push(Effect::Deny, &[Access::Write], subpath(level), config_level.clone());
    }
    for target in link_targets(config, &own_agents) {
        rules.push(Effect::Deny, &[Access::Write], subpath(&target), config_level.clone());
    }
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    rules.rules.push(Rule {
        effect: Effect::Deny,
        access: vec![Access::Write],
        target: Target::Protected {
            source: canonical(&config.source),
            checkout: canonical(&config.checkout),
        },
        source: Source::Piton {
            what: "a protected file (.piton/ or .pi/): changed only with the user's approval".into(),
        },
    });

    // 6. The daemon.
    rules.push(
        Effect::Deny,
        &all,
        subpath(facts.state_dir),
        Source::Piton {
            what: "the daemon's state folder".into(),
        },
    );
    rules.push(
        Effect::Allow,
        &[Access::Read],
        subpath(facts.logs),
        Source::Piton {
            what: "Piton's logs".into(),
        },
    );
    rules.push(
        Effect::Allow,
        &[Access::Connect],
        literal(facts.agent_socket),
        Source::Piton {
            what: "the agent socket (protected-write requests)".into(),
        },
    );
    rules.push(
        Effect::Deny,
        &[Access::Connect],
        literal(facts.control_socket),
        Source::Piton {
            what: "the daemon's control socket (clients only)".into(),
        },
    );

    let keychains = facts.home.join("Library/Keychains");
    let keychain = exposed
        .iter()
        .any(|e| keychains.starts_with(e) || e.starts_with(&keychains));
    Ok(Policy {
        rules: rules.rules,
        keychain,
    })
}

#[derive(Default)]
struct Rules {
    rules: Vec<Rule>,
}

impl Rules {
    /// Adds a rule, and the same rule for the canonical path when it differs: Seatbelt compares resolved paths
    /// (`/var` is `/private/var`), a connect may compare the path as given.
    fn push(&mut self, effect: Effect, access: &[Access], target: Target, source: Source) {
        let canonical = match &target {
            Target::Subpath { path } => Some(Target::Subpath {
                path: canonical_target(path),
            }),
            Target::Literal { path } => Some(Target::Literal {
                path: canonical_target(path),
            }),
            Target::Prefix { path } => Some(Target::Prefix {
                path: canonical_target(path),
            }),
            Target::Protected { .. } => None,
        };
        let rule = Rule {
            effect,
            access: access.to_vec(),
            target,
            source,
        };
        let canonical = canonical
            .filter(|c| *c != rule.target)
            .map(|target| Rule { target, ..rule.clone() });
        self.rules.push(rule);
        self.rules.extend(canonical);
    }
}

fn subpath(path: &Path) -> Target {
    Target::Subpath {
        path: path.to_path_buf(),
    }
}

fn literal(path: &Path) -> Target {
    Target::Literal {
        path: path.to_path_buf(),
    }
}

/// pi's agent directory in `env`: `PI_CODING_AGENT_DIR` (with `~`), else `~/.pi/agent`. Canonical, as Seatbelt
/// compares paths.
fn agent_dir(env: &Env, home: &Path) -> PathBuf {
    let dir = env
        .get(AGENT_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(|v| expand_home(v, home))
        .unwrap_or_else(|| home.join(".pi/agent"));
    canonical_target(&dir)
}

/// The launch's agent directory comes from the workspace's environment, so it can't be where it would open what the
/// sandbox otherwise closes.
fn check_agent_dir(
    agent: &Path,
    host_agent: &Path,
    config: &LaunchConfig,
    facts: &Facts,
    denied: &[PathBuf],
) -> Result<(), SandboxError> {
    let refuse = |reason: String| {
        Err(SandboxError::AgentDir {
            path: agent.to_path_buf(),
            reason,
        })
    };
    let overlaps = |other: &Path| agent.starts_with(other) || other.starts_with(agent);
    let home = canonical_target(facts.home);
    if home.starts_with(agent) {
        return refuse("it's the home folder or contains it".into());
    }
    if agent != host_agent && agent.starts_with(host_agent) {
        return refuse(format!(
            "it's inside the host's agent directory {}",
            host_agent.display()
        ));
    }
    // A project's `.pi/` (settings, extensions) anywhere in the checkout: pi loads it in that folder.
    let pi = std::ffi::OsStr::new(".pi");
    if let Ok(inside) = agent.strip_prefix(canonical_target(&config.checkout))
        && inside
            .components()
            .any(|c| matches!(c, Component::Normal(name) if name.eq_ignore_ascii_case(pi)))
    {
        return refuse("it's inside a .pi folder of the checkout, whose files pi loads".into());
    }
    if let Some(level) = config.levels.iter().map(|l| canonical_target(l)).find(|l| overlaps(l)) {
        return refuse(format!("it overlaps the configuration level {}", level.display()));
    }
    if overlaps(&canonical_target(facts.state_dir)) {
        return refuse("it overlaps the daemon's state folder".into());
    }
    if let Some(deny) = denied.iter().map(|d| canonical_target(d)).find(|d| overlaps(d)) {
        return refuse(format!("it overlaps the denied path {}", deny.display()));
    }
    Ok(())
}

/// A grant as a literal absolute path: `~` and `$HOME` from the user database, other `$VAR`s (a whole leading
/// segment, `$NAME` or `${NAME}`) from the daemon's base environment.
pub(super) fn expand_grant(grant: &str, home: &Path, env: &Env) -> Result<PathBuf, SandboxError> {
    let path = expand_home(grant, home).to_string_lossy().into_owned();
    let expanded = match path.strip_prefix('$') {
        None => path,
        Some(rest) => {
            let (name, tail) = match rest.strip_prefix('{') {
                Some(braced) => braced.split_once('}').unwrap_or((braced, "")),
                None => rest.split_at(rest.find('/').unwrap_or(rest.len())),
            };
            let value = env
                .get(name)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| SandboxError::UnsetVariable {
                    grant: grant.to_string(),
                    var: name.to_string(),
                })?;
            format!("{value}{tail}")
        }
    };
    if !expanded.starts_with('/') || expanded.contains('$') {
        return Err(SandboxError::NotAbsolute {
            grant: grant.to_string(),
            expanded,
        });
    }
    Ok(PathBuf::from(expanded))
}

/// The canonical targets of configuration entries that are symlinks (a global tier linked into a dotfiles
/// repository): writing through the link writes the target, so the target is closed too. The workspace's own
/// `.agents/` is agent-editable.
fn link_targets(config: &LaunchConfig, own_agents: &Path) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for level in config.levels.iter().filter(|level| *level != own_agents) {
        let Ok(entries) = std::fs::read_dir(level) else {
            continue;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            let is_link = path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
            if is_link
                && let Ok(target) = std::fs::canonicalize(&path)
                && !targets.contains(&target)
            {
                targets.push(target);
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Paths, Preset},
        project::{Relation, Workspace},
    };

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        repo: PathBuf,
        state: PathBuf,
    }

    fn fixture(global: &str, project: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let repo = home.join("p/repo");
        let state = home.join(".local/state/piton");
        for d in [
            home.join(".config/piton"),
            repo.join(".piton/.local"),
            state.join("logs"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(home.join(".config/piton/config.toml"), global).unwrap();
        std::fs::write(repo.join(".piton/config.toml"), project).unwrap();
        Fixture {
            _dir: dir,
            home,
            repo,
            state,
        }
    }

    fn launch_config(f: &Fixture) -> LaunchConfig {
        let paths = Paths {
            home: f.home.clone(),
            global: f.home.join(".config/piton"),
        };
        let ws = Workspace {
            path: f.repo.clone(),
            checkout: f.repo.clone(),
            relation: Relation::Source,
        };
        crate::config::resolve_launch(&ws, &paths).unwrap()
    }

    fn policy_with(f: &Fixture, base: &Env, launch: &Env) -> Result<Policy, SandboxError> {
        let control = f.state.join("piton.sock");
        let agent = f.state.join("agent.sock");
        let logs = f.state.join("logs");
        resolve(
            &launch_config(f),
            &Facts {
                home: &f.home,
                base_env: base,
                launch_env: launch,
                workspace: &f.repo,
                state_dir: &f.state,
                control_socket: &control,
                agent_socket: &agent,
                logs: &logs,
                write_roots: &[],
            },
        )
    }

    fn policy(f: &Fixture) -> Policy {
        policy_with(f, &Env::new(), &Env::new()).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn allowed(p: &Policy, path: &Path, access: Access) -> bool {
        p.decide(path, access).allowed
    }

    /// With no configuration: read everything but credentials, write the workspace and temp, nothing else.
    #[test]
    fn the_built_in_default() {
        let f = fixture("", "");
        let p = policy(&f);
        let h = &f.home;
        assert!(allowed(&p, Path::new("/etc/hosts"), Access::Read));
        assert!(allowed(&p, &h.join(".gitconfig"), Access::Read));
        assert!(allowed(&p, &f.repo.join("src/main.rs"), Access::Write));
        assert!(allowed(&p, &f.repo.join(".piton/.local/gigs/x"), Access::Write));
        assert!(!allowed(&p, &f.home.join("elsewhere"), Access::Write));
        for denied in [
            ".ssh/id_ed25519",
            ".aws/credentials",
            "Library/Keychains/login.keychain-db",
            ".zsh_history",
        ] {
            assert!(!allowed(&p, &h.join(denied), Access::Read), "{denied}");
        }
        // No user caches: host tools reuse them.
        for closed in [".cache/x", "Library/Caches/go-build/x", ".cargo/registry/x", ".zshrc"] {
            assert!(!allowed(&p, &h.join(closed), Access::Write), "{closed}");
        }
        // pi's runtime state, not its code.
        let agent = h.join(".pi/agent");
        assert!(allowed(&p, &agent.join("sessions/--w--/s.jsonl"), Access::Write));
        assert!(allowed(&p, &agent.join("auth.json"), Access::Write));
        assert!(allowed(&p, &agent.join("auth.json.lock/x"), Access::Write));
        for state in [
            "models-store.json",
            "models-store.json.lock",
            "mcp-auth.json",
            "mcp-auth.json.lock",
            "mcp-auth-refresh-0123abcd.lock",
            "mcp-auth-refresh-0123abcd.lock/x",
            "crashes.json",
            "mcp.log",
            "mcp.log.1",
            // pi locks `settings.json` and `trust.json` to read them; only the locks are writable.
            "settings.json.lock",
            "settings.json.lock/x",
            "trust.json.lock",
            "trust.json.lock/x",
        ] {
            assert!(allowed(&p, &agent.join(state), Access::Write), "{state}");
        }
        for code in [
            "settings.json",
            // Which folders' `.pi/` extensions and settings host sessions load.
            "trust.json",
            "mcp.json",
            "keybindings.json",
            "models.json",
            "bin/rg",
            "tmp/extensions/x",
            "git/x",
            // The name prefix covers only its own names.
            "mcp-auth-refreshx",
            "extensions/x.ts",
            "extensions/sessions/index.ts",
            "npm/x",
            "skills/s/SKILL.md",
        ] {
            assert!(!allowed(&p, &agent.join(code), Access::Write), "{code}");
        }
        // Configuration and protected files.
        assert!(!allowed(&p, &h.join(".config/piton/config.toml"), Access::Write));
        assert!(!allowed(&p, &f.repo.join(".piton/config.toml"), Access::Write));
        assert!(!allowed(&p, &f.repo.join("sub/.pi/settings.json"), Access::Write));
        assert!(allowed(&p, &f.repo.join(".agents/rules/r.md"), Access::Write));
        // The daemon.
        assert!(!allowed(&p, &f.state.join("piton.sock"), Access::Connect));
        assert!(!allowed(&p, &f.state.join("piton.db"), Access::Read));
        assert!(allowed(&p, &f.state.join("agent.sock"), Access::Connect));
        assert!(allowed(&p, &f.state.join("logs/daemon/current.log"), Access::Read));
        assert!(!allowed(&p, &f.state.join("logs/daemon/current.log"), Access::Write));
        // Every other socket and host: the network is open.
        assert!(allowed(&p, Path::new("/private/tmp/ssh-agent.sock"), Access::Connect));
        assert!(!p.keychain);
    }

    #[test]
    fn presets_allow_and_deny() {
        let f = fixture(
            "",
            "[sandbox]\npresets = [\"rust\"]\nallow = [\"~/out\"]\nallow_file = [\"~/f\"]\ndeny = [\"~/p/repo/secret\"]\n",
        );
        assert_eq!(launch_config(&f).sandbox.presets, [Preset::Rust]);
        let p = policy(&f);
        let h = &f.home;
        assert!(allowed(&p, &h.join(".cargo/registry/index/x"), Access::Write));
        assert!(allowed(&p, &h.join(".cargo/.package-cache"), Access::Write));
        assert!(!allowed(&p, &h.join(".cargo/bin/cargo"), Access::Write));
        assert!(allowed(&p, &h.join("out/x"), Access::Write));
        assert!(allowed(&p, &h.join("f"), Access::Write));
        assert!(!allowed(&p, &h.join("f2"), Access::Write));
        // A user deny beats the workspace grant.
        assert!(!allowed(&p, &f.repo.join("secret/k"), Access::Read));
        assert!(!allowed(&p, &f.repo.join("secret/k"), Access::Write));
        assert_eq!(
            p.decide(&f.repo.join("secret/k"), Access::Read).source,
            Source::Config {
                key: "sandbox.deny".into()
            }
        );
    }

    /// `allow` alone doesn't reopen a credential; `expose` (global only) does, and makes it writable with `allow`.
    #[test]
    fn expose_lifts_a_built_in_deny() {
        let f = fixture(
            "[sandbox]\nexpose = [\"~/.ssh/config\", \"~/Library/Keychains\"]\nallow = [\"~/Library/Keychains\", \"~/.aws\"]\n",
            "",
        );
        let p = policy(&f);
        let h = &f.home;
        assert!(allowed(&p, &h.join(".ssh/config"), Access::Read));
        assert!(!allowed(&p, &h.join(".ssh/config"), Access::Write));
        assert!(!allowed(&p, &h.join(".ssh/id_ed25519"), Access::Read));
        assert!(allowed(
            &p,
            &h.join("Library/Keychains/login.keychain-db"),
            Access::Write
        ));
        assert!(
            !allowed(&p, &h.join(".aws/credentials"), Access::Read),
            "allow doesn't lift a deny"
        );
        assert!(!allowed(&p, &h.join(".aws/credentials"), Access::Write));
        assert!(p.keychain);
    }

    #[test]
    fn grants_expand_from_the_daemons_environment() {
        let f = fixture("[sandbox]\nallow = [\"$CACHE/x\"]\n", "");
        let p = policy_with(
            &f,
            &env(&[("CACHE", "/private/tmp/c")]),
            &env(&[("CACHE", "/elsewhere")]),
        )
        .unwrap();
        assert!(
            p.rules
                .iter()
                .any(|r| r.target == subpath(Path::new("/private/tmp/c/x")))
        );
        assert!(matches!(
            policy_with(&f, &Env::new(), &Env::new()),
            Err(SandboxError::UnsetVariable { .. })
        ));
        let home = Path::new("/Users/a");
        assert_eq!(
            expand_grant("${HOME}x", home, &env(&[("HOME", "/h")])).unwrap(),
            Path::new("/hx")
        );
        assert!(matches!(
            expand_grant("$REL", home, &env(&[("REL", "rel")])),
            Err(SandboxError::NotAbsolute { .. })
        ));
    }

    /// The workspace sets `PI_CODING_AGENT_DIR` (its `mise.toml`): a per-folder one works, one that would open what
    /// the sandbox closes refuses the launch.
    #[test]
    fn the_agent_directory_comes_from_the_launch_but_cant_open_anything() {
        let f = fixture("", "");
        let h = f.home.to_string_lossy().into_owned();
        let launch = |dir: &str| policy_with(&f, &Env::new(), &env(&[(AGENT_DIR_ENV, dir)]));

        let p = launch("~/p/agents/one").unwrap();
        assert!(allowed(
            &p,
            &f.home.join("p/agents/one/sessions/s.jsonl"),
            Access::Write
        ));
        assert!(!allowed(
            &p,
            &f.home.join("p/agents/one/extensions/x.ts"),
            Access::Write
        ));
        // Inside the workspace: its code is closed even there.
        let p = launch("~/p/repo/agent").unwrap();
        assert!(!allowed(&p, &f.repo.join("agent/extensions/x.ts"), Access::Write));
        assert!(allowed(&p, &f.repo.join("agent/auth.json"), Access::Write));
        // The host's agent directory stays closed, runtime state included.
        assert!(!allowed(&p, &f.home.join(".pi/agent/extensions/x.ts"), Access::Write));
        assert!(!allowed(&p, &f.home.join(".pi/agent/sessions/s.jsonl"), Access::Write));

        for refused in [
            h.clone(),
            "/".into(),
            "~/.pi/agent/extensions".into(),
            "~/p/repo/.pi".into(),
            "~/p/repo/sub/.PI/agent".into(),
            "~/.config/piton/agent".into(),
            "~/.config".into(),
            "~/.local/state/piton/x".into(),
            "~/.ssh/agent".into(),
            "~/p/repo/.piton".into(),
        ] {
            assert!(
                matches!(launch(&refused), Err(SandboxError::AgentDir { .. })),
                "{refused}"
            );
        }
        // The host's own, from the daemon's environment.
        let base = env(&[(AGENT_DIR_ENV, &format!("{h}/.config/pi/agent"))]);
        let p = policy_with(&f, &base, &base).unwrap();
        assert!(allowed(&p, &f.home.join(".config/pi/agent/auth.json"), Access::Write));
        assert!(!allowed(
            &p,
            &f.home.join(".config/pi/agent/settings.json"),
            Access::Write
        ));
    }

    #[test]
    fn configuration_links_are_closed() {
        let f = fixture("", "");
        let dotfiles = f.home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("config.toml"), "").unwrap();
        let global = f.home.join(".config/piton/config.toml");
        std::fs::remove_file(&global).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("config.toml"), &global).unwrap();
        let p = policy_with(&f, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&p, &dotfiles.join("config.toml"), Access::Write));
    }

    /// Every rule is valid Seatbelt the library accepts.
    #[test]
    fn the_rules_are_accepted_by_the_library() {
        let f = fixture(
            "[sandbox]\nexpose = [\"~/Library/Keychains\"]\npresets = [\"mise\", \"rust\", \"node\", \"swift\"]\n",
            "",
        );
        let caps = super::super::apply::capabilities(&policy(&f)).unwrap();
        assert!(caps.platform_rules().iter().any(|r| r.contains("piton.sock")));
    }
}
