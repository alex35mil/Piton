//! A session's commands that run through the daemon: which bash calls pi is running (from its own events), what a
//! request to run one through the daemon gets, and the block question while the user decides.
//!
//! A request over `agent.sock` is only a request. The daemon accepts one for a bash call pi reported running
//! (`tool_execution_start`) with exactly that command, once per call; a rerun only when that call's arguments asked for
//! exactly it (`rerun`: a runtime and grant, read as a `[[sandbox.commands]]` entry's). It decides where it runs (a
//! per-command entry it matched itself, or, once the user approves, the requested grant, `all`, or the host), the shell
//! and its prefix (pi's settings files), and the environment (the session's launch environment).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Map, Value};
use tokio::sync::oneshot;

use crate::{
    agent::{
        Approved, Matched, PiShell, RunDecision, RunRequest, Runtime, Why,
        block::PersistScope,
        grant_for,
        question::{self, Answer, Question, RpcChoice},
    },
    config::{
        AllGrant, CommandGrant, CommandPattern, CommandRuntime, ConfigFile, Grant, GrantLists, Paths, Sharing,
        WriteLevel,
    },
    launch::{
        Env,
        sandbox::grant::{CommandPolicies, EntryRuntime},
    },
};

/// A bash call pi is running.
#[derive(Debug, Clone)]
pub struct BashCall {
    pub command: String,
    /// The rerun the call's arguments asked for.
    pub rerun: CallRerun,
    /// A request was taken for it: one per call.
    pub used: bool,
    /// The kernel's refusals routed to this call (`operation path`).
    pub refusals: Vec<String>,
    /// The call's number in its sandbox's tag.
    pub number: u64,
}

/// The `rerun` argument of a bash call, as pi reported it (before pi validates it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallRerun {
    NotAsked,
    Asked(CommandRuntime),
    /// Present but not a runtime and grant: why.
    Invalid(String),
}

impl CallRerun {
    fn of(args: Option<&Value>) -> Self {
        match args.and_then(|a| a.get("rerun")) {
            None | Some(Value::Null) => CallRerun::NotAsked,
            Some(value) => match serde_json::from_value::<CommandRuntime>(value.clone()) {
                Ok(runtime) => CallRerun::Asked(runtime),
                Err(e) => CallRerun::Invalid(e.to_string()),
            },
        }
    }
}

/// A rerun the agent asked for, resolved before the user is asked: what the question's sandbox side runs with.
#[derive(Debug, Clone, PartialEq)]
pub enum Requested {
    Host,
    All,
    /// A narrowed grant, and the launch's policy resolved again with its lists.
    Narrowed {
        lists: Box<GrantLists>,
        policy: Box<crate::launch::sandbox::Policy>,
    },
}

impl Requested {
    /// The runtime as the agent wrote it.
    pub fn runtime(&self) -> CommandRuntime {
        match self {
            Requested::Host => CommandRuntime::Host,
            Requested::All => CommandRuntime::Sandbox(Grant::All(AllGrant::All)),
            Requested::Narrowed { lists, .. } => CommandRuntime::Sandbox(Grant::Lists(lists.clone())),
        }
    }
}

/// What the session needs to run commands: fixed at launch.
pub struct Commands {
    pub policies: CommandPolicies,
    pub env: Env,
    pub shell: PiShell,
    /// The source checkout: where \"this project\" persists, and what `cwd` must be inside of (with the checkout).
    pub source: PathBuf,
    pub checkout: PathBuf,
    pub paths: Paths,
    /// What a launch-time grant list lists, for matching.
    pub grants: Vec<crate::config::Listed<CommandGrant>>,
}

/// The question the user is being asked, and where the decision goes.
pub struct PendingQuestion {
    /// The typed question: its id is what `session.decide` names; its runtime is the one the `rpc` dialog shows.
    pub question: Question,
    /// The `rpc` rendering's dialog showing now, and which one it is.
    pub dialog_id: String,
    pub stage: Stage,
    /// The request, as the agent sent it.
    pub request: RunRequest,
    /// What it asked for, resolved.
    pub requested: Requested,
    /// The tag the call's sandbox gets.
    pub tag: String,
    pub reply: oneshot::Sender<RunDecision>,
    pub deadline: tokio::time::Instant,
}

/// Blocked processes that aren't bash calls: the question showing, the refusals already offered (once per process,
/// operation, and target), and what's waiting to be asked.
#[derive(Default)]
pub struct ProcessBlocks {
    pub asking: Option<ProcessQuestion>,
    offered: std::collections::HashSet<(String, String, String)>,
    waiting: std::collections::VecDeque<ProcessQuestion>,
}

/// One process question.
#[derive(Debug, Clone)]
pub struct ProcessQuestion {
    pub id: String,
    pub process: String,
    pub refusal: String,
    pub offer: crate::launch::sandbox::grant::Offer,
}

impl ProcessBlocks {
    pub fn asking(&self, id: &str) -> bool {
        self.asking.as_ref().is_some_and(|q| q.id == id)
    }

    /// Whether this refusal is new: each (process, operation, target) is offered once per session.
    pub fn first(&mut self, process: &str, operation: &str, target: &str) -> bool {
        self.offered
            .insert((process.to_string(), operation.to_string(), target.to_string()))
    }

    /// Every blocked process is asked about, in order: none is dropped.
    pub fn queue(&mut self, question: ProcessQuestion) {
        self.waiting.push_back(question);
    }

    /// The question showing is shown again, first: its dialog left unanswered (the run it showed in ended), or the user
    /// chose the option that decides nothing.
    pub fn requeue_unanswered(&mut self) {
        if let Some(question) = self.asking.take() {
            self.waiting.push_front(question);
        }
    }

    /// The next question to show, when none is showing.
    pub fn next(&mut self) -> Option<&ProcessQuestion> {
        if self.asking.is_none() {
            self.asking = self.waiting.pop_front();
        }
        self.asking.as_ref()
    }
}

impl Commands {
    /// Saves a process block's entry in the chosen file's `[sandbox]`.
    pub fn persist_lift(
        &self,
        scope: PersistScope,
        lift: &crate::launch::sandbox::grant::Lift,
    ) -> Result<PathBuf, String> {
        let (key, value) = lift.entry();
        self.append(scope, key, &serde_json::Value::String(value))
    }

    /// Saves `entry` in the chosen file's `[sandbox] silence`: blocks it matches are only logged from then on.
    pub fn persist_silence(&self, scope: PersistScope, entry: &crate::config::Silence) -> Result<PathBuf, String> {
        let value = serde_json::to_value(entry).map_err(|e| e.to_string())?;
        self.append(scope, "silence", &value)
    }

    fn append(&self, scope: PersistScope, key: &str, value: &serde_json::Value) -> Result<PathBuf, String> {
        let file = self.file(scope);
        let path = file.path(&self.paths).map_err(|e| e.to_string())?;
        file.append_to_list(&self.paths, &format!("sandbox.{key}"), value)
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// The file `path` would be checked as listed in when saved in `scope`: what an offer's entry is resolved with.
    pub fn scope_file(&self, scope: PersistScope) -> Option<PathBuf> {
        self.file(scope).path(&self.paths).ok()
    }

    fn file(&self, scope: PersistScope) -> ConfigFile {
        match scope {
            PersistScope::Global => ConfigFile {
                level: WriteLevel::Global,
                sharing: Sharing::Shared,
            },
            PersistScope::Project => ConfigFile {
                level: WriteLevel::Repository {
                    source: self.source.clone(),
                },
                sharing: Sharing::Local,
            },
        }
    }
}

/// Which `rpc` dialog is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Choice,
    Pattern(CommandRuntime, PersistScope),
}

/// The calls pi is running, from its events.
pub struct Calls {
    /// The session's tag: a call's sandbox is tagged `<session tag>-<number>`.
    tag_prefix: String,
    next: u64,
    calls: HashMap<String, BashCall>,
    /// Calls that ended, newest last, with their refusals: a rerun is a new call with the same command, and the kernel's
    /// record of the first run's refusals may arrive after it ended.
    ended: std::collections::VecDeque<(String, BashCall)>,
}

/// How many ended calls are kept for their refusals.
const ENDED_KEPT: usize = 16;
/// How many refusals one call keeps.
const REFUSALS_KEPT: usize = 64;

impl Calls {
    pub fn new(session_tag: &str) -> Self {
        Self {
            tag_prefix: session_tag.to_string(),
            next: 0,
            calls: HashMap::new(),
            ended: std::collections::VecDeque::new(),
        }
    }

    /// Follows `tool_execution_start` and `tool_execution_end` of bash calls.
    pub fn track(&mut self, kind: &str, record: &Map<String, Value>) {
        let id = || record.get("toolCallId").and_then(Value::as_str).map(str::to_string);
        match kind {
            "tool_execution_start" if record.get("toolName").and_then(Value::as_str) == Some("bash") => {
                let Some(id) = id() else { return };
                let args = record.get("args");
                let command = args.and_then(|a| a.get("command")).and_then(Value::as_str);
                let Some(command) = command else { return };
                let rerun = CallRerun::of(args);
                // A rerun shows what the last run of the same command was refused.
                let refusals = match rerun {
                    CallRerun::Asked(_) | CallRerun::Invalid(_) => self
                        .ended
                        .iter()
                        .rev()
                        .find(|(_, call)| call.command == command)
                        .map(|(_, call)| call.refusals.clone())
                        .unwrap_or_default(),
                    CallRerun::NotAsked => Vec::new(),
                };
                self.next += 1;
                let number = self.next;
                self.calls.insert(
                    id,
                    BashCall {
                        command: command.to_string(),
                        rerun,
                        used: false,
                        refusals,
                        number,
                    },
                );
            }
            "tool_execution_end" => {
                if let Some(id) = id()
                    && let Some(call) = self.calls.remove(&id)
                {
                    self.end(id, call);
                }
            }
            // A run that ended (aborted, crashed) leaves no call running.
            "agent_end" => {
                for (id, call) in std::mem::take(&mut self.calls) {
                    self.end(id, call);
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, id: String, call: BashCall) {
        self.ended.push_back((id, call));
        while self.ended.len() > ENDED_KEPT {
            self.ended.pop_front();
        }
    }

    #[cfg(test)]
    pub fn running(&self) -> impl Iterator<Item = (&String, &BashCall)> {
        self.calls.iter()
    }

    #[cfg(test)]
    pub fn get_running(&self, id: &str) -> Option<&BashCall> {
        self.calls.get(id)
    }

    /// Records a refusal for the call `id`, running or recently ended. False when it's neither.
    pub fn note(&mut self, id: &str, refusal: &str) -> bool {
        let call = match self.calls.get_mut(id) {
            Some(call) => call,
            None => match self.ended.iter_mut().rev().find(|(i, _)| i == id) {
                Some((_, call)) => call,
                None => return false,
            },
        };
        if call.refusals.len() < REFUSALS_KEPT && !call.refusals.iter().any(|r| r == refusal) {
            call.refusals.push(refusal.to_string());
        }
        true
    }

    /// Takes the call a request names: it must be running, with the same command, and not taken before. Returns it with
    /// the tag its sandbox carries.
    fn take(&mut self, request: &RunRequest) -> Result<(&mut BashCall, String), String> {
        let prefix = self.tag_prefix.clone();
        let call = self
            .calls
            .get_mut(&request.call_id)
            .ok_or_else(|| "no such bash call is running".to_string())?;
        if call.command != request.command {
            return Err("the command isn't the running call's".into());
        }
        if call.used {
            return Err("that call was already handled".into());
        }
        call.used = true;
        let tag = format!("{prefix}-{}", call.number);
        Ok((call, tag))
    }

    /// The tag of a command the user typed (`<session tag>-u`): a call's sandbox, so its refusals aren't taken for a
    /// blocked process; the user saw the command fail, and it's never offered to run again.
    fn typed_tag(&self) -> String {
        format!("{}-u", self.tag_prefix)
    }

    /// Whose sandbox a tag names: a call's (running or recently ended), a typed command's, or pi's own.
    pub fn owner(&self, tag: &str) -> TagOwner {
        if tag == self.tag_prefix {
            return TagOwner::Session;
        }
        if tag == self.typed_tag() {
            return TagOwner::Typed;
        }
        let Some(number) = tag
            .strip_prefix(&self.tag_prefix)
            .and_then(|t| t.strip_prefix('-'))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            return TagOwner::Other;
        };
        let call = Some(number).and_then(|number| {
            self.calls
                .iter()
                .chain(self.ended.iter().map(|(id, call)| (id, call)))
                .find(|(_, call)| call.number == number)
                .map(|(id, _)| id.clone())
        });
        match call {
            Some(id) => TagOwner::Call(id),
            None => TagOwner::GoneCall,
        }
    }
}

/// Whose sandbox a refusal came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagOwner {
    /// pi's own sandbox: an MCP server, an extension, pi.
    Session,
    /// A bash call the session still knows.
    Call(String),
    /// A bash call ended long enough ago to be forgotten, still running something (a background job).
    GoneCall,
    /// A command the user typed.
    Typed,
    /// Not this session's.
    Other,
}

/// What the daemon does with a request, before any question: run it (at the session's policy or a grant), ask, or
/// refuse.
pub enum First {
    Decided(RunDecision),
    /// Ask the user; what the agent asked for, resolved, the refusals recorded for the call so far, the tag its sandbox
    /// gets, and what `open` would do.
    Ask {
        requested: Requested,
        refusals: Vec<String>,
        tag: String,
        note: Option<String>,
    },
}

impl Commands {
    /// The first decision for `request`. `calls` is the session's running calls.
    pub fn decide(&self, calls: &mut Calls, request: &RunRequest) -> First {
        let not_run = |reason: String| First::Decided(RunDecision::NotRun { reason });
        if !self.inside(&request.cwd) {
            return not_run(format!("{} is outside this session's checkout", request.cwd.display()));
        }
        // A command the user typed: always at the session's policy, never widened.
        if request.typed_by_user() {
            if request.rerun.is_some() {
                return not_run("a command you type runs in the sandbox".into());
            }
            return self.run(
                request,
                Runtime::Session(tagged(&self.policies.launch, &calls.typed_tag())),
            );
        }
        let (call, tag) = match calls.take(request) {
            Ok(taken) => taken,
            Err(reason) => return not_run(reason),
        };
        match &request.rerun {
            Some(asked) => {
                match &call.rerun {
                    CallRerun::Asked(called) if called == asked => {}
                    CallRerun::Asked(_) => return not_run("the rerun isn't the one the call asked for".into()),
                    CallRerun::NotAsked => return not_run("the call didn't ask for a rerun (`rerun`)".into()),
                    CallRerun::Invalid(why) => return not_run(format!("the call's `rerun` is invalid: {why}")),
                }
                let requested = match self.resolve(asked) {
                    Ok(requested) => requested,
                    Err(why) => return not_run(format!("the requested grant can't be used: {why}")),
                };
                use crate::agent::open::{self, OpenCheck};
                let refusals = call.refusals.clone();
                let nodes = match crate::agent::analyze(&request.command, Some(&request.cwd)) {
                    Ok(analysis) => analysis.commands,
                    Err(e) => return not_run(e.to_string()),
                };
                let note = match open::check(&nodes, &request.cwd, &self.policies.launch) {
                    OpenCheck::Refused { reason } => return not_run(reason),
                    OpenCheck::Ask { note } => Some(note),
                    OpenCheck::Other => None,
                };
                First::Ask {
                    requested,
                    refusals,
                    tag,
                    note,
                }
            }
            None => match grant_for(&self.grants, &request.command, &request.cwd) {
                Ok(Matched::Grant(entry)) => {
                    let by = Why::Grant(entry.value.pattern.clone());
                    match self.policies.of(entry) {
                        Ok(EntryRuntime::Host) => self.run(request, Runtime::Host { by }),
                        Ok(EntryRuntime::Sandbox {
                            grant,
                            policy: Ok(policy),
                        }) => self.run(
                            request,
                            Runtime::Widened {
                                policy: tagged(policy, &tag),
                                grant: grant.clone(),
                                by,
                            },
                        ),
                        Ok(EntryRuntime::Sandbox {
                            policy: Err(reason), ..
                        }) => not_run(format!(
                            "the grant for `{}` can't be used: {reason}",
                            entry.value.pattern.pattern()
                        )),
                        Err(reason) => not_run(format!(
                            "the grant for `{}` can't be used: {reason}",
                            entry.value.pattern.pattern()
                        )),
                    }
                }
                Ok(Matched::None | Matched::Redirect) | Err(_) => {
                    self.run(request, Runtime::Session(tagged(&self.policies.launch, &tag)))
                }
            },
        }
    }

    fn run(&self, request: &RunRequest, runtime: Runtime) -> First {
        First::Decided(RunDecision::Run(Box::new(self.approved(request, runtime))))
    }

    /// What a requested rerun runs with: a narrowed grant resolved through the launch's configuration, as if this
    /// project's `config.local.toml` listed it (where "always in this project" saves it), so it never reaches the floor.
    pub fn resolve(&self, runtime: &CommandRuntime) -> Result<Requested, String> {
        runtime.check()?;
        match runtime {
            CommandRuntime::Host => Ok(Requested::Host),
            CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => Ok(Requested::All),
            CommandRuntime::Sandbox(Grant::Lists(lists)) => {
                let file = self
                    .scope_file(PersistScope::Project)
                    .ok_or_else(|| "this project has no configuration file to resolve it with".to_string())?;
                let policy = self.policies.resolver.with(lists, &file)?;
                Ok(Requested::Narrowed {
                    lists: lists.clone(),
                    policy: Box::new(policy),
                })
            }
        }
    }

    /// A rerun the user approved, where they chose: `all` or the requested grant in the sandbox, or the host. `runtime`
    /// is one the question offered (`Question::check`); a narrowed grant runs with the policy resolved before asking.
    pub fn approved_rerun(
        &self,
        request: &RunRequest,
        requested: &Requested,
        tag: &str,
        runtime: &CommandRuntime,
    ) -> Result<Approved, String> {
        let widened = |policy: &crate::launch::sandbox::Policy, grant: Grant| Runtime::Widened {
            policy: tagged(policy, tag),
            grant,
            by: Why::Approval,
        };
        let runtime = match runtime {
            CommandRuntime::Host => Runtime::Host { by: Why::Approval },
            CommandRuntime::Sandbox(grant @ Grant::All(AllGrant::All)) => widened(&self.policies.all, grant.clone()),
            CommandRuntime::Sandbox(grant @ Grant::Lists(lists)) => match requested {
                Requested::Narrowed { lists: asked, policy } if asked == lists => widened(policy, grant.clone()),
                Requested::Narrowed { .. } | Requested::All | Requested::Host => {
                    return Err("only the requested grant, `all`, or the host can be approved".into());
                }
            },
        };
        Ok(self.approved(request, runtime))
    }

    /// The command as the daemon runs it: where `runtime` says, the shell and prefix from pi's settings, and the
    /// environment the runtime gets.
    fn approved(&self, request: &RunRequest, runtime: Runtime) -> Approved {
        let env = match &runtime {
            // The session's own sandbox: the extension's (pi's shell environment). It reaches only the command, inside
            // its sandbox (`launch::handoff::for_helper`), never the helper that applies it.
            Runtime::Session(_) => request.env.clone(),
            // Widened: the session's launch environment, so a request can't point a granted command at its own `PATH`.
            Runtime::Widened { .. } => self.env.clone(),
            // On the host: the launch environment without what marks a sandboxed pi.
            Runtime::Host { .. } => crate::agent::host_env(&self.env),
        };
        Approved {
            runtime,
            shell: self.shell.shell.clone(),
            command: self.shell.command(&request.command),
            cwd: request.cwd.clone(),
            env,
            timeout: request
                .timeout
                .filter(|t| t.is_finite() && *t > 0.0)
                .map(Duration::from_secs_f64),
        }
    }

    /// The typed question for a rerun request: opens on what was asked.
    pub fn question(
        &self,
        id: String,
        request: &RunRequest,
        requested: &Requested,
        refusals: Vec<String>,
        note: Option<String>,
    ) -> Question {
        Question::new(
            id,
            request.command.clone(),
            request.reason.clone(),
            refusals,
            note,
            self.policies.writable_vars.clone(),
            requested.runtime(),
        )
    }

    /// Saves a per-command entry for `pattern` with exactly the approved `runtime` and grant.
    pub fn persist(
        &self,
        scope: PersistScope,
        pattern: CommandPattern,
        runtime: CommandRuntime,
    ) -> Result<PathBuf, String> {
        let file = self.file(scope);
        let grant = CommandGrant { pattern, runtime };
        let path = file.path(&self.paths).map_err(|e| e.to_string())?;
        file.append_command_grant(&self.paths, &grant)
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn inside(&self, cwd: &Path) -> bool {
        let cwd = crate::agent::canonical_target(cwd);
        [&self.checkout, &self.source]
            .iter()
            .any(|root| cwd.starts_with(crate::agent::canonical_target(root)))
    }
}

/// `policy` tagged with a call.
fn tagged(policy: &crate::launch::sandbox::Policy, tag: &str) -> crate::launch::sandbox::Policy {
    let mut policy = policy.clone();
    policy.tag = tag.to_string();
    policy
}

/// What an answer to an `rpc` dialog leads to.
pub enum Next {
    /// No answer yet (the first option, which decides nothing): show the question again.
    AskAgain,
    /// Show the question again on the other side.
    Switch,
    /// Show the question again at `all`.
    Widen,
    /// Ask the pattern dialog.
    AskPattern(CommandRuntime, PersistScope),
    /// The user decided.
    Decided(Answer),
}

/// Reads an answer to the `rpc` dialog at `stage`, shown for `question`.
pub fn rpc_answer(stage: Stage, question: &Question, answer: &Map<String, Value>) -> Next {
    let runtime = question.showing.clone();
    match stage {
        Stage::Choice => match question::read_rpc_choice(&runtime, answer) {
            RpcChoice::AskAgain => Next::AskAgain,
            RpcChoice::Switch => Next::Switch,
            RpcChoice::Widen => Next::Widen,
            RpcChoice::Once => Next::Decided(Answer::Once(runtime)),
            RpcChoice::Deny => Next::Decided(Answer::Deny),
            RpcChoice::Command(scope) => match CommandPattern::new(&question.command) {
                Ok(pattern) => Next::Decided(Answer::Always {
                    runtime,
                    scope,
                    pattern,
                }),
                // A multi-line command can't be a pattern: run it once.
                Err(_) => Next::Decided(Answer::Once(runtime)),
            },
            RpcChoice::Pattern(scope) => Next::AskPattern(runtime, scope),
        },
        Stage::Pattern(runtime, scope) => match question::read_pattern(answer) {
            Some(pattern) => Next::Decided(Answer::Always {
                runtime,
                scope,
                pattern,
            }),
            None => Next::Decided(Answer::Deny),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{config::Listed, launch::sandbox::Policy};

    fn policy() -> Policy {
        Policy {
            rules: Vec::new(),
            keychain: false,
            services: crate::launch::sandbox::Services::Every {
                source: crate::launch::sandbox::Source::Workspace,
            },
            apps: crate::launch::sandbox::Apps::Closed,
            network: Vec::new(),
            tag: String::new(),
        }
    }

    fn commands(dir: &Path, grants: &[&str]) -> Commands {
        let grants: Vec<Listed<CommandGrant>> = grants
            .iter()
            .map(|p| Listed {
                value: CommandGrant {
                    pattern: CommandPattern::new(p).unwrap(),
                    runtime: CommandRuntime::Sandbox(Grant::All(AllGrant::All)),
                },
                file: dir.join("g.toml"),
            })
            .collect();
        let mut all = policy();
        all.keychain = true;
        Commands {
            policies: CommandPolicies {
                launch: policy(),
                all,
                entries: grants
                    .iter()
                    .map(|g| {
                        (
                            g.clone(),
                            EntryRuntime::Sandbox {
                                grant: Grant::All(AllGrant::All),
                                policy: Ok(policy()),
                            },
                        )
                    })
                    .collect(),
                resolver: crate::launch::sandbox::grant::Resolver::new(
                    crate::config::LaunchConfig {
                        sandbox: Default::default(),
                        workers: Default::default(),
                        source: dir.to_path_buf(),
                        checkout: dir.to_path_buf(),
                        levels: Vec::new(),
                    },
                    resolved,
                ),
                silence: Vec::new(),
                credentials: crate::launch::sandbox::grant::Credentials::Liftable,
                claude_login: None,
                writable_vars: Vec::new(),
            },
            env: [("FROM".to_string(), "launch".to_string())].into_iter().collect(),
            shell: PiShell {
                shell: "/bin/bash".into(),
                prefix: Some("set -e".into()),
            },
            source: dir.to_path_buf(),
            checkout: dir.to_path_buf(),
            paths: Paths {
                home: dir.into(),
                global: dir.join("g"),
            },
            grants,
        }
    }

    /// The resolver's stand-in: one write rule per `write` entry the configuration lists, so a test can tell a narrowed
    /// grant's policy apart; an entry naming `$UNSET` doesn't resolve.
    fn resolved(config: &crate::config::LaunchConfig) -> Result<Policy, String> {
        let mut resolved = policy();
        for listed in &config.sandbox.write {
            if listed.value.contains("$UNSET") {
                return Err("the grant names $UNSET, which isn't set".into());
            }
            resolved.rules.push(crate::launch::sandbox::Rule {
                effect: crate::launch::sandbox::Effect::Allow,
                access: vec![crate::launch::sandbox::Access::Write],
                target: crate::launch::sandbox::Target::Subpath {
                    path: listed.value.clone().into(),
                },
                source: crate::launch::sandbox::Source::Workspace,
            });
        }
        Ok(resolved)
    }

    fn all() -> CommandRuntime {
        CommandRuntime::Sandbox(Grant::All(AllGrant::All))
    }

    fn narrowed(path: &str) -> CommandRuntime {
        CommandRuntime::Sandbox(Grant::Lists(Box::new(GrantLists {
            write: vec![path.into()],
            ..GrantLists::default()
        })))
    }

    /// pi's `tool_execution_start` for a bash call, with `rerun` as the agent wrote it.
    fn start_with(calls: &mut Calls, id: &str, command: &str, rerun: Option<Value>) {
        let mut args = json!({"command": command});
        if let Some(rerun) = rerun {
            args["rerun"] = rerun;
        }
        let record = json!({"toolCallId": id, "toolName": "bash", "args": args});
        calls.track("tool_execution_start", record.as_object().unwrap());
    }

    fn start(calls: &mut Calls, id: &str, command: &str, rerun: Option<CommandRuntime>) {
        start_with(calls, id, command, rerun.map(|r| serde_json::to_value(r).unwrap()));
    }

    fn request(dir: &Path, id: &str, command: &str, rerun: Option<CommandRuntime>) -> RunRequest {
        RunRequest {
            call_id: id.into(),
            command: command.into(),
            cwd: dir.to_path_buf(),
            timeout: Some(5.0),
            env: [("FROM".to_string(), "request".to_string())].into_iter().collect(),
            reason: None,
            rerun,
        }
    }

    /// What a request asked for, resolved: panics when it isn't asked.
    fn asked(first: First) -> (Requested, String) {
        match first {
            First::Ask { requested, tag, .. } => (requested, tag),
            First::Decided(RunDecision::Run(_)) => panic!("ran"),
            First::Decided(RunDecision::NotRun { reason }) => panic!("refused: {reason}"),
        }
    }

    fn kind(first: &First) -> &'static str {
        match first {
            First::Decided(RunDecision::Run(_)) => "run",
            First::Decided(RunDecision::NotRun { .. }) => "refused",
            First::Ask { .. } => "ask",
        }
    }

    /// The sandbox policy a run got: panics for a host run.
    fn sandbox(approved: &Approved) -> &crate::launch::sandbox::Policy {
        match &approved.runtime {
            Runtime::Session(policy) | Runtime::Widened { policy, .. } => policy,
            Runtime::Host { .. } => panic!("ran on the host"),
        }
    }

    fn run(first: First) -> Box<Approved> {
        match first {
            First::Decided(RunDecision::Run(approved)) => approved,
            First::Decided(RunDecision::NotRun { reason }) => panic!("refused: {reason}"),
            First::Ask { .. } => panic!("asked"),
        }
    }

    /// A request is accepted only for a running call with the same command, once. Every call runs, each in its own
    /// tagged sandbox: at the session's policy with pi's environment, or a grant's with the launch environment. A rerun
    /// asks.
    #[test]
    fn requests_follow_pi_s_calls() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let c = commands(&dir, &["git push *"]);
        let mut calls = Calls::new("7-ab");
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "1", "git push", None))),
            "refused",
            "not running"
        );

        start(&mut calls, "1", "git push", None);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "1", "rm -rf ~", None))),
            "refused",
            "other command"
        );
        let granted = run(c.decide(&mut calls, &request(&dir, "1", "git push", None)));
        assert_eq!(granted.command, "set -e\ngit push", "pi's prefix");
        assert_eq!(granted.timeout, Some(Duration::from_secs(5)));
        assert_eq!(sandbox(&granted).tag, "7-ab-1");
        assert!(matches!(granted.runtime, Runtime::Widened { by: Why::Grant(_), .. }));
        assert_eq!(
            granted.env["FROM"], "launch",
            "a widened run gets the launch environment"
        );
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "1", "git push", None))),
            "refused",
            "once per call"
        );

        start(&mut calls, "2", "ls", None);
        let plain = run(c.decide(&mut calls, &request(&dir, "2", "ls", None)));
        assert!(matches!(plain.runtime, Runtime::Session(_)), "the session's own policy");
        assert_eq!(sandbox(&plain).tag, "7-ab-2");
        assert_eq!(sandbox(&plain).rules, c.policies.launch.rules);
        assert_eq!(plain.env["FROM"], "request", "pi's shell environment");
        assert_eq!(calls.owner("7-ab-2"), TagOwner::Call("2".into()));
        assert_eq!(calls.owner("7-ab"), TagOwner::Session, "pi's own sandbox");
        assert_eq!(calls.owner("7-ab-99"), TagOwner::GoneCall, "a call long forgotten");
        assert_eq!(calls.owner("7-abc-2"), TagOwner::Other);

        let typed = run(c.decide(&mut calls, &request(&dir, "user:1", "ls ~", None)));
        assert!(
            matches!(typed.runtime, Runtime::Session(_)),
            "a typed command is never widened"
        );
        assert_eq!(calls.owner(&sandbox(&typed).tag), TagOwner::Typed);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "user:2", "ls", Some(all())))),
            "refused"
        );

        start(&mut calls, "9", "husky install", Some(all()));
        let req = request(&dir, "9", "husky install", Some(all()));
        let (requested, tag) = asked(c.decide(&mut calls, &req));
        assert_eq!(requested, Requested::All);
        let rerun = c.approved_rerun(&req, &requested, &tag, &all()).unwrap();
        assert!(sandbox(&rerun).keychain, "at `all`");
        assert_eq!(sandbox(&rerun).tag, tag);
        assert_eq!(rerun.env["FROM"], "launch");

        start(&mut calls, "5", "git push", None);
        let outside = request(Path::new("/"), "5", "git push", None);
        assert_eq!(
            kind(&c.decide(&mut calls, &outside)),
            "refused",
            "cwd outside the checkout"
        );

        calls.track("tool_execution_end", json!({"toolCallId": "2"}).as_object().unwrap());
        assert!(calls.running().all(|(id, _)| id != "2"));
        calls.track("agent_end", &Map::new());
        assert_eq!(calls.running().count(), 0);
    }

    /// A rerun is accepted only when the call asked for exactly it, read with the `[[sandbox.commands]]` parser. A
    /// narrowed grant resolves through the launch's configuration before anyone is asked; one that can't is refused.
    #[test]
    fn a_rerun_is_what_the_call_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let c = commands(&dir, &[]);
        let mut calls = Calls::new("7-ab");
        let refused = |calls: &mut Calls, id: &str, rerun: Option<CommandRuntime>| match c
            .decide(calls, &request(&dir, id, "husky install", rerun))
        {
            First::Decided(RunDecision::NotRun { reason }) => reason,
            First::Decided(RunDecision::Run(_)) | First::Ask { .. } => panic!("not refused"),
        };

        start(&mut calls, "1", "husky install", None);
        assert!(refused(&mut calls, "1", Some(all())).contains("didn't ask"));
        start(&mut calls, "2", "husky install", Some(CommandRuntime::Host));
        assert!(refused(&mut calls, "2", Some(all())).contains("isn't the one"));
        start(&mut calls, "3", "husky install", Some(narrowed("/a")));
        assert!(refused(&mut calls, "3", Some(narrowed("/b"))).contains("isn't the one"));
        // What pi reported but the shared parser refuses: a host rerun with a grant, a sandbox one without.
        for (id, bad, says) in [
            ("4", json!({"runtime": "host", "grant": "all"}), "`grant` is for"),
            ("5", json!({"runtime": "sandbox"}), "needs `grant`"),
            ("6", json!(true), ""),
        ] {
            start_with(&mut calls, id, "husky install", Some(bad.clone()));
            let reason = refused(&mut calls, id, Some(all()));
            assert!(reason.contains("invalid") && reason.contains(says), "{bad}: {reason}");
        }
        // A narrowed grant that doesn't resolve, or names a relative path, is refused before asking.
        start(&mut calls, "7", "husky install", Some(narrowed("$UNSET/x")));
        assert!(refused(&mut calls, "7", Some(narrowed("$UNSET/x"))).contains("$UNSET"));
        start(&mut calls, "8", "husky install", Some(narrowed("rel")));
        assert!(refused(&mut calls, "8", Some(narrowed("rel"))).contains("must be absolute"));

        start(&mut calls, "9", "husky install", Some(CommandRuntime::Host));
        let (requested, _) = asked(c.decide(
            &mut calls,
            &request(&dir, "9", "husky install", Some(CommandRuntime::Host)),
        ));
        assert_eq!(requested, Requested::Host);
        start(&mut calls, "10", "husky install", Some(narrowed("/a")));
        let (requested, _) = asked(c.decide(&mut calls, &request(&dir, "10", "husky install", Some(narrowed("/a")))));
        match &requested {
            Requested::Narrowed { lists, policy } => {
                assert_eq!(lists.write, ["/a"]);
                assert_eq!(policy.rules.len(), 1, "resolved with the grant's lists");
            }
            Requested::Host | Requested::All => panic!("not narrowed"),
        }
        assert_eq!(requested.runtime(), narrowed("/a"));
    }

    /// A refusal goes to its call (running or just ended); a rerun shows what the last run of the same command was
    /// refused; a process block is offered once.
    #[test]
    fn refusals_follow_their_call() {
        let mut calls = Calls::new("1-x");
        start(&mut calls, "1", "husky install", None);
        assert!(calls.note("1", "file-write-create /w/.git/hooks/pre-commit"));
        assert!(
            calls.note("1", "file-write-create /w/.git/hooks/pre-commit"),
            "kept once"
        );
        assert!(!calls.note("9", "x"), "unknown call");
        calls.track("tool_execution_end", json!({"toolCallId": "1"}).as_object().unwrap());
        assert!(
            calls.note("1", "file-write-data /w/.git/config"),
            "recorded after it ended"
        );
        start(&mut calls, "2", "husky install", Some(all()));
        let rerun = calls.get_running("2").unwrap();
        assert_eq!(
            rerun.refusals,
            [
                "file-write-create /w/.git/hooks/pre-commit",
                "file-write-data /w/.git/config"
            ]
        );

        let mut blocks = ProcessBlocks::default();
        assert!(blocks.first("node", "file-write-create", "/h/x"));
        assert!(!blocks.first("node", "file-write-create", "/h/x"));
        assert!(blocks.first("node", "file-write-create", "/h/y"));
        let question = |id: &str| ProcessQuestion {
            id: id.into(),
            process: "node".into(),
            refusal: "file-write-create /h/x".into(),
            offer: crate::launch::sandbox::grant::Offer {
                project: crate::launch::sandbox::grant::Lift::WriteFile("/h/x".into()),
                everywhere: crate::launch::sandbox::grant::Lift::WriteFile("/h/x".into()),
            },
        };
        blocks.queue(question("a"));
        blocks.queue(question("b"));
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("a".into()));
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("a".into()), "one at a time");
        assert!(blocks.asking("a"));
        // A question that lost its dialog unanswered (the run ended) is asked again, before the ones behind it.
        blocks.requeue_unanswered();
        assert!(blocks.asking.is_none());
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("a".into()));
        blocks.asking = None;
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("b".into()));
        // Nothing is dropped, however many wait.
        blocks.asking = None;
        for i in 0..20 {
            blocks.queue(question(&i.to_string()));
        }
        let mut shown = 0;
        while blocks.next().is_some() {
            shown += 1;
            blocks.asking = None;
        }
        assert_eq!(shown, 20);
    }

    /// Each `rpc` option leads to the same typed answer a `view` client sends, at the runtime the dialog showed: the
    /// question opens on the request, a narrowed grant can be widened to `all`, and switching reaches the host.
    #[test]
    fn rpc_answers_map_to_typed_answers() {
        let dir = tempfile::tempdir().unwrap();
        let c = commands(dir.path(), &[]);
        let mut req = request(dir.path(), "1", "husky install", Some(narrowed("/a")));
        req.reason = Some("hooks".into());
        let requested = c.resolve(&narrowed("/a")).unwrap();
        let narrow = c.question("q".into(), &req, &requested, Vec::new(), None);
        assert_eq!(narrow.showing, narrowed("/a"), "opens on the request");
        let sandbox = narrow.widened();
        let host = sandbox.switched();
        let labels = |q: &Question| -> Vec<String> {
            question::rpc_dialog("d", q)["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        let a = |q: &Question, stage, value: &str| rpc_answer(stage, q, json!({"value": value}).as_object().unwrap());
        let decided = |next: Next| match next {
            Next::Decided(answer) => answer,
            Next::AskAgain | Next::Switch | Next::Widen | Next::AskPattern(..) => panic!("not decided"),
        };
        let (n, s, h) = (labels(&narrow), labels(&sandbox), labels(&host));
        assert!(matches!(a(&narrow, Stage::Choice, &n[7]), Next::Widen));
        assert!(matches!(a(&narrow, Stage::Choice, &n[8]), Next::Switch));
        assert_eq!(decided(a(&narrow, Stage::Choice, &n[1])), Answer::Once(narrowed("/a")));
        assert_eq!(
            decided(a(&narrow, Stage::Choice, &n[3])),
            Answer::Always {
                runtime: narrowed("/a"),
                scope: PersistScope::Project,
                pattern: CommandPattern::new("husky install").unwrap(),
            }
        );
        assert!(matches!(a(&sandbox, Stage::Choice, &s[0]), Next::AskAgain));
        assert!(matches!(a(&sandbox, Stage::Choice, &s[7]), Next::Switch));
        assert_eq!(decided(a(&sandbox, Stage::Choice, &s[1])), Answer::Once(all()));
        assert_eq!(
            decided(a(&host, Stage::Choice, &h[1])),
            Answer::Once(CommandRuntime::Host)
        );
        assert_eq!(decided(a(&host, Stage::Choice, &h[2])), Answer::Deny);
        assert_eq!(
            decided(a(&host, Stage::Choice, &h[3])),
            Answer::Always {
                runtime: CommandRuntime::Host,
                scope: PersistScope::Project,
                pattern: CommandPattern::new("husky install").unwrap(),
            }
        );
        assert!(matches!(
            a(&host, Stage::Choice, &h[6]),
            Next::AskPattern(CommandRuntime::Host, PersistScope::Global)
        ));
        let pattern = Stage::Pattern(CommandRuntime::Host, PersistScope::Global);
        assert_eq!(
            decided(a(&host, pattern.clone(), "husky *")),
            Answer::Always {
                runtime: CommandRuntime::Host,
                scope: PersistScope::Global,
                pattern: CommandPattern::new("husky *").unwrap(),
            }
        );
        assert_eq!(decided(a(&host, pattern, "")), Answer::Deny);
        // One side's labels don't approve another's run.
        assert_eq!(decided(a(&sandbox, Stage::Choice, &h[1])), Answer::Deny);
        assert_eq!(decided(a(&narrow, Stage::Choice, &s[1])), Answer::Deny);
    }

    /// An approved rerun runs where the user chose: `all` or the requested grant (its resolved policy, not `all`) with
    /// the launch environment, or the host without the session's own variables. Nothing else can be approved.
    #[test]
    fn an_approved_rerun_runs_where_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = commands(dir.path(), &[]);
        c.env.insert("PITON_RUNTIME".into(), "sandbox".into());
        let req = request(dir.path(), "1", "husky install", Some(narrowed("/a")));
        let requested = c.resolve(&narrowed("/a")).unwrap();
        let sandboxed = c.approved_rerun(&req, &requested, "t-1", &all()).unwrap();
        match &sandboxed.runtime {
            Runtime::Widened { policy, grant, by } => {
                assert!(policy.keychain, "at `all`");
                assert_eq!(policy.tag, "t-1");
                assert_eq!(*grant, Grant::All(AllGrant::All));
                assert_eq!(*by, Why::Approval);
            }
            Runtime::Session(_) | Runtime::Host { .. } => panic!("not widened"),
        }
        assert_eq!(sandboxed.env["PITON_RUNTIME"], "sandbox");
        let narrow = c.approved_rerun(&req, &requested, "t-1", &narrowed("/a")).unwrap();
        match &narrow.runtime {
            Runtime::Widened { policy, grant, .. } => {
                assert!(!policy.keychain, "not `all`");
                assert_eq!(policy.rules.len(), 1, "the policy resolved with the grant");
                assert_eq!(policy.tag, "t-1");
                assert_eq!(CommandRuntime::Sandbox(grant.clone()), narrowed("/a"));
            }
            Runtime::Session(_) | Runtime::Host { .. } => panic!("not widened"),
        }
        assert_eq!(narrow.env["FROM"], "launch");
        assert!(c.approved_rerun(&req, &requested, "t-1", &narrowed("/b")).is_err());
        assert!(
            c.approved_rerun(&req, &Requested::All, "t-1", &narrowed("/a")).is_err(),
            "a narrowed grant nobody asked for"
        );
        let host = c
            .approved_rerun(&req, &requested, "t-1", &CommandRuntime::Host)
            .unwrap();
        assert_eq!(host.runtime, Runtime::Host { by: Why::Approval });
        assert_eq!(host.env["FROM"], "launch");
        assert!(!host.env.contains_key("PITON_RUNTIME"));
        assert_eq!(host.command, "set -e\nhusky install");
    }

    /// A saved host entry runs its matching command on the host, without asking; a sandbox entry runs widened.
    #[test]
    fn a_host_entry_runs_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let mut c = commands(&dir, &["husky install *"]);
        let host = Listed {
            value: CommandGrant {
                pattern: CommandPattern::new("log show *").unwrap(),
                runtime: CommandRuntime::Host,
            },
            file: dir.join("g.toml"),
        };
        c.grants.push(host.clone());
        c.policies.entries.push((host, EntryRuntime::Host));
        let mut calls = Calls::new("7-ab");
        start(&mut calls, "1", "log show --last 1m", None);
        let ran = run(c.decide(&mut calls, &request(&dir, "1", "log show --last 1m", None)));
        assert_eq!(
            ran.runtime,
            Runtime::Host {
                by: Why::Grant(CommandPattern::new("log show *").unwrap())
            }
        );
        assert_eq!(ran.env["FROM"], "launch");
        // A redirect gets no entry at all: the session's sandbox.
        start(&mut calls, "2", "log show > ~/.zshrc", None);
        let redirected = run(c.decide(&mut calls, &request(&dir, "2", "log show > ~/.zshrc", None)));
        assert!(matches!(redirected.runtime, Runtime::Session(_)));
    }

    /// "Always" saves exactly what was approved: `all`, the narrowed grant, or the host.
    #[test]
    fn persists_to_the_chosen_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = commands(dir.path(), &[]);
        let global = c
            .persist(
                PersistScope::Global,
                CommandPattern::new("husky install").unwrap(),
                all(),
            )
            .unwrap();
        assert_eq!(global, dir.path().join("g/config.toml"));
        let text = std::fs::read_to_string(&global).unwrap();
        assert!(
            text.contains("runtime = \"sandbox\"") && text.contains("grant = \"all\""),
            "{text}"
        );
        let project = c
            .persist(
                PersistScope::Project,
                CommandPattern::new("git push *").unwrap(),
                CommandRuntime::Host,
            )
            .unwrap();
        assert_eq!(project, dir.path().join(".piton/config.local.toml"));
        let text = std::fs::read_to_string(&project).unwrap();
        assert!(
            text.contains("bash(git push *)") && text.contains("runtime = \"host\""),
            "{text}"
        );
        assert!(!text.contains("grant"), "{text}");

        c.persist(
            PersistScope::Project,
            CommandPattern::new("mise run cli:test*").unwrap(),
            narrowed("$TMPDIR"),
        )
        .unwrap();
        let saved: toml::Table = toml::from_str(&std::fs::read_to_string(&project).unwrap()).unwrap();
        let entry = &saved["sandbox"]["commands"][1];
        assert_eq!(entry["match"].as_str(), Some("bash(mise run cli:test*)"));
        assert_eq!(entry["runtime"].as_str(), Some("sandbox"));
        assert_eq!(entry["grant"]["write"][0].as_str(), Some("$TMPDIR"));
    }
}
