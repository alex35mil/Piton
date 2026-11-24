//! A session's commands that run through the daemon: which bash calls pi is running (from its own events), what a
//! request to run one through the daemon gets, and the block question while the user decides.
//!
//! A request over `agent.sock` is only a request. The daemon accepts one for a bash call pi reported running
//! (`tool_execution_start`) with exactly that command, once per call; a rerun only when that call's arguments asked for
//! it (`rerun`, hinting the side the question opens on). It decides where it runs (a per-command entry it matched itself, or, once the user
//! approves, `all` in the sandbox or the host), the shell and its prefix (pi's settings files), and the environment
//! (the session's launch environment).

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
        question::{self, Answer, Question, RpcChoice, Target},
    },
    config::{AllGrant, CommandGrant, CommandPattern, CommandRuntime, ConfigFile, Grant, Paths, Sharing, WriteLevel},
    launch::{
        Env,
        sandbox::grant::{CommandPolicies, EntryRuntime},
    },
};

/// A bash call pi is running.
#[derive(Debug, Clone)]
pub struct BashCall {
    pub command: String,
    /// The rerun the call's arguments asked for (pi validated them before the call ran).
    pub rerun: Option<crate::agent::Rerun>,
    /// A request was taken for it: one per call.
    pub used: bool,
    /// The kernel's refusals routed to this call.
    pub refusals: Vec<CallRefusal>,
    /// The call's number in its sandbox's tag.
    pub number: u64,
    /// Piton asked the user about it after it failed: once per call.
    pub asked: bool,
    /// The user denied running this command outside the sandbox in this run (Piton's question about it, or the last run
    /// of the same command that this rerun follows).
    pub declined: bool,
}

/// The process name a gate refusal is recorded under.
pub const GATE_PROCESS: &str = "gate";

/// One refusal of a call's sandbox, as the kernel names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRefusal {
    pub process: String,
    pub operation: String,
    pub target: String,
}

impl CallRefusal {
    /// `operation target`, as the question shows it.
    pub fn summary(&self) -> String {
        match self.target.is_empty() {
            true => self.operation.clone(),
            false => format!("{} {}", self.operation, self.target),
        }
    }
}

impl From<&crate::daemon::Refusal> for CallRefusal {
    fn from(refusal: &crate::daemon::Refusal) -> Self {
        Self {
            process: refusal.process.clone(),
            operation: refusal.operation.clone(),
            target: refusal.target.clone(),
        }
    }
}

/// What the session decides about a call that failed in its own sandbox, before the agent hears of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterFailure {
    /// No block recorded yet: the kernel's record may still come.
    Wait,
    /// Not asked: the blocks (with why each isn't asked about) and anything else the agent should know.
    Report { blocks: Vec<String>, note: Option<String> },
    /// Ask the user: the blocks, the refusals the question shows, the tag a rerun's sandbox gets, and what `open`
    /// would do.
    Ask {
        blocks: Vec<String>,
        refusals: Vec<String>,
        tag: String,
        note: Option<String>,
    },
}

/// How a failed call's block is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Treated {
    /// An approved rerun lifts it.
    Asked,
    /// Not asked about, with why.
    Shown(&'static str),
    /// A refusal programs hit and recover from: listed only when nothing else was recorded.
    Incidental,
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
    /// The configuration levels the launch read (its `.piton/` and `.agents/` folders, the global tier included).
    pub levels: Vec<PathBuf>,
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
        let entry = lift.entry();
        let file = self.file(scope);
        let path = file.path(&self.paths).map_err(|e| e.to_string())?;
        file.write(
            &self.paths,
            &[crate::config::KeyChange {
                key: crate::config::KeyPath::new(entry.path()),
                value: Some(entry.value),
            }],
        )
        .map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// Whether this session's configuration reads `file`: one of the files a network question saves to (its own
    /// project's or the global one, which exist or not), or a file in a level folder of its chain.
    pub fn reads(&self, file: &Path) -> bool {
        let own = [PersistScope::Project, PersistScope::Global]
            .into_iter()
            .filter_map(|scope| self.scope_file(scope));
        let in_level = |dir: &Path| self.levels.iter().any(|l| l == dir);
        own.into_iter().any(|f| f == file) || file.parent().is_some_and(in_level)
    }

    /// A saved connection entry joins the launch's policy and every narrowed grant's, so calls started from now on reach
    /// it. (`all` opens the network already.)
    pub fn add_network_entry(&mut self, target: crate::config::NetworkTarget, source: crate::launch::sandbox::Source) {
        let grant = crate::launch::sandbox::NetworkGrant {
            target,
            effect: crate::config::Effect::Allow,
            source,
        };
        let add = |policy: &mut crate::launch::sandbox::Policy| {
            if let crate::launch::sandbox::Network::Filtered { grants, .. } = &mut policy.network
                && !grants.contains(&grant)
            {
                grants.push(grant.clone());
            }
        };
        add(&mut self.policies.launch);
        for (_, runtime) in &mut self.policies.entries {
            if let EntryRuntime::Sandbox(Ok(policy)) = runtime {
                add(policy);
            }
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Choice,
    Pattern(Target, PersistScope),
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

    /// Follows `tool_execution_start` and `tool_execution_end` of bash calls. Returns the tags of the calls that ended.
    pub fn track(&mut self, kind: &str, record: &Map<String, Value>) -> Vec<String> {
        let id = || record.get("toolCallId").and_then(Value::as_str).map(str::to_string);
        let before: Vec<u64> = self.calls.values().map(|c| c.number).collect();
        self.follow(kind, record, id);
        before
            .into_iter()
            .filter(|n| !self.calls.values().any(|c| c.number == *n))
            .map(|n| format!("{}-{n}", self.tag_prefix))
            .collect()
    }

    fn follow(&mut self, kind: &str, record: &Map<String, Value>, id: impl Fn() -> Option<String>) {
        match kind {
            "tool_execution_start" if record.get("toolName").and_then(Value::as_str) == Some("bash") => {
                let Some(id) = id() else { return };
                let args = record.get("args");
                let command = args.and_then(|a| a.get("command")).and_then(Value::as_str);
                let Some(command) = command else { return };
                let rerun = args
                    .and_then(|a| a.get("rerun"))
                    .and_then(|r| serde_json::from_value::<crate::agent::Rerun>(r.clone()).ok());
                // A rerun shows what the last run of the same command was refused, and keeps the user's denial of
                // Piton's question about it.
                let (refusals, declined) = match rerun {
                    Some(_) => self
                        .ended
                        .iter()
                        .rev()
                        .find(|(_, call)| call.command == command)
                        .map(|(_, call)| (call.refusals.clone(), call.declined))
                        .unwrap_or_default(),
                    None => (Vec::new(), false),
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
                        asked: false,
                        declined,
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
            // A run that ended (aborted, crashed) leaves no call running. A denial holds for the run it was given in:
            // the user may ask the agent to try again in the next one.
            "agent_end" => {
                for (id, call) in std::mem::take(&mut self.calls) {
                    self.end(id, call);
                }
                for (_, call) in &mut self.ended {
                    call.declined = false;
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
    pub fn note(&mut self, id: &str, refusal: CallRefusal) -> bool {
        let call = match self.calls.get_mut(id) {
            Some(call) => call,
            None => match self.ended.iter_mut().rev().find(|(i, _)| i == id) {
                Some((_, call)) => call,
                None => return false,
            },
        };
        if call.refusals.len() < REFUSALS_KEPT && !call.refusals.contains(&refusal) {
            call.refusals.push(refusal);
        }
        true
    }

    /// The command of the call `id`, running or recently ended.
    pub fn command_of(&self, id: &str) -> Option<&str> {
        self.calls
            .get(id)
            .or_else(|| self.ended.iter().rev().find(|(i, _)| i == id).map(|(_, c)| c))
            .map(|c| c.command.as_str())
    }

    /// The user denied Piton's question about the running call `id`: a rerun the agent asks for in this run is refused.
    pub fn decline(&mut self, id: &str) {
        if let Some(call) = self.calls.get_mut(id) {
            call.declined = true;
        }
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
    /// Ask the user; the refusals recorded for the call so far, the tag its sandbox gets, and what `open` would do.
    Ask {
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
        match request.rerun {
            Some(asked) => match call.rerun {
                Some(_) if call.declined => {
                    not_run("the user already declined to run this command outside the sandbox; don't ask again".into())
                }
                Some(called) if called == asked => {
                    use crate::agent::open::{self, OpenCheck};
                    let refusals = call.refusals.iter().map(CallRefusal::summary).collect();
                    let nodes = match crate::agent::analyze(&request.command, Some(&request.cwd)) {
                        Ok(analysis) => analysis.commands,
                        Err(e) => return not_run(e.to_string()),
                    };
                    match open::check(&nodes, &request.cwd, &self.policies.launch) {
                        OpenCheck::Refused { reason } => not_run(reason),
                        OpenCheck::Ask { note } => First::Ask {
                            refusals,
                            tag,
                            note: Some(note),
                        },
                        OpenCheck::Other => First::Ask {
                            refusals,
                            tag,
                            note: None,
                        },
                    }
                }
                Some(_) => not_run("the rerun isn't the one the call asked for".into()),
                None => not_run("the call didn't ask for a rerun (`rerun`)".into()),
            },
            None => match grant_for(&self.grants, &request.command, &request.cwd) {
                Ok(Matched::Grant(entry)) => {
                    let by = Why::Grant(entry.value.pattern.clone());
                    match self.policies.of(entry) {
                        Ok(EntryRuntime::Host) => self.run(request, Runtime::Host { by }),
                        Ok(EntryRuntime::Sandbox(Ok(policy))) => self.run(
                            request,
                            Runtime::Widened {
                                policy: tagged(policy, &tag),
                                by,
                            },
                        ),
                        Ok(EntryRuntime::Sandbox(Err(reason))) => not_run(format!(
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

    /// What a call that failed in the session's own sandbox gets before the agent hears of it: Piton asks the user when
    /// a block it recorded for the call is one an approved rerun lifts; otherwise the blocks are reported. `last` is the
    /// last look (the kernel's record had its time). `slot_taken`: another question is showing.
    pub fn after_failure(&self, calls: &mut Calls, request: &RunRequest, last: bool, slot_taken: bool) -> AfterFailure {
        use crate::{
            agent::open::{self, OpenCheck},
            launch::sandbox::grant::{CommandBlock, command_block},
        };
        let report = |blocks, note| AfterFailure::Report { blocks, note };
        let prefix = calls.tag_prefix.clone();
        let Some(call) = calls.calls.get_mut(&request.call_id) else {
            return report(Vec::new(), None);
        };
        if request.typed_by_user() || call.command != request.command || call.asked {
            return report(Vec::new(), None);
        }
        let refusals = call.refusals.clone();
        let launch_ops = |r: &CallRefusal| matches!(r.operation.as_str(), "lsopen" | "appleevent-send");
        if refusals.is_empty() {
            return match last {
                true => report(Vec::new(), None),
                false => AfterFailure::Wait,
            };
        }
        let treated: Vec<(String, Treated)> = refusals
            .iter()
            .map(|r| {
                let silenced =
                    crate::launch::sandbox::silenced(&self.policies.silence, &r.process, &r.operation, &r.target)
                        .is_some();
                let treated = match (silenced, r.process.as_str()) {
                    (true, _) => Treated::Shown("silenced"),
                    // The gate already asked the user about this host, or a `silence` entry refused it.
                    (false, GATE_PROCESS) => Treated::Shown("refused by the network gate"),
                    (false, _) => {
                        match command_block(&self.policies.launch, &self.policies.all, &r.operation, &r.target) {
                            CommandBlock::Liftable => Treated::Asked,
                            CommandBlock::CredentialRead => Treated::Shown("a credential read"),
                            CommandBlock::Floor => Treated::Shown("protected: no approval lifts it"),
                            CommandBlock::Unsupported => Treated::Shown("not one an approval lifts"),
                            CommandBlock::Incidental => Treated::Incidental,
                        }
                    }
                };
                (r.summary(), treated)
            })
            .collect();
        // Incidental refusals are listed only when nothing else was recorded: they never explain a failure on their
        // own, but when they're all there is, the agent sees them rather than nothing.
        let only_incidental = treated.iter().all(|(_, t)| *t == Treated::Incidental);
        // An incidental refusal often comes first (`open` writes a sysctl on its way to Launch Services): keep waiting
        // for the one that explains the failure until the last look.
        if only_incidental && !last {
            return AfterFailure::Wait;
        }
        let blocks: Vec<String> = treated
            .iter()
            .filter_map(|(summary, treated)| match treated {
                Treated::Asked => Some(summary.clone()),
                Treated::Shown(why) => Some(format!("{summary} ({why})")),
                Treated::Incidental => only_incidental.then(|| format!("{summary} (incidental)")),
            })
            .collect();
        if !treated.iter().any(|(_, t)| *t == Treated::Asked) {
            return report(blocks, None);
        }
        if slot_taken {
            return report(
                blocks,
                Some("Piton didn't ask about it: another question was waiting for the user".into()),
            );
        }
        let note = match refusals.iter().any(launch_ops) {
            true => match open::check(
                &crate::agent::analyze(&request.command, Some(&request.cwd))
                    .map(|a| a.commands)
                    .unwrap_or_default(),
                &request.cwd,
                &self.policies.launch,
            ) {
                OpenCheck::Refused { reason } => return report(blocks, Some(reason)),
                OpenCheck::Ask { note } => Some(note),
                OpenCheck::Other => None,
            },
            false => None,
        };
        call.asked = true;
        AfterFailure::Ask {
            refusals: treated
                .into_iter()
                .filter(|(_, t)| *t != Treated::Incidental)
                .map(|(summary, _)| summary)
                .collect(),
            blocks,
            tag: format!("{prefix}-{}", call.number),
            note,
        }
    }

    fn run(&self, request: &RunRequest, runtime: Runtime) -> First {
        First::Decided(RunDecision::Run(Box::new(self.approved(request, runtime))))
    }

    /// A rerun the user approved, where they chose: at `all` in the sandbox, or on the host.
    pub fn approved_rerun(&self, request: &RunRequest, tag: &str, target: Target) -> Approved {
        let runtime = match target {
            Target::Sandbox => Runtime::Widened {
                policy: tagged(&self.policies.all, tag),
                by: Why::Approval,
            },
            Target::Host => Runtime::Host { by: Why::Approval },
        };
        self.approved(request, runtime)
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

    /// The typed question for a rerun request (opens on the side the agent hinted) or for Piton's own after a failed
    /// call (opens in the sandbox).
    pub fn question(
        &self,
        id: String,
        request: &RunRequest,
        asker: question::Asker,
        refusals: Vec<String>,
        note: Option<String>,
    ) -> Question {
        Question {
            id,
            command: request.command.clone(),
            asker,
            refusals,
            note,
            writable_vars: self.policies.writable_vars.clone(),
            runtime: request.rerun.map_or(Target::Sandbox, |r| r.runtime),
        }
    }

    /// Saves a per-command entry for `pattern`: `runtime = "sandbox"` with `grant = "all"`, or `runtime = "host"`.
    pub fn persist(&self, scope: PersistScope, pattern: CommandPattern, target: Target) -> Result<PathBuf, String> {
        let file = self.file(scope);
        let grant = CommandGrant {
            pattern,
            runtime: match target {
                Target::Sandbox => CommandRuntime::Sandbox(Grant::All(AllGrant::All)),
                Target::Host => CommandRuntime::Host,
            },
        };
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
    /// Show the question again in the other runtime.
    Switch,
    /// Ask the pattern dialog.
    AskPattern(Target, PersistScope),
    /// The user decided.
    Decided(Answer),
}

/// Reads an answer to the `rpc` dialog at `stage`, shown for `question`.
pub fn rpc_answer(stage: Stage, question: &Question, answer: &Map<String, Value>) -> Next {
    let target = question.runtime;
    match stage {
        Stage::Choice => match question::read_rpc_choice(target, answer) {
            RpcChoice::AskAgain => Next::AskAgain,
            RpcChoice::Switch => Next::Switch,
            RpcChoice::Once => Next::Decided(Answer::Once(target)),
            RpcChoice::Deny => Next::Decided(Answer::Deny),
            RpcChoice::Command(scope) => match CommandPattern::new(&question.command) {
                Ok(pattern) => Next::Decided(Answer::Always { target, scope, pattern }),
                // A multi-line command can't be a pattern: run it once.
                Err(_) => Next::Decided(Answer::Once(target)),
            },
            RpcChoice::Pattern(scope) => Next::AskPattern(target, scope),
        },
        Stage::Pattern(target, scope) => match question::read_pattern(answer) {
            Some(pattern) => Next::Decided(Answer::Always { target, scope, pattern }),
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
            network: crate::launch::sandbox::Network::Open {
                source: crate::launch::sandbox::Source::Workspace,
            },
            listen: crate::launch::sandbox::Listen::Closed,
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
                    .map(|g| (g.clone(), EntryRuntime::Sandbox(Ok(Box::new(policy())))))
                    .collect(),
                resolver: crate::launch::sandbox::grant::Resolver::new(
                    crate::config::LaunchConfig {
                        sandbox: Default::default(),
                        workers: Default::default(),
                        source: dir.to_path_buf(),
                        checkout: dir.to_path_buf(),
                        levels: Vec::new(),
                        untrusted: Vec::new(),
                    },
                    |_| Ok(policy()),
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
            levels: vec![dir.join("g"), dir.join(".piton")],
        }
    }

    /// A sandbox rerun hint, for the tests' `true`.
    fn sandbox_rerun(asked: bool) -> Option<crate::agent::Rerun> {
        asked.then_some(crate::agent::Rerun {
            runtime: Target::Sandbox,
        })
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

    fn start(calls: &mut Calls, id: &str, command: &str, rerun: bool) {
        start_with(calls, id, command, rerun.then(|| json!({"runtime": "sandbox"})));
    }

    fn request_with(dir: &Path, id: &str, command: &str, rerun: Option<crate::agent::Rerun>) -> RunRequest {
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

    fn request(dir: &Path, id: &str, command: &str, rerun: bool) -> RunRequest {
        request_with(dir, id, command, sandbox_rerun(rerun))
    }

    fn agent() -> question::Asker {
        question::Asker::Agent { reason: None }
    }

    fn refusal(operation: &str, target: &str) -> CallRefusal {
        CallRefusal {
            process: "touch".into(),
            operation: operation.into(),
            target: target.into(),
        }
    }

    /// Starts a plain call and runs it at the session's policy: what a failed call looks like before `after_failure`.
    fn begin(c: &Commands, calls: &mut Calls, dir: &Path, id: &str, command: &str) -> RunRequest {
        start(calls, id, command, false);
        run(c.decide(calls, &request(dir, id, command, false)));
        request(dir, id, command, false)
    }

    /// An `Ask`'s blocks, refusals, tag, and note; panics otherwise.
    fn asked(after: AfterFailure) -> (Vec<String>, Vec<String>, String, Option<String>) {
        match after {
            AfterFailure::Ask {
                blocks,
                refusals,
                tag,
                note,
            } => (blocks, refusals, tag, note),
            AfterFailure::Wait | AfterFailure::Report { .. } => panic!("not asked: {after:?}"),
        }
    }

    /// A `Report`'s blocks and note; panics otherwise.
    fn reported(after: AfterFailure) -> (Vec<String>, Option<String>) {
        match after {
            AfterFailure::Report { blocks, note } => (blocks, note),
            AfterFailure::Wait | AfterFailure::Ask { .. } => panic!("not reported: {after:?}"),
        }
    }

    fn after(c: &Commands, calls: &mut Calls, req: &RunRequest, last: bool) -> AfterFailure {
        c.after_failure(calls, req, last, false)
    }

    /// A `connect` entry saved from a network question reaches calls that start afterwards, at the session's policy and
    /// a grant's; only a session whose configuration reads the file takes it.
    #[test]
    fn a_saved_network_entry_reaches_later_calls() {
        let dir = tempfile::tempdir().unwrap();
        let filtered = |p: &mut crate::launch::sandbox::Policy| {
            p.network = crate::launch::sandbox::Network::Filtered {
                grants: Vec::new(),
                gate: crate::launch::sandbox::Gate::None,
            };
        };
        let mut c = commands(dir.path(), &["vendor *"]);
        filtered(&mut c.policies.launch);
        for (_, runtime) in &mut c.policies.entries {
            if let EntryRuntime::Sandbox(Ok(p)) = runtime {
                filtered(p);
            }
        }
        let to = crate::launch::sandbox::Destination::parse("api.vendor.com:443").unwrap();
        let saved = dir.path().join(".piton/config.local.toml");
        assert!(c.reads(&saved));
        assert!(!c.reads(&dir.path().join("elsewhere/.piton/config.local.toml")));
        // A project without a `.piton/` folder at launch still reads the file a question saves to.
        c.levels.clear();
        assert!(c.reads(&saved), "the project's own file, created by the answer");
        assert!(c.reads(&dir.path().join("g/config.toml")), "the global file");
        c.add_network_entry(
            crate::config::NetworkTarget::Host {
                host: "api.vendor.com".into(),
                port: Some(443),
            },
            crate::launch::sandbox::Source::Workspace,
        );
        let mut calls = Calls::new("t");
        for (id, command) in [("a", "curl x"), ("b", "vendor sync")] {
            start(&mut calls, id, command, false);
            let approved = run(c.decide(&mut calls, &request(dir.path(), id, command, false)));
            let policy = match &approved.runtime {
                Runtime::Session(p) | Runtime::Widened { policy: p, .. } => p,
                Runtime::Host { .. } => panic!("host"),
            };
            assert!(policy.decide_network(&to).allowed, "{command}");
        }
    }

    /// A failed call is asked about only for a block an approved rerun lifts, once; everything else is reported with
    /// why, and a call with nothing recorded waits for the kernel's record until the last look.
    #[test]
    fn a_failed_call_is_asked_about_only_for_a_liftable_block() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let mut c = commands(&dir, &[]);
        let home = dir.join("h");
        // The launch denies writes below the agent directory (floor) and reads of ~/.ssh (a credential); `all` keeps
        // the floor and lifts the rest.
        let deny = |access: crate::launch::sandbox::Access, path: PathBuf, source| crate::launch::sandbox::Rule {
            effect: crate::launch::sandbox::Effect::Deny,
            access: vec![access],
            target: crate::launch::sandbox::Target::Subpath { path },
            source,
        };
        let floor = deny(
            crate::launch::sandbox::Access::Write,
            home.join(".pi"),
            crate::launch::sandbox::Source::Piton {
                what: "agent dir".into(),
            },
        );
        let ssh = deny(
            crate::launch::sandbox::Access::Read,
            home.join(".ssh"),
            crate::launch::sandbox::Source::NonoGroup { group: "ssh".into() },
        );
        c.policies.launch.rules = vec![floor.clone(), ssh];
        c.policies.all.rules = vec![
            crate::launch::sandbox::Rule {
                effect: crate::launch::sandbox::Effect::Allow,
                access: vec![
                    crate::launch::sandbox::Access::Write,
                    crate::launch::sandbox::Access::Read,
                ],
                target: crate::launch::sandbox::Target::Subpath { path: "/".into() },
                source: crate::launch::sandbox::Source::Workspace,
            },
            floor,
        ];
        c.policies.launch.apps = crate::launch::sandbox::Apps::Closed;
        c.policies.all.apps = crate::launch::sandbox::Apps::Open {
            source: crate::launch::sandbox::Source::Workspace,
        };
        let kind = |a: &AfterFailure| match a {
            AfterFailure::Wait => "wait",
            AfterFailure::Report { .. } => "report",
            AfterFailure::Ask { .. } => "ask",
        };
        let mut calls = Calls::new("7-ab");

        // Nothing recorded: wait, then report nothing on the last look.
        let req = begin(&c, &mut calls, &dir, "1", "make");
        assert_eq!(after(&c, &mut calls, &req, false), AfterFailure::Wait);
        assert_eq!(
            after(&c, &mut calls, &req, true),
            AfterFailure::Report {
                blocks: Vec::new(),
                note: None
            }
        );

        // A liftable write: asked, once, tagged as the call.
        let req = begin(&c, &mut calls, &dir, "2", "touch ~/x");
        let target = home.join("x").display().to_string();
        calls.note("2", refusal("file-write-create", &target));
        let (blocks, refusals, tag, note) = asked(after(&c, &mut calls, &req, false));
        assert_eq!(blocks, [format!("file-write-create {target}")]);
        assert_eq!(refusals, blocks);
        assert_eq!(tag, "7-ab-2");
        assert_eq!(note, None);
        assert_eq!(kind(&after(&c, &mut calls, &req, true)), "report", "asked once");

        // The floor and a credential read: reported with why, not asked.
        let req = begin(&c, &mut calls, &dir, "3", "git push");
        calls.note(
            "3",
            refusal("file-write-create", &home.join(".pi/x").display().to_string()),
        );
        calls.note(
            "3",
            refusal("file-read-data", &home.join(".ssh/id").display().to_string()),
        );
        let (blocks, note) = reported(after(&c, &mut calls, &req, false));
        assert_eq!(note, None);
        assert!(blocks[0].ends_with("(protected: no approval lifts it)"), "{blocks:?}");
        assert!(blocks[1].ends_with("(a credential read)"), "{blocks:?}");

        // Silenced: reported, not asked.
        c.policies.silence.push(crate::launch::sandbox::Silenced {
            target: crate::launch::sandbox::Silent::Path(home.join("quiet")),
            process: None,
            source: crate::launch::sandbox::SilenceSource::Preset("test"),
        });
        let req = begin(&c, &mut calls, &dir, "4", "touch ~/quiet/x");
        calls.note(
            "4",
            refusal("file-write-create", &home.join("quiet/x").display().to_string()),
        );
        let (blocks, _) = reported(after(&c, &mut calls, &req, true));
        assert!(blocks[0].ends_with("(silenced)"), "{blocks:?}");

        // Another question showing: reported, saying why.
        let req = begin(&c, &mut calls, &dir, "5", "touch ~/y");
        calls.note("5", refusal("file-write-create", &home.join("y").display().to_string()));
        let (_, note) = reported(c.after_failure(&mut calls, &req, false, true));
        assert!(note.is_some_and(|n| n.contains("another question")));

        // A typed command is never asked about.
        let typed = request(&dir, "user:1", "touch ~/z", false);
        assert_eq!(kind(&after(&c, &mut calls, &typed, true)), "report");

        // Operations only the tagged default reports: a preferences write asks; a signal is floor.
        let req = begin(&c, &mut calls, &dir, "6", "defaults write com.foo k v");
        calls.note("6", refusal("user-preference-write", "com.foo"));
        let (blocks, ..) = asked(after(&c, &mut calls, &req, false));
        assert_eq!(blocks, ["user-preference-write com.foo"]);
        let req = begin(&c, &mut calls, &dir, "7", "kill 1");
        calls.note("7", refusal("signal", "pgrp [bash(9)] signum:15"));
        let (blocks, _) = reported(after(&c, &mut calls, &req, false));
        assert!(blocks[0].ends_with("(protected: no approval lifts it)"), "{blocks:?}");

        // Incidental refusals never ask. Alone, they're listed; beside a real block, left out.
        let req = begin(&c, &mut calls, &dir, "8", "make");
        calls.note("8", refusal("sysctl-write", "kern.grade_cputype"));
        assert_eq!(
            after(&c, &mut calls, &req, false),
            AfterFailure::Wait,
            "an incidental record alone keeps the wait going"
        );
        let (blocks, _) = reported(after(&c, &mut calls, &req, true));
        assert_eq!(blocks, ["sysctl-write kern.grade_cputype (incidental)"]);
        let req = begin(&c, &mut calls, &dir, "9", "touch ~/w");
        calls.note("9", refusal("sysctl-write", "kern.grade_cputype"));
        calls.note("9", refusal("file-write-create", &home.join("w").display().to_string()));
        let (blocks, refusals, ..) = asked(after(&c, &mut calls, &req, false));
        assert_eq!(blocks, [format!("file-write-create {}", home.join("w").display())]);
        assert_eq!(refusals, blocks);
    }

    /// A blocked `open` or `osascript` is asked about from the kernel's tagged record, with what it opens; one that
    /// left no record (it never ran) isn't. A target the check refuses is reported, never asked.
    #[test]
    fn a_blocked_open_is_asked_about_from_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let mut c = commands(&dir, &[]);
        c.policies.launch.apps = crate::launch::sandbox::Apps::Closed;
        c.policies.all.apps = crate::launch::sandbox::Apps::Open {
            source: crate::launch::sandbox::Source::Workspace,
        };
        let mut calls = Calls::new("7-ab");
        let web = "open https://example.com";

        // The kernel's tagged record, after the incidental sysctl write that comes first: waited for, then asked.
        let req = begin(&c, &mut calls, &dir, "1", web);
        calls.note("1", refusal("sysctl-write", "kern.grade_cputype"));
        assert_eq!(after(&c, &mut calls, &req, false), AfterFailure::Wait);
        calls.note("1", refusal("lsopen", ""));
        let (blocks, _, _, note) = asked(after(&c, &mut calls, &req, false));
        assert!(note.is_some_and(|n| n.contains("browser")));
        assert_eq!(blocks, ["lsopen"], "the incidental one is left out");

        // `open` never ran: no record, nothing asked.
        let never = format!("false && {web}");
        let req = begin(&c, &mut calls, &dir, "3", &never);
        assert_eq!(
            after(&c, &mut calls, &req, true),
            AfterFailure::Report {
                blocks: Vec::new(),
                note: None
            }
        );

        // `osascript`: reported with the refusal, never asked.
        let script = "osascript -e 'tell application \"Terminal\" to do script \"x\"'";
        let req = begin(&c, &mut calls, &dir, "4", script);
        calls.note(
            "4",
            refusal(
                "file-issue-extension",
                "target:/System/Applications/Utilities/Terminal.app extension-class:com.apple.app-sandbox.read",
            ),
        );
        calls.note("4", refusal("lsopen", ""));
        calls.note("4", refusal("network-outbound", "/private/var/run/syslog"));
        let (blocks, note) = reported(after(&c, &mut calls, &req, false));
        assert!(note.is_some_and(|n| n.contains("osascript")));
        assert_eq!(blocks.len(), 2, "the incidental syslog connect is left out: {blocks:?}");
        assert!(blocks[0].ends_with("(protected: no approval lifts it)"), "{blocks:?}");
    }

    /// A dev server's refused TCP listen is asked about: an approved rerun opens listening.
    #[test]
    fn a_blocked_listen_is_asked_about() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let mut c = commands(&dir, &[]);
        c.policies.launch.listen = crate::launch::sandbox::Listen::Closed;
        c.policies.all.listen = crate::launch::sandbox::Listen::Open {
            source: crate::launch::sandbox::Source::Workspace,
        };
        let mut calls = Calls::new("7-ab");
        let req = begin(&c, &mut calls, &dir, "1", "npm run dev");
        calls.note("1", refusal("network-inbound", "local:*:50958"));
        let (blocks, _, _, _) = asked(after(&c, &mut calls, &req, false));
        assert_eq!(blocks, ["network-inbound local:*:50958"]);
    }

    /// After the user denies Piton's question, the agent's rerun of the same command in that run is refused; a new
    /// run may ask again.
    #[test]
    fn a_denied_command_isn_t_asked_again_in_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let c = commands(&dir, &[]);
        let mut calls = Calls::new("7-ab");
        start(&mut calls, "1", "touch ~/x", false);
        run(c.decide(&mut calls, &request(&dir, "1", "touch ~/x", false)));
        calls.decline("1");
        calls.track("tool_execution_end", json!({"toolCallId": "1"}).as_object().unwrap());
        start(&mut calls, "2", "touch ~/x", true);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "2", "touch ~/x", true))),
            "refused"
        );
        calls.track("agent_end", &Map::new());
        start(&mut calls, "3", "touch ~/x", true);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "3", "touch ~/x", true))),
            "ask"
        );
    }

    /// The question opens on the side the agent hinted; a rerun must be the one the call asked for, and a `grant` is no
    /// part of a rerun.
    #[test]
    fn the_question_opens_on_the_hinted_side() {
        let dir = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(dir.path()).unwrap();
        let c = commands(&dir, &[]);
        let mut calls = Calls::new("7-ab");
        let host = Some(crate::agent::Rerun { runtime: Target::Host });

        start_with(&mut calls, "1", "probe", Some(json!({"runtime": "host"})));
        let req = request_with(&dir, "1", "probe", host);
        assert_eq!(kind(&c.decide(&mut calls, &req)), "ask");
        assert_eq!(
            c.question("q".into(), &req, agent(), Vec::new(), None).runtime,
            Target::Host
        );
        assert_eq!(
            c.question("q".into(), &request(&dir, "2", "x", true), agent(), Vec::new(), None)
                .runtime,
            Target::Sandbox
        );

        start_with(&mut calls, "3", "probe", Some(json!({"runtime": "sandbox"})));
        assert_eq!(
            kind(&c.decide(&mut calls, &request_with(&dir, "3", "probe", host))),
            "refused",
            "not the rerun the call asked for"
        );
        // A grant isn't part of a rerun: the call's argument isn't read as one, and the request doesn't parse.
        start_with(
            &mut calls,
            "4",
            "probe",
            Some(json!({"runtime": "sandbox", "grant": "all"})),
        );
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "4", "probe", true))),
            "refused"
        );
        assert!(serde_json::from_value::<crate::agent::Rerun>(json!({"runtime": "sandbox", "grant": "all"})).is_err());
        assert!(serde_json::from_value::<crate::agent::Rerun>(json!({"runtime": "vm"})).is_err());
        assert!(serde_json::from_value::<crate::agent::Rerun>(json!({})).is_err());
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
            kind(&c.decide(&mut calls, &request(&dir, "1", "git push", false))),
            "refused",
            "not running"
        );

        start(&mut calls, "1", "git push", false);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "1", "rm -rf ~", false))),
            "refused",
            "other command"
        );
        let granted = run(c.decide(&mut calls, &request(&dir, "1", "git push", false)));
        assert_eq!(granted.command, "set -e\ngit push", "pi's prefix");
        assert_eq!(granted.timeout, Some(Duration::from_secs(5)));
        assert_eq!(sandbox(&granted).tag, "7-ab-1");
        assert!(matches!(granted.runtime, Runtime::Widened { by: Why::Grant(_), .. }));
        assert_eq!(
            granted.env["FROM"], "launch",
            "a widened run gets the launch environment"
        );
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "1", "git push", false))),
            "refused",
            "once per call"
        );

        start(&mut calls, "2", "ls", false);
        let plain = run(c.decide(&mut calls, &request(&dir, "2", "ls", false)));
        assert!(matches!(plain.runtime, Runtime::Session(_)), "the session's own policy");
        assert_eq!(sandbox(&plain).tag, "7-ab-2");
        assert_eq!(sandbox(&plain).rules, c.policies.launch.rules);
        assert_eq!(plain.env["FROM"], "request", "pi's shell environment");
        assert_eq!(calls.owner("7-ab-2"), TagOwner::Call("2".into()));
        assert_eq!(calls.owner("7-ab"), TagOwner::Session, "pi's own sandbox");
        assert_eq!(calls.owner("7-ab-99"), TagOwner::GoneCall, "a call long forgotten");
        assert_eq!(calls.owner("7-abc-2"), TagOwner::Other);

        let typed = run(c.decide(&mut calls, &request(&dir, "user:1", "ls ~", false)));
        assert!(
            matches!(typed.runtime, Runtime::Session(_)),
            "a typed command is never widened"
        );
        assert_eq!(calls.owner(&sandbox(&typed).tag), TagOwner::Typed);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "user:2", "ls", true))),
            "refused"
        );

        let First::Ask { tag, .. } = ({
            start(&mut calls, "9", "husky install", true);
            c.decide(&mut calls, &request(&dir, "9", "husky install", true))
        }) else {
            panic!("a rerun asks");
        };
        let rerun = c.approved_rerun(&request(&dir, "9", "husky install", true), &tag, Target::Sandbox);
        assert!(sandbox(&rerun).keychain, "at `all`");
        assert_eq!(sandbox(&rerun).tag, tag);
        assert_eq!(rerun.env["FROM"], "launch");

        start(&mut calls, "3", "husky install", false);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "3", "husky install", true))),
            "refused",
            "didn't ask"
        );
        start(&mut calls, "4", "husky install", true);
        assert_eq!(
            kind(&c.decide(&mut calls, &request(&dir, "4", "husky install", true))),
            "ask"
        );

        start(&mut calls, "5", "git push", false);
        let outside = request(Path::new("/"), "5", "git push", false);
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

    /// A refusal goes to its call (running or just ended); a rerun shows what the last run of the same command was
    /// refused; a process block is offered once.
    #[test]
    fn refusals_follow_their_call() {
        let mut calls = Calls::new("1-x");
        start(&mut calls, "1", "husky install", false);
        assert!(calls.note("1", refusal("file-write-create", "/w/.git/hooks/pre-commit")));
        assert!(
            calls.note("1", refusal("file-write-create", "/w/.git/hooks/pre-commit")),
            "kept once"
        );
        assert!(!calls.note("9", refusal("x", "")), "unknown call");
        calls.track("tool_execution_end", json!({"toolCallId": "1"}).as_object().unwrap());
        assert!(
            calls.note("1", refusal("file-write-data", "/w/.git/config")),
            "recorded after it ended"
        );
        start(&mut calls, "2", "husky install", true);
        let rerun = calls.get_running("2").unwrap();
        assert_eq!(
            rerun.refusals.iter().map(CallRefusal::summary).collect::<Vec<_>>(),
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
                project: crate::launch::sandbox::grant::Lift::Write("/h/x".into()),
                everywhere: crate::launch::sandbox::grant::Lift::Write("/h/x".into()),
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

    /// Each `rpc` option leads to the same typed answer a `view` client sends, in the runtime the dialog showed.
    #[test]
    fn rpc_answers_map_to_typed_answers() {
        let dir = tempfile::tempdir().unwrap();
        let c = commands(dir.path(), &[]);
        let mut req = request(dir.path(), "1", "husky install", true);
        req.reason = Some("hooks".into());
        let sandbox = c.question(
            "q".into(),
            &req,
            question::Asker::Agent {
                reason: req.reason.clone(),
            },
            Vec::new(),
            None,
        );
        assert_eq!(sandbox.runtime, Target::Sandbox, "starts in the sandbox");
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
            Next::AskAgain | Next::Switch | Next::AskPattern(..) => panic!("not decided"),
        };
        let (s, h) = (labels(&sandbox), labels(&host));
        assert!(matches!(a(&sandbox, Stage::Choice, &s[0]), Next::AskAgain));
        assert!(matches!(a(&sandbox, Stage::Choice, &s[7]), Next::Switch));
        assert_eq!(
            decided(a(&sandbox, Stage::Choice, &s[1])),
            Answer::Once(Target::Sandbox)
        );
        assert_eq!(decided(a(&host, Stage::Choice, &h[1])), Answer::Once(Target::Host));
        assert_eq!(decided(a(&host, Stage::Choice, &h[2])), Answer::Deny);
        assert_eq!(
            decided(a(&host, Stage::Choice, &h[3])),
            Answer::Always {
                target: Target::Host,
                scope: PersistScope::Project,
                pattern: CommandPattern::new("husky install").unwrap(),
            }
        );
        assert!(matches!(
            a(&host, Stage::Choice, &h[6]),
            Next::AskPattern(Target::Host, PersistScope::Global)
        ));
        assert_eq!(
            decided(a(&host, Stage::Pattern(Target::Host, PersistScope::Global), "husky *")),
            Answer::Always {
                target: Target::Host,
                scope: PersistScope::Global,
                pattern: CommandPattern::new("husky *").unwrap(),
            }
        );
        assert_eq!(
            decided(a(&host, Stage::Pattern(Target::Host, PersistScope::Global), "")),
            Answer::Deny
        );
        // The sandbox dialog's labels don't approve a host run, nor the other way round.
        assert_eq!(decided(a(&sandbox, Stage::Choice, &h[1])), Answer::Deny);
    }

    /// An approved rerun runs where the user chose: at `all` with the launch environment, or on the host without the
    /// session's own variables.
    #[test]
    fn an_approved_rerun_runs_where_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = commands(dir.path(), &[]);
        c.env.insert("PITON_RUNTIME".into(), "sandbox".into());
        let req = request(dir.path(), "1", "husky install", true);
        let sandboxed = c.approved_rerun(&req, "t-1", Target::Sandbox);
        match &sandboxed.runtime {
            Runtime::Widened { policy, by } => {
                assert!(policy.keychain, "at `all`");
                assert_eq!(policy.tag, "t-1");
                assert_eq!(*by, Why::Approval);
            }
            Runtime::Session(_) | Runtime::Host { .. } => panic!("not widened"),
        }
        assert_eq!(sandboxed.env["PITON_RUNTIME"], "sandbox");
        let host = c.approved_rerun(&req, "t-1", Target::Host);
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
        start(&mut calls, "1", "log show --last 1m", false);
        let ran = run(c.decide(&mut calls, &request(&dir, "1", "log show --last 1m", false)));
        assert_eq!(
            ran.runtime,
            Runtime::Host {
                by: Why::Grant(CommandPattern::new("log show *").unwrap())
            }
        );
        assert_eq!(ran.env["FROM"], "launch");
        // A redirect gets no entry at all: the session's sandbox.
        start(&mut calls, "2", "log show > ~/.zshrc", false);
        let redirected = run(c.decide(&mut calls, &request(&dir, "2", "log show > ~/.zshrc", false)));
        assert!(matches!(redirected.runtime, Runtime::Session(_)));
    }

    #[test]
    fn persists_to_the_chosen_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = commands(dir.path(), &[]);
        let global = c
            .persist(
                PersistScope::Global,
                CommandPattern::new("husky install").unwrap(),
                Target::Sandbox,
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
                Target::Host,
            )
            .unwrap();
        assert_eq!(project, dir.path().join(".piton/config.local.toml"));
        let text = std::fs::read_to_string(project).unwrap();
        assert!(
            text.contains("bash(git push *)") && text.contains("runtime = \"host\""),
            "{text}"
        );
        assert!(!text.contains("grant"), "{text}");
    }

    /// An Allow sets one key in the checkout's `config.local.toml` (a path quoted in its map, a boolean), beside the
    /// file's other entries; saving it again leaves the file valid and unchanged.
    #[test]
    fn persisting_an_allow_sets_its_key() {
        use crate::launch::sandbox::grant::Lift;
        let dir = tempfile::tempdir().unwrap();
        let c = commands(dir.path(), &[]);
        let file = dir.path().join(".piton/config.local.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[sandbox.filesystem]\n\"~/x\" = \"write\"\n").unwrap();
        for _ in 0..2 {
            for lift in [Lift::TcpListen, Lift::Write("/h/.cache/tool".into())] {
                let saved = c.persist_lift(PersistScope::Project, &lift).unwrap();
                assert_eq!(saved, file);
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let table: toml::Table = toml::from_str(&text).unwrap();
            let sandbox: crate::config::SandboxTable = table["sandbox"].clone().try_into().unwrap();
            assert_eq!(sandbox.allow_tcp_listening, Some(true), "{text}");
            let key = |p: &str| crate::config::PathKey::new(p).unwrap();
            assert_eq!(
                sandbox.filesystem[&key("/h/.cache/tool")],
                crate::config::FileAccess::Write
            );
            assert_eq!(
                sandbox.filesystem[&key("~/x")],
                crate::config::FileAccess::Write,
                "the file's other entries stay"
            );
        }
    }
}
