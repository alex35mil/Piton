//! One launch's [`Policy`]: Piton's built-in policy, the merged `[sandbox]` chain, and the launch's facts (workspace,
//! agent directory, the daemon's state folder), as an ordered list of literal rules.
//!
//! Order, the last matching rule deciding:
//! 1. writes: the workspace, the source's `.piton/.local`, temp, presets, `allow`, `allow_file`;
//! 2. nono's credential denies; then `expose`, which lifts them (and reopens writes granted inside an exposed path);
//! 3. the host agent directory is read-only; the launch's agent directory's runtime state is writable;
//! 4. the user's `deny`;
//! 5. every configuration level, and what its entries link to, is read-only; protected files are read-only; so are
//!    Git's hooks and configuration in the launch's checkouts;
//! 6. the daemon's state folder is closed (reads, writes, connects) except its logs and the agent socket, and its
//!    control socket can't be connected to, wherever it is.
//!
//! Seatbelt compares real paths, so each rule also gets its real-path form. A deny's follows every symlink: it closes
//! what a link points to as well. An allow's follows only links no sandboxed session of the project can replace
//! (`/var` is `/private/var`): one it could replace would let it move the grant wherever it likes.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

use super::{
    SandboxError,
    policy::{self, AgentDirWrite, Source},
    rules::{Access, Effect, Policy, Rule, Target},
};
use crate::{
    agent::{GitControl, canonical_target, lexical},
    config::{LOCAL_DIR, LaunchConfig, Preset, expand_home},
    launch::{
        Env,
        env::{Origin, SandboxEnv, ToolSource, Widening},
    },
};

/// What a launch is, beyond its configuration.
pub struct Facts<'a> {
    pub home: &'a Path,
    /// The daemon's base environment: `$VAR` grants and the host agent directory come from it, never from the
    /// workspace's (a repository's `mise.toml` can set variables).
    pub base_env: &'a Env,
    /// pi's environment for this launch, with where each value came from: the agent directory pi will use.
    pub launch_env: &'a SandboxEnv,
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

/// The most symlinks one path's resolution follows, as the kernel's limit; past it, the rest is kept as written.
const MAX_LINKS: usize = 40;

/// A launch's policy, and the grants it doesn't resolve as their paths suggest.
#[derive(Debug)]
pub struct Resolution {
    pub policy: Policy,
    pub links: Vec<UnfollowedLink>,
}

/// A grant whose path goes through a symlink a sandboxed session could have made, so the grant doesn't follow it: it
/// covers the path as written, which matches nothing while the link is there. A user who links a granted folder
/// elsewhere grants the real path instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfollowedLink {
    pub link: PathBuf,
    pub grant: PathBuf,
    pub source: Source,
}

impl std::fmt::Display for UnfollowedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match self.grant == self.link {
            true => "is a symlink".to_string(),
            false => format!("goes through the symlink {}", self.link.display()),
        };
        write!(
            f,
            "the sandbox grant {} {what}, which a sandboxed session could have made, so it isn't followed: grant the \
             real path instead",
            self.grant.display(),
        )
    }
}

/// The launch's policy, or why the launch is refused.
pub fn resolve(config: &LaunchConfig, facts: &Facts) -> Result<Resolution, SandboxError> {
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

    // 3. pi's agent directories: as named, so the runtime grants below resolve like every other grant.
    let host_agent = agent_dir(
        facts
            .base_env
            .get(Widening::AgentDir.var())
            .filter(|v| !v.is_empty())
            .map(String::as_str),
        facts.home,
        facts.workspace,
    );
    let launch_agent = facts.launch_env.widening(Widening::AgentDir);
    let agent = agent_dir(launch_agent.map(|(value, _)| value), facts.home, facts.workspace);
    check_agent_dir(
        &canonical_target(&agent),
        &canonical_target(&host_agent),
        config,
        facts,
        &denied,
    )?;
    rules.push(
        Effect::Deny,
        &[Access::Write],
        subpath(&host_agent),
        Source::Piton {
            what: "pi's agent directory: extensions, settings, and packages host sessions load".into(),
        },
    );
    if canonical_target(&agent) != canonical_target(&host_agent) {
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
    rules.push(
        Effect::Deny,
        &[Access::Write],
        Target::Protected {
            source: canonical(&config.source),
            checkout: canonical(&config.checkout),
        },
        Source::Piton {
            what: "a protected file (.piton/ or .pi/): changed only with the user's approval".into(),
        },
    );
    // The `git` preset (the user's own choice) leaves this rule out.
    if !table.presets.contains(&Preset::Git) {
        rules.push(
            Effect::Deny,
            &[Access::Write],
            Target::GitControl {
                control: GitControl::of(&[&canonical(&config.checkout), &canonical(&config.source)]),
            },
            Source::Piton {
                what: "Git's hooks and configuration: Git on the host runs what they name".into(),
            },
        );
    }

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
    let (rules, links) = rules.finish(&[&config.checkout, &config.source]);
    let policy = Policy { rules, keychain };
    if let Some((value, origin)) = launch_agent {
        accept_widening(Widening::AgentDir, value, &agent, origin, &policy, config)?;
    }
    Ok(Resolution { policy, links })
}

/// A value that widens the sandbox is accepted from the daemon's environment, or from a file the agent can't write.
/// One the agent could have written (or a tool that names no file) is accepted only while it leads into the
/// checkout. `path` is the value as the policy uses it.
fn accept_widening(
    widening: Widening,
    value: &str,
    path: &Path,
    origin: &Origin,
    policy: &Policy,
    config: &LaunchConfig,
) -> Result<(), SandboxError> {
    let set_by = match origin {
        Origin::Daemon => return Ok(()),
        Origin::Tool { tool, source } => match source {
            ToolSource::File(file) => match agent_writable(file, policy, config) {
                true => file.display().to_string(),
                false => return Ok(()),
            },
            ToolSource::Unknown => format!("{tool}, which named no file"),
        },
    };
    let checkout = canonical_target(&config.checkout);
    match canonical_target(path).starts_with(&checkout) {
        true => Ok(()),
        false => Err(SandboxError::Widening {
            var: widening.var(),
            value: value.to_string(),
            set_by,
            checkout,
        }),
    }
}

/// Whether a sandboxed session of this project could have written `file`: this launch's policy lets it, as written or
/// at its real path, or it's in the checkout or source checkout, where a session in the whole checkout writes even
/// when this launch's workspace is a subfolder.
fn agent_writable(file: &Path, policy: &Policy, config: &LaunchConfig) -> bool {
    let written = lexical(file);
    let real = canonical_target(file);
    let roots = [canonical_target(&config.checkout), canonical_target(&config.source)];
    [&written, &real].into_iter().any(|path| {
        policy.decide(path, Access::Write).allowed
            || roots.iter().any(|root| path.starts_with(root))
            || [&config.checkout, &config.source]
                .iter()
                .any(|root| path.starts_with(root))
    })
}

/// The rules as written, in order; [`Rules::finish`] adds their real-path forms.
#[derive(Default)]
struct Rules {
    rules: Vec<Rule>,
}

impl Rules {
    fn push(&mut self, effect: Effect, access: &[Access], target: Target, source: Source) {
        self.rules.push(Rule {
            effect,
            access: access.to_vec(),
            target,
            source,
        });
    }

    /// Every rule, followed by the same rule for its real path when that differs: Seatbelt compares real paths (`/var`
    /// is `/private/var`), a connect may compare the path as given.
    ///
    /// What a sandboxed session of this project can replace: the `checkouts` (a session whose workspace is the whole
    /// checkout writes there, even when this launch's is a subfolder) and what this launch's write grants cover, as
    /// written or resolved. A resolved grant can make more replaceable (`~/l/b` with `~/l -> ~/x` covers `~/x/b`), which
    /// can stop another grant's resolution, so it's computed to a fixed point. The set only grows, which only follows
    /// fewer links, and it's bounded (each grant resolves to one of finitely many stopping points), so it ends.
    fn finish(self, checkouts: &[&Path]) -> (Vec<Rule>, Vec<UnfollowedLink>) {
        let mut replaceable: Vec<Target> = checkouts
            .iter()
            .map(|c| subpath(&canonical_target(c)))
            .chain(self.rules.iter().filter(|r| grants_write(r)).map(|r| r.target.clone()))
            .collect();
        loop {
            let resolved: Vec<RealForm> = self.rules.iter().map(|r| real_form(r, &replaceable)).collect();
            let mut grew = false;
            for (rule, real) in self.rules.iter().zip(&resolved) {
                if grants_write(rule)
                    && let Some(target) = &real.target
                    && !replaceable.contains(target)
                {
                    replaceable.push(target.clone());
                    grew = true;
                }
            }
            if grew {
                continue;
            }
            let mut rules = Vec::with_capacity(self.rules.len() * 2);
            let mut links: Vec<UnfollowedLink> = Vec::new();
            for (rule, real) in self.rules.into_iter().zip(resolved) {
                if let (Some(link), Some(grant)) = (real.unfollowed, rule_path(&rule.target)) {
                    let report = UnfollowedLink {
                        link,
                        grant: grant.to_path_buf(),
                        source: rule.source.clone(),
                    };
                    if !links.contains(&report) {
                        links.push(report);
                    }
                }
                let companion = real
                    .target
                    .filter(|t| *t != rule.target)
                    .map(|target| Rule { target, ..rule.clone() });
                rules.push(rule);
                rules.extend(companion);
            }
            return (rules, links);
        }
    }
}

fn grants_write(rule: &Rule) -> bool {
    match rule.effect {
        Effect::Allow => rule.access.contains(&Access::Write),
        Effect::Deny => false,
    }
}

fn rule_path(target: &Target) -> Option<&Path> {
    match target {
        Target::Subpath { path } | Target::Literal { path } | Target::Prefix { path } => Some(path),
        Target::Protected { .. } | Target::GitControl { .. } => None,
    }
}

/// A rule's real-path target, and the link its resolution stopped at because a sandboxed session could have made it.
struct RealForm {
    target: Option<Target>,
    unfollowed: Option<PathBuf>,
}

/// A deny follows every link: it closes what a link points to too. An allow follows only links outside `replaceable`,
/// and a write grant never its own last component, which it can always replace.
fn real_form(rule: &Rule, replaceable: &[Target]) -> RealForm {
    let resolve = |path: &Path, keep_last: Last| match rule.effect {
        Effect::Deny => Walk {
            path: canonical_target(path),
            unfollowed: None,
        },
        Effect::Allow => walk(path, keep_last, replaceable),
    };
    let last = match rule.access.contains(&Access::Write) {
        true => Last::Kept,
        false => Last::Followed,
    };
    let (target, walked) = match &rule.target {
        Target::Subpath { path } => {
            let walked = resolve(path, last);
            (subpath(&walked.path), walked.unfollowed)
        }
        Target::Literal { path } => {
            let walked = resolve(path, last);
            (literal(&walked.path), walked.unfollowed)
        }
        // A name prefix isn't an entry: its folder is resolved, the prefix kept.
        Target::Prefix { path } => match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let walked = resolve(parent, Last::Followed);
                (
                    Target::Prefix {
                        path: walked.path.join(name),
                    },
                    walked.unfollowed,
                )
            }
            (None, _) | (_, None) => (rule.target.clone(), None),
        },
        Target::Protected { .. } | Target::GitControl { .. } => {
            return RealForm {
                target: None,
                unfollowed: None,
            };
        }
    };
    RealForm {
        target: Some(target),
        unfollowed: walked,
    }
}

/// Whether a walk follows a link at the path's last component.
#[derive(Debug, Clone, Copy)]
enum Last {
    /// A write grant's own entry: always replaceable by the sandbox, never followed.
    Kept,
    /// Followed like any other component.
    Followed,
}

struct Walk {
    path: PathBuf,
    unfollowed: Option<PathBuf>,
}

/// `path` resolved from `/`, following each symlink unless `replaceable` covers it (a sandboxed session could have made
/// it), or it's the last component and `last` keeps it. Below a link that isn't followed, the agent decides what each
/// name resolves to, so the rest is kept as written. Past [`MAX_LINKS`] links, the rest is kept too.
fn walk(path: &Path, last: Last, replaceable: &[Target]) -> Walk {
    let mut pending: std::collections::VecDeque<OsString> = names(&lexical(path)).collect();
    // A relative path (an agent directory named relative) resolves against the working folder, as before.
    let mut current = match path.is_absolute() {
        true => PathBuf::from("/"),
        false => PathBuf::new(),
    };
    let mut followed = 0;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            current.pop();
            continue;
        }
        let candidate = current.join(&name);
        let is_link = candidate.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            current = candidate;
            continue;
        }
        let kept = match (last, pending.is_empty()) {
            (Last::Kept, true) => true,
            (Last::Kept, false) | (Last::Followed, _) => replaceable.iter().any(|t| t.covers(&candidate)),
        };
        let target = match kept || followed >= MAX_LINKS {
            true => None,
            false => std::fs::read_link(&candidate).ok(),
        };
        let Some(target) = target else {
            let mut rest = candidate.clone();
            rest.extend(pending);
            return Walk {
                path: rest,
                unfollowed: kept.then_some(candidate),
            };
        };
        followed += 1;
        if target.is_absolute() {
            current = PathBuf::from("/");
        }
        for name in names(&target).collect::<Vec<_>>().into_iter().rev() {
            pending.push_front(name);
        }
    }
    Walk {
        path: current,
        unfollowed: None,
    }
}

/// A path's names, with `..` kept as `..` (a link's target may climb).
fn names(path: &Path) -> impl Iterator<Item = OsString> + '_ {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name.to_os_string()),
        Component::ParentDir => Some(OsString::from("..")),
        Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
    })
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

/// pi's agent directory from `PI_CODING_AGENT_DIR` (with `~`; a relative one from the workspace, pi's working
/// folder), else `~/.pi/agent`. As named, with no symlink followed: its rules get their real paths like every other
/// rule.
fn agent_dir(value: Option<&str>, home: &Path, workspace: &Path) -> PathBuf {
    let dir = value
        .map(|v| workspace.join(expand_home(v, home)))
        .unwrap_or_else(|| home.join(".pi/agent"));
    lexical(&dir)
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
        resolve_in(f, &f.repo, base, launch).map(|r| r.policy)
    }

    /// `launch` as values of the daemon's environment: the tests of what an agent directory may be.
    fn resolve_in(f: &Fixture, workspace: &Path, base: &Env, launch: &Env) -> Result<Resolution, SandboxError> {
        resolve_env(f, workspace, base, &SandboxEnv::daemon(launch))
    }

    fn resolve_env(f: &Fixture, workspace: &Path, base: &Env, launch: &SandboxEnv) -> Result<Resolution, SandboxError> {
        let control = f.state.join("piton.sock");
        let agent = f.state.join("agent.sock");
        let logs = f.state.join("logs");
        resolve(
            &launch_config(f),
            &Facts {
                home: &f.home,
                base_env: base,
                launch_env: launch,
                workspace,
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
        let launch = |dir: &str| policy_with(&f, &Env::new(), &env(&[(Widening::AgentDir.var(), dir)]));

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
        let base = env(&[(Widening::AgentDir.var(), &format!("{h}/.config/pi/agent"))]);
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

    fn link(target: &Path, at: &Path) {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
    }

    fn reported(r: &Resolution) -> Vec<PathBuf> {
        r.links.iter().map(|l| l.link.clone()).collect()
    }

    /// A runtime entry of pi's agent directory the agent replaced with a link doesn't move the grant to the link's
    /// target: the next launch can't write the shell config through `crashes.json`.
    #[test]
    fn a_runtime_entry_replaced_with_a_link_grants_nothing() {
        let f = fixture("", "");
        let agent = f.home.join(".pi/agent");
        std::fs::write(f.home.join(".zshrc"), "").unwrap();
        std::fs::write(f.home.join("victim"), "").unwrap();
        link(&f.home.join(".zshrc"), &agent.join("crashes.json"));
        link(&f.home.join("victim"), &agent.join("mcp.log"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&r.policy, &f.home.join(".zshrc"), Access::Write));
        assert!(!allowed(&r.policy, &f.home.join("victim"), Access::Write));
        assert!(allowed(&r.policy, &agent.join("auth.json"), Access::Write));
        let links = reported(&r);
        assert!(links.contains(&agent.join("crashes.json")), "{links:?}");
        assert!(links.contains(&agent.join("mcp.log")), "{links:?}");
    }

    #[test]
    fn an_allow_replaced_with_a_link_grants_nothing() {
        let f = fixture("[sandbox]\nallow = [\"~/d/cache\"]\n", "");
        mkdir(&f.home.join("victim"));
        link(&f.home.join("victim"), &f.home.join("d/cache"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(!allowed(&r.policy, &f.home.join("victim/x"), Access::Write));
        assert_eq!(reported(&r), [f.home.join("d/cache")]);
        assert_eq!(
            r.links[0].source,
            Source::Config {
                key: "sandbox.allow".into()
            }
        );
    }

    /// A link the sandbox can't replace is followed: the user's own (a folder moved to another disk), like `/var`.
    #[test]
    fn a_link_above_every_grant_is_followed() {
        let f = fixture("[sandbox]\nallow = [\"~/ln/cache\"]\n", "");
        mkdir(&f.home.join("real/cache"));
        link(&f.home.join("real"), &f.home.join("ln"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(allowed(&r.policy, &f.home.join("real/cache/x"), Access::Write));
        assert!(!allowed(&r.policy, &f.home.join("real/other"), Access::Write));
        assert!(r.links.is_empty(), "{:?}", r.links);
    }

    /// An agent directory inside the workspace, below a folder the agent swapped for a link out of it.
    #[test]
    fn an_agent_directory_below_a_planted_link_stays_in_the_checkout() {
        let f = fixture("", "");
        let outside = f.home.join("outside");
        mkdir(&outside.join("agent"));
        link(&outside, &f.repo.join("tools"));
        let launch = env(&[(Widening::AgentDir.var(), "~/p/repo/tools/agent")]);
        let r = resolve_in(&f, &f.repo, &Env::new(), &launch).unwrap();
        assert!(!allowed(&r.policy, &outside.join("agent/crashes.json"), Access::Write));
        assert!(!allowed(
            &r.policy,
            &outside.join("agent/sessions/s.jsonl"),
            Access::Write
        ));
        assert!(!allowed(
            &r.policy,
            &outside.join("agent/mcp-auth-refresh-x.lock"),
            Access::Write
        ));
        assert!(reported(&r).contains(&f.repo.join("tools")));
    }

    /// A session in the whole checkout could have planted the link; a later one in a subfolder must not follow it.
    #[test]
    fn links_anywhere_in_the_checkout_are_never_followed() {
        let f = fixture("", "");
        let launch_agents = f.home.join("Library/LaunchAgents");
        mkdir(&launch_agents);
        mkdir(&f.repo.join("packages/web"));
        link(&launch_agents, &f.repo.join(".pi-agent"));
        let launch = env(&[(Widening::AgentDir.var(), "~/p/repo/.pi-agent")]);
        let r = resolve_in(&f, &f.repo.join("packages/web"), &Env::new(), &launch).unwrap();
        for entry in ["mcp-auth-refresh-x.plist", "crashes.json", "sessions/x.plist"] {
            assert!(
                !allowed(&r.policy, &launch_agents.join(entry), Access::Write),
                "{entry}"
            );
        }
    }

    /// A resolved grant can make an entry replaceable that another grant's path goes through.
    #[test]
    fn what_resolved_grants_cover_is_replaceable_too() {
        let f = fixture("[sandbox]\nallow = [\"~/l/b\", \"~/x/b/c/d\"]\n", "");
        mkdir(&f.home.join("x/b"));
        mkdir(&f.home.join("victim/d"));
        link(&f.home.join("x"), &f.home.join("l"));
        link(&f.home.join("victim"), &f.home.join("x/b/c"));
        let r = resolve_in(&f, &f.repo, &Env::new(), &Env::new()).unwrap();
        assert!(allowed(&r.policy, &f.home.join("x/b/y"), Access::Write));
        assert!(!allowed(&r.policy, &f.home.join("victim/d/z"), Access::Write));
        assert_eq!(reported(&r), [f.home.join("x/b/c")]);
    }

    /// Denies follow every link: a deny named by a link closes its target.
    #[test]
    fn a_deny_closes_what_its_link_points_to() {
        let f = fixture("[sandbox]\ndeny = [\"~/p/repo/secret\"]\n", "");
        let secret = f.home.join("secret");
        mkdir(&secret);
        link(&secret, &f.repo.join("secret"));
        let p = policy(&f);
        assert!(!allowed(&p, &secret.join("k"), Access::Read));
    }

    /// Git's hooks and configuration are closed in the checkout, the rest of `.git` and a new worktree's `.git` file
    /// aren't, and `piton sandbox why` names the rule.
    #[test]
    fn gits_control_files_are_closed() {
        let f = fixture("", "");
        let p = policy(&f);
        for denied in [
            ".git",
            ".git/hooks/post-merge",
            ".git/config",
            ".GIT/Config",
            "vendor/lib/.git/hooks/x",
        ] {
            assert!(!allowed(&p, &f.repo.join(denied), Access::Write), "{denied}");
        }
        assert_eq!(
            p.decide(&f.repo.join(".git/config"), Access::Write).source,
            Source::Piton {
                what: "Git's hooks and configuration: Git on the host runs what they name".into()
            }
        );
        for open in [
            ".git/index",
            ".git/refs/heads/x",
            ".git/worktrees/w/commondir",
            ".piton/.local/pull-requests/1/rebase/.git",
        ] {
            assert!(allowed(&p, &f.repo.join(open), Access::Write), "{open}");
        }
        assert!(allowed(&p, &f.repo.join(".git/config"), Access::Read));

        // The `git` preset in the global configuration lifts them.
        let g = fixture("[sandbox]\npresets = [\"git\"]\n", "");
        let p = policy(&g);
        for open in [".git/config", ".git/hooks/post-merge", ".git/commondir"] {
            assert!(allowed(&p, &g.repo.join(open), Access::Write), "{open}");
        }
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

    fn from_file(file: &Path) -> Origin {
        Origin::Tool {
            tool: crate::launch::env::EnvTool::Mise,
            source: ToolSource::File(file.to_path_buf()),
        }
    }

    fn agent_from(value: &str, origin: Origin) -> SandboxEnv {
        SandboxEnv::for_tests(&[(Widening::AgentDir.var(), value, origin)])
    }

    /// A workspace's `mise.toml` the agent can write may set the agent directory only inside the checkout; a file it
    /// can't write (a client folder's, above the checkout) may set it anywhere the existing checks allow.
    #[test]
    fn the_agent_directory_follows_the_file_that_set_it() {
        let f = fixture("", "");
        let own = f.repo.join("mise.toml");
        let sub = f.repo.join("packages/web");
        mkdir(&sub);
        let refused = |workspace: &Path, env: &SandboxEnv| {
            matches!(
                resolve_env(&f, workspace, &Env::new(), env),
                Err(SandboxError::Widening { .. })
            )
        };

        // The workspace's own `mise.toml`: inside the checkout, also from a subfolder session.
        let inside = agent_from("~/p/repo/.pi-agent", from_file(&own));
        let r = resolve_env(&f, &sub, &Env::new(), &inside).unwrap();
        assert!(allowed(
            &r.policy,
            &f.repo.join(".pi-agent/crashes.json"),
            Access::Write
        ));
        assert!(refused(&f.repo, &agent_from("~/Library/LaunchAgents", from_file(&own))));
        assert!(refused(&sub, &agent_from("~/Library/LaunchAgents", from_file(&own))));
        // A subfolder's file, outside this launch's workspace but in the checkout.
        assert!(refused(
            &sub,
            &agent_from("~/elsewhere", from_file(&f.repo.join("other/mise.toml")))
        ));
        // No file named: written by the agent.
        let unknown = Origin::Tool {
            tool: crate::launch::env::EnvTool::Mise,
            source: ToolSource::Unknown,
        };
        assert!(refused(&f.repo, &agent_from("~/elsewhere", unknown)));

        // A client folder's file above the checkout: the agent can't write it.
        let client = f.home.join("p/mise.toml");
        let shared = agent_from("~/p/shared-agent", from_file(&client));
        let r = resolve_env(&f, &f.repo, &Env::new(), &shared).unwrap();
        assert!(allowed(
            &r.policy,
            &f.home.join("p/shared-agent/crashes.json"),
            Access::Write
        ));
        // The daemon's environment: trusted.
        let daemon = SandboxEnv::daemon(&env(&[(Widening::AgentDir.var(), "~/elsewhere")]));
        assert!(resolve_env(&f, &f.repo, &Env::new(), &daemon).is_ok());

        // A file the launch's grants cover is writable wherever it is.
        let g = fixture("[sandbox]\nallow = [\"~/shared\"]\n", "");
        let granted = agent_from("~/elsewhere", from_file(&g.home.join("shared/mise.toml")));
        assert!(matches!(
            resolve_env(&g, &g.repo, &Env::new(), &granted),
            Err(SandboxError::Widening { .. })
        ));
    }

    /// mise names a file as written: a link in the checkout to a file the agent can't write is still the agent's.
    #[test]
    fn a_file_reached_through_a_checkout_link_is_the_agents() {
        let f = fixture("", "");
        let outside = f.home.join("dotfiles/mise.toml");
        mkdir(outside.parent().unwrap());
        std::fs::write(&outside, "").unwrap();
        link(&outside, &f.repo.join("mise.toml"));
        let env = agent_from("~/elsewhere", from_file(&f.repo.join("mise.toml")));
        assert!(matches!(
            resolve_env(&f, &f.repo, &Env::new(), &env),
            Err(SandboxError::Widening { .. })
        ));
    }

    /// pi resolves a relative agent directory from its working folder: the workspace.
    #[test]
    fn a_relative_agent_directory_is_the_workspaces() {
        let f = fixture("", "");
        let sub = f.repo.join("web");
        mkdir(&sub);
        let env = agent_from("./agent", from_file(&sub.join("mise.toml")));
        let r = resolve_env(&f, &sub, &Env::new(), &env).unwrap();
        assert!(allowed(&r.policy, &sub.join("agent/auth.json"), Access::Write));
    }
}
