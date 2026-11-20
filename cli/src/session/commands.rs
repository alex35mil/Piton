//! A session's commands that run through the daemon: which bash calls pi is running (from its own events), what a
//! request to run one through the daemon gets, and the block question while the user decides.
//!
//! A request over `agent.sock` is only a request. The daemon accepts one for a bash call pi reported running
//! (`tool_execution_start`) with exactly that command, once per call; a rerun only when that call's arguments asked for
//! it (`outside_sandbox: true`). It decides the policy (a per-command grant it matched itself, or `all` once the user
//! approves), the shell and its prefix (pi's settings files), and the environment (the session's launch environment).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Map, Value};
use tokio::sync::oneshot;

use crate::{
    agent::{
        Approved, Matched, PiShell, RunDecision, RunRequest,
        block::{self, Answer, PersistScope},
        grant_for,
    },
    config::{AllGrant, CommandGrant, CommandPattern, ConfigFile, Grant, Paths, Sharing, WriteLevel},
    launch::{Env, sandbox::grant::CommandPolicies},
};

/// How long the block question waits for an answer.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A bash call pi is running.
#[derive(Debug, Clone)]
pub struct BashCall {
    pub command: String,
    /// The agent asked for this call to run outside the sandbox.
    pub outside_sandbox: bool,
    /// A request was taken for it: one per call.
    pub used: bool,
    /// The kernel's refusals routed to this call (`operation path`).
    pub refusals: Vec<String>,
    /// The call's number in its sandbox's tag.
    pub number: u64,
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
    pub id: String,
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
    pub lift: crate::launch::sandbox::grant::Lift,
}

/// How many process questions wait behind the one showing.
const PROCESS_WAITING: usize = 8;

impl ProcessBlocks {
    pub fn asking(&self, id: &str) -> bool {
        self.asking.as_ref().is_some_and(|q| q.id == id)
    }

    /// Whether this refusal is new: each (process, operation, target) is offered once per session.
    pub fn first(&mut self, process: &str, operation: &str, target: &str) -> bool {
        self.offered
            .insert((process.to_string(), operation.to_string(), target.to_string()))
    }

    pub fn queue(&mut self, question: ProcessQuestion) {
        if self.waiting.len() < PROCESS_WAITING {
            self.waiting.push_back(question);
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
        let file = self.file(scope);
        let path = file.path(&self.paths).map_err(|e| e.to_string())?;
        let (key, value) = lift.entry();
        file.append_to_list(&self.paths, &format!("sandbox.{key}"), &value)
            .map_err(|e| e.to_string())?;
        Ok(path)
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

/// Which dialog is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Choice,
    Pattern(PersistScope),
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
                let outside_sandbox = args
                    .and_then(|a| a.get("outside_sandbox"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // A rerun shows what the last run of the same command was refused.
                let refusals = match outside_sandbox {
                    true => self
                        .ended
                        .iter()
                        .rev()
                        .find(|(_, call)| call.command == command)
                        .map(|(_, call)| call.refusals.clone())
                        .unwrap_or_default(),
                    false => Vec::new(),
                };
                self.next += 1;
                let number = self.next;
                self.calls.insert(
                    id,
                    BashCall {
                        command: command.to_string(),
                        outside_sandbox,
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

    /// A command the user typed: a number of its own, kept like a call's so its refusals are noted (and not offered).
    fn number_typed(&mut self, id: &str) -> String {
        self.next += 1;
        let number = self.next;
        self.end(
            id.to_string(),
            BashCall {
                command: String::new(),
                outside_sandbox: false,
                used: true,
                refusals: Vec::new(),
                number,
            },
        );
        format!("{}-{number}", self.tag_prefix)
    }

    /// The call a sandbox tag names (`<session tag>-<number>`), running or recently ended. `None` for the session's
    /// own tag (pi's sandbox) or another session's.
    pub fn by_tag(&self, tag: &str) -> Option<String> {
        let number: u64 = tag.strip_prefix(&self.tag_prefix)?.strip_prefix('-')?.parse().ok()?;
        self.calls
            .iter()
            .chain(self.ended.iter().map(|(id, call)| (id, call)))
            .find(|(_, call)| call.number == number)
            .map(|(id, _)| id.clone())
    }
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

/// Which environment a run gets.
enum RunEnv {
    /// At the session's own policy: the extension's (pi's shell environment), which grants nothing new.
    Request,
    /// Widened: the session's launch environment, so a request can't point a granted command at its own `PATH`.
    Launch,
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
            if request.rerun {
                return not_run("a command you type runs in the sandbox".into());
            }
            let tag = calls.number_typed(&request.call_id);
            return self.run(request, &self.policies.launch, &tag, RunEnv::Request);
        }
        let (call, tag) = match calls.take(request) {
            Ok(taken) => taken,
            Err(reason) => return not_run(reason),
        };
        match request.rerun {
            true => match call.outside_sandbox {
                true => {
                    use crate::agent::open::{self, OpenCheck};
                    let refusals = call.refusals.clone();
                    let nodes = match crate::agent::analyze(&request.command, Some(&request.cwd)) {
                        Ok(analysis) => analysis.commands,
                        Err(e) => return not_run(e.to_string()),
                    };
                    match open::check(&nodes, &request.cwd, &self.policies.launch, open::default_app) {
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
                false => not_run("the call didn't ask to run outside the sandbox (outside_sandbox: true)".into()),
            },
            false => match grant_for(&self.grants, &request.command, &request.cwd) {
                Ok(Matched::Grant(entry)) => match self.policies.of(entry) {
                    Ok(policy) => self.run(request, policy, &tag, RunEnv::Launch),
                    Err(reason) => not_run(format!(
                        "the grant for `{}` can't be used: {reason}",
                        entry.value.pattern.pattern()
                    )),
                },
                Ok(Matched::None | Matched::Redirect) | Err(_) => {
                    self.run(request, &self.policies.launch, &tag, RunEnv::Request)
                }
            },
        }
    }

    fn run(&self, request: &RunRequest, policy: &crate::launch::sandbox::Policy, tag: &str, env: RunEnv) -> First {
        First::Decided(RunDecision::Run(Box::new(self.approved(request, policy, tag, env))))
    }

    /// A rerun the user approved: at `all`, with the launch environment.
    pub fn approved_rerun(&self, request: &RunRequest, tag: &str) -> Approved {
        self.approved(request, &self.policies.all, tag, RunEnv::Launch)
    }

    /// The command as the daemon runs it: its policy tagged with the call, the shell and prefix from pi's settings.
    fn approved(
        &self,
        request: &RunRequest,
        policy: &crate::launch::sandbox::Policy,
        tag: &str,
        env: RunEnv,
    ) -> Approved {
        let mut policy = policy.clone();
        policy.tag = tag.to_string();
        Approved {
            policy,
            shell: self.shell.shell.clone(),
            command: self.shell.command(&request.command),
            cwd: request.cwd.clone(),
            env: match env {
                RunEnv::Request => request.env.clone(),
                RunEnv::Launch => self.env.clone(),
            },
            timeout: request
                .timeout
                .filter(|t| t.is_finite() && *t > 0.0)
                .map(Duration::from_secs_f64),
        }
    }

    /// Saves a per-command grant for `pattern` with `grant = "all"`.
    pub fn persist(&self, scope: PersistScope, pattern: CommandPattern) -> Result<PathBuf, String> {
        let file = self.file(scope);
        let grant = CommandGrant {
            pattern,
            grant: Grant::All(AllGrant::All),
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

/// What the user's answer to a dialog leads to.
pub enum Next {
    /// Ask the pattern dialog.
    AskPattern(PersistScope),
    /// Run at `all` (after persisting, when `persist` says so).
    Run {
        persist: Option<(PersistScope, CommandPattern)>,
    },
    Deny,
}

/// Reads an answer to the dialog at `stage`.
pub fn answer(stage: Stage, command: &str, answer: &Map<String, Value>) -> Next {
    match stage {
        Stage::Choice => match block::read_answer(answer) {
            Answer::Once => Next::Run { persist: None },
            Answer::Deny => Next::Deny,
            Answer::Persist(scope) => match CommandPattern::new(command) {
                Ok(pattern) => Next::Run {
                    persist: Some((scope, pattern)),
                },
                // A multi-line command can't be a pattern: run it once.
                Err(_) => Next::Run { persist: None },
            },
            Answer::Pattern(scope) => Next::AskPattern(scope),
        },
        Stage::Pattern(scope) => match block::read_pattern(answer) {
            Some(pattern) => Next::Run {
                persist: Some((scope, pattern)),
            },
            None => Next::Deny,
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
                    grant: Grant::All(AllGrant::All),
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
                credentials: crate::launch::sandbox::grant::Credentials::Liftable,
                entries: grants.iter().map(|g| (g.clone(), Ok(policy()))).collect(),
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

    fn start(calls: &mut Calls, id: &str, command: &str, outside: bool) {
        let record =
            json!({"toolCallId": id, "toolName": "bash", "args": {"command": command, "outside_sandbox": outside}});
        calls.track("tool_execution_start", record.as_object().unwrap());
    }

    fn request(dir: &Path, id: &str, command: &str, rerun: bool) -> RunRequest {
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

    fn kind(first: &First) -> &'static str {
        match first {
            First::Decided(RunDecision::Run(_)) => "run",
            First::Decided(RunDecision::NotRun { .. }) => "refused",
            First::Ask { .. } => "ask",
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
        assert_eq!(granted.policy.tag, "7-ab-1");
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
        assert_eq!(plain.policy.tag, "7-ab-2");
        assert_eq!(plain.policy.rules, c.policies.launch.rules, "the session's own policy");
        assert_eq!(plain.env["FROM"], "request", "pi's shell environment");
        assert_eq!(calls.by_tag("7-ab-2").as_deref(), Some("2"));
        assert_eq!(calls.by_tag("7-ab"), None, "pi's own sandbox");
        assert_eq!(calls.by_tag("7-abc-2"), None);

        let typed = run(c.decide(&mut calls, &request(&dir, "user:1", "ls ~", false)));
        assert_eq!(
            typed.policy.rules, c.policies.launch.rules,
            "a typed command is never widened"
        );
        assert_eq!(calls.by_tag(&typed.policy.tag).as_deref(), Some("user:1"));
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
        let rerun = c.approved_rerun(&request(&dir, "9", "husky install", true), &tag);
        assert!(rerun.policy.keychain, "at `all`");
        assert_eq!(rerun.policy.tag, tag);
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
        start(&mut calls, "2", "husky install", true);
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
            lift: crate::launch::sandbox::grant::Lift::WriteFile("/h/x".into()),
        };
        blocks.queue(question("a"));
        blocks.queue(question("b"));
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("a".into()));
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("a".into()), "one at a time");
        assert!(blocks.asking("a"));
        blocks.asking = None;
        assert_eq!(blocks.next().map(|q| q.id.clone()), Some("b".into()));
    }

    #[test]
    fn answers_lead_somewhere() {
        let a = |stage, value: &str| answer(stage, "husky install", json!({"value": value}).as_object().unwrap());
        let labels: Vec<String> = block::dialog("x", &block::Question::default())["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(matches!(a(Stage::Choice, &labels[0]), Next::Run { persist: None }));
        assert!(matches!(a(Stage::Choice, &labels[1]), Next::Deny));
        assert!(matches!(
            a(Stage::Choice, &labels[2]),
            Next::Run { persist: Some((PersistScope::Project, ref p)) } if p.pattern() == "husky install"
        ));
        assert!(matches!(
            a(Stage::Choice, &labels[5]),
            Next::AskPattern(PersistScope::Global)
        ));
        assert!(matches!(
            a(Stage::Pattern(PersistScope::Global), "husky *"),
            Next::Run { persist: Some((PersistScope::Global, ref p)) } if p.pattern() == "husky *"
        ));
        assert!(matches!(a(Stage::Pattern(PersistScope::Global), ""), Next::Deny));
    }

    #[test]
    fn persists_to_the_chosen_file() {
        let dir = tempfile::tempdir().unwrap();
        let c = commands(dir.path(), &[]);
        let global = c
            .persist(PersistScope::Global, CommandPattern::new("husky install").unwrap())
            .unwrap();
        assert_eq!(global, dir.path().join("g/config.toml"));
        let project = c
            .persist(PersistScope::Project, CommandPattern::new("git push *").unwrap())
            .unwrap();
        assert_eq!(project, dir.path().join(".piton/config.local.toml"));
        assert!(std::fs::read_to_string(project).unwrap().contains("bash(git push *)"));
    }
}
