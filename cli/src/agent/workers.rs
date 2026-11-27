//! Worker operations a session's agent asks for (`create_worker`, `list_workers`, `delete_worker`): the daemon decides
//! from `[workers.permissions]`, asks the user when it says `"ask"`, then runs the shared operation
//! (`daemon/workers.rs`). The agent never approves: answers come only from clients.
//!
//! A session works only on its own project: its source checkout and that source's workers. Only the wait for the
//! user's first answer is tied to the agent's connection; once approved, the operation (and the follow-up questions it
//! needs: a protected-files update, leftovers on delete) runs as its own task to the end, so a session that deletes the
//! worker it runs in, which stops its own pi, still gets the worker deleted.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{
    answer::{Offer, Once},
    block::{CHOOSE_BELOW, PersistScope},
};
use crate::{
    config::{PermissionMode, WorkerPermission},
    daemon::{Daemon, Deleted, WorkerEntry},
    project::{
        CreateRequest, WorkerInfo, WorkerRefusal,
        source::{Blocked, CreateUpdate, DeleteUpdate},
    },
    session::{SessionCmd, SessionKey},
};

/// A worker operation that asks the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerOp {
    Create,
    Delete,
}

impl WorkerOp {
    /// Its key in `[workers.permissions]`.
    pub fn permission(self) -> WorkerPermission {
        match self {
            WorkerOp::Create => WorkerPermission::CreateByAgent,
            WorkerOp::Delete => WorkerPermission::DeleteByAgent,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            WorkerOp::Create => "create",
            WorkerOp::Delete => "delete",
        }
    }
}

/// What a session's agent asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerRequest {
    Create {
        name: String,
        label: Option<String>,
        item_id: Option<String>,
    },
    List,
    Delete {
        path: PathBuf,
    },
}

/// For the rest of this session; nothing saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForSession;

impl Offer for ForSession {
    const OFFERED: bool = true;
}

/// The user's answer to a worker operation: once (one operation the daemon bounds), for this session, always (saves
/// `"allow"` in the chosen file's `[workers.permissions]`), or deny.
pub type WorkerAnswer = super::answer::Answer<Once<()>, ForSession, PersistScope, ()>;

/// Approve or not: a follow-up that has no broader answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Approved,
    Denied,
}

/// The answer to a source update that changes protected files after a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteUpdateAnswer {
    Approve,
    /// Delete, and leave the source as it is.
    Skip,
    Deny,
}

/// A question the session asks for a worker operation, with where its answer goes. Dropping it unanswered closes its
/// reply: the operation doesn't happen.
#[derive(Debug)]
pub enum Asked {
    /// May the agent create or delete this worker?
    Operation {
        op: WorkerOp,
        lines: Vec<String>,
        reply: oneshot::Sender<WorkerAnswer>,
    },
    /// Creating fast-forwards the source, which changes protected files.
    CreateUpdate {
        blocked: Blocked,
        reply: oneshot::Sender<Approval>,
    },
    /// Deleting fast-forwards the source afterwards, which changes protected files.
    DeleteUpdate {
        blocked: Blocked,
        reply: oneshot::Sender<DeleteUpdateAnswer>,
    },
    /// The worker has uncommitted changes or unpushed commits.
    Leftovers {
        path: PathBuf,
        lines: Vec<String>,
        reply: oneshot::Sender<Approval>,
    },
}

/// An option of a worker question's dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// [`CHOOSE_BELOW`]: no answer yet; the question is shown again.
    AskAgain,
    Operation(WorkerAnswerChoice),
    Approval(Approval),
    DeleteUpdate(DeleteUpdateAnswer),
}

/// [`WorkerAnswer`] without its payloads, as an option names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerAnswerChoice {
    Once,
    Session,
    Always(PersistScope),
    Deny,
}

impl Asked {
    /// The options, in order, with what each answers. The first decides nothing, as in every Piton question.
    fn options(&self) -> Vec<(String, Choice)> {
        let ask_again = (CHOOSE_BELOW.to_string(), Choice::AskAgain);
        match self {
            Asked::Operation { .. } => vec![
                ask_again,
                ("Allow once".into(), Choice::Operation(WorkerAnswerChoice::Once)),
                (
                    "Allow for this session".into(),
                    Choice::Operation(WorkerAnswerChoice::Session),
                ),
                (
                    format!("Always allow {}", PersistScope::Project.label()),
                    Choice::Operation(WorkerAnswerChoice::Always(PersistScope::Project)),
                ),
                (
                    format!("Always allow {}", PersistScope::Global.label()),
                    Choice::Operation(WorkerAnswerChoice::Always(PersistScope::Global)),
                ),
                ("Deny".into(), Choice::Operation(WorkerAnswerChoice::Deny)),
            ],
            Asked::CreateUpdate { .. } => vec![
                ask_again,
                (
                    "Write these files and continue".into(),
                    Choice::Approval(Approval::Approved),
                ),
                ("Don't create the worker".into(), Choice::Approval(Approval::Denied)),
            ],
            Asked::DeleteUpdate { .. } => vec![
                ask_again,
                (
                    "Write these files and update the source".into(),
                    Choice::DeleteUpdate(DeleteUpdateAnswer::Approve),
                ),
                (
                    "Delete, and leave the source as it is".into(),
                    Choice::DeleteUpdate(DeleteUpdateAnswer::Skip),
                ),
                (
                    "Don't delete the worker".into(),
                    Choice::DeleteUpdate(DeleteUpdateAnswer::Deny),
                ),
            ],
            Asked::Leftovers { .. } => vec![
                ask_again,
                (
                    "Delete anyway (this work is lost)".into(),
                    Choice::Approval(Approval::Approved),
                ),
                ("Keep the worker".into(), Choice::Approval(Approval::Denied)),
            ],
        }
    }

    fn title(&self) -> String {
        match self {
            Asked::Operation { op, lines, .. } => {
                format!("The agent asks to {} a worker.\n\n{}", op.verb(), lines.join("\n"))
            }
            Asked::CreateUpdate { blocked, .. } | Asked::DeleteUpdate { blocked, .. } => format!(
                "Updating {} to {} ({}) changes protected files, which the sandbox can't write:\n{}\n\nPiton writes \
                 exactly these files from that commit; the rest of the update, Git hooks included, runs in the \
                 sandbox.",
                blocked.checkout.display(),
                blocked.upstream,
                blocked.target,
                blocked
                    .files
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            Asked::Leftovers { path, lines, .. } => {
                format!("{} has work that isn't pushed:\n{}", path.display(), lines.join("\n"))
            }
        }
    }

    /// The question as pi's dialog record.
    pub fn dialog(&self, id: &str) -> Value {
        let options: Vec<String> = self.options().into_iter().map(|(label, _)| label).collect();
        json!({
            "type": "extension_ui_request",
            "id": id,
            "method": "select",
            "title": self.title(),
            "options": options,
        })
    }

    /// Whether the asker stopped waiting.
    pub fn abandoned(&self) -> bool {
        match self {
            Asked::Operation { reply, .. } => reply.is_closed(),
            Asked::CreateUpdate { reply, .. } | Asked::Leftovers { reply, .. } => reply.is_closed(),
            Asked::DeleteUpdate { reply, .. } => reply.is_closed(),
        }
    }

    /// The operation this question asks about, when it's the first question of one.
    pub fn operation(&self) -> Option<WorkerOp> {
        match self {
            Asked::Operation { op, .. } => Some(*op),
            Asked::CreateUpdate { .. } | Asked::DeleteUpdate { .. } | Asked::Leftovers { .. } => None,
        }
    }

    /// Answers "for this session" without asking: the user already did, earlier in this session.
    pub fn approve_for_session(self) {
        match self {
            Asked::Operation { reply, .. } => {
                let _ = reply.send(WorkerAnswer::Session(ForSession));
            }
            Asked::CreateUpdate { .. } | Asked::DeleteUpdate { .. } | Asked::Leftovers { .. } => {}
        }
    }

    /// A client's `extension_ui_response`: sends the answer, or returns the question to show again. A dismissal or an
    /// unknown option answers no.
    pub fn answer(self, answer: &serde_json::Map<String, Value>) -> Answered {
        let value = answer.get("value").and_then(Value::as_str);
        let choice = value.and_then(|v| self.options().into_iter().find(|(label, _)| label == v).map(|(_, c)| c));
        match (self, choice) {
            (asked, Some(Choice::AskAgain)) => Answered::AskAgain(asked),
            (Asked::Operation { op, reply, .. }, Some(Choice::Operation(choice))) => {
                let (answer, session) = match choice {
                    WorkerAnswerChoice::Once => (WorkerAnswer::Narrowest(Once(())), None),
                    WorkerAnswerChoice::Session => (WorkerAnswer::Session(ForSession), Some(op)),
                    WorkerAnswerChoice::Always(scope) => (WorkerAnswer::Always(scope), None),
                    WorkerAnswerChoice::Deny => (WorkerAnswer::Deny(()), None),
                };
                let _ = reply.send(answer);
                Answered::Done { for_session: session }
            }
            (Asked::CreateUpdate { reply, .. } | Asked::Leftovers { reply, .. }, Some(Choice::Approval(a))) => {
                let _ = reply.send(a);
                Answered::Done { for_session: None }
            }
            (Asked::DeleteUpdate { reply, .. }, Some(Choice::DeleteUpdate(a))) => {
                let _ = reply.send(a);
                Answered::Done { for_session: None }
            }
            (Asked::Operation { reply, .. }, _) => {
                let _ = reply.send(WorkerAnswer::Deny(()));
                Answered::Done { for_session: None }
            }
            (Asked::CreateUpdate { reply, .. } | Asked::Leftovers { reply, .. }, _) => {
                let _ = reply.send(Approval::Denied);
                Answered::Done { for_session: None }
            }
            (Asked::DeleteUpdate { reply, .. }, _) => {
                let _ = reply.send(DeleteUpdateAnswer::Deny);
                Answered::Done { for_session: None }
            }
        }
    }
}

/// What a client's answer did.
#[derive(Debug)]
pub enum Answered {
    /// No answer yet: show the question again.
    AskAgain(Asked),
    /// Answered; `for_session` is the operation the user allowed for the rest of the session.
    Done { for_session: Option<WorkerOp> },
}

/// What the agent gets back, one line.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerResponse {
    WorkerCreated {
        worker: WorkerInfo,
    },
    Workers {
        workers: Vec<WorkerEntry>,
    },
    WorkerDeleted {
        stopped: usize,
        warning: Option<String>,
    },
    /// The operation was refused for a reason the agent can tell the user (`WorkerRefusal`).
    WorkerRefused {
        refusal: WorkerRefusal,
    },
    /// Not done: the user denied it or didn't answer, or a setting turns it off.
    NotDone {
        reason: String,
    },
    /// The session ended.
    Ended,
}

fn not_done(reason: impl Into<String>) -> WorkerResponse {
    WorkerResponse::NotDone { reason: reason.into() }
}

/// Answers `request` from `session`. Dropping the future before the user's first answer (the agent stopped waiting)
/// withdraws the question; after it, the operation runs to the end on its own.
pub async fn serve(daemon: std::sync::Arc<Daemon>, session: SessionKey, request: WorkerRequest) -> WorkerResponse {
    let Some(handle) = daemon.session(session) else {
        return WorkerResponse::Ended;
    };
    let workspace = handle.snapshot().workspace;
    let source = match tokio::task::spawn_blocking({
        let workspace = workspace.clone();
        move || crate::project::source_of(&workspace)
    })
    .await
    {
        Ok(Ok(source)) => source,
        Ok(Err(refusal)) => return WorkerResponse::WorkerRefused { refusal },
        Err(e) => return not_done(e.to_string()),
    };
    match request {
        WorkerRequest::List => match daemon.list_workers(source).await {
            Ok(workers) => WorkerResponse::Workers { workers },
            Err(refusal) => WorkerResponse::WorkerRefused { refusal },
        },
        WorkerRequest::Create { name, label, item_id } => {
            let lines = vec![
                format!("Source: {}", source.display()),
                format!("Worker: {}", crate::project::worker_path(&source, &name).display()),
                format!("Label: {}", label.as_deref().unwrap_or(&name)),
                format!("Item: {}", item_id.as_deref().unwrap_or("none")),
            ];
            if let Some(response) = approve(&daemon, session, &source, WorkerOp::Create, lines).await {
                return response;
            }
            let request = CreateRequest {
                source: source.clone(),
                name,
                label,
                item_id,
                update: CreateUpdate::Ask,
            };
            detached(create(daemon.clone(), session, request)).await
        }
        WorkerRequest::Delete { path } => {
            let worker = match tokio::task::spawn_blocking(move || crate::project::worker_info(&path)).await {
                Ok(Ok(worker)) => worker,
                Ok(Err(refusal)) => return WorkerResponse::WorkerRefused { refusal },
                Err(e) => return not_done(e.to_string()),
            };
            if worker.source != source {
                return WorkerResponse::WorkerRefused {
                    refusal: WorkerRefusal::OutsideProject {
                        path: worker.path,
                        project: source,
                    },
                };
            }
            let lines = delete_lines(&daemon, &worker, &workspace);
            if let Some(response) = approve(&daemon, session, &source, WorkerOp::Delete, lines).await {
                return response;
            }
            detached(delete(daemon.clone(), session, source, worker.path)).await
        }
    }
}

/// What the delete question shows: the worker, its live sessions (they stop), and whether the asking session ends.
fn delete_lines(daemon: &Daemon, worker: &WorkerInfo, workspace: &Path) -> Vec<String> {
    let mut lines = vec![format!("Worker: {} ({})", worker.path.display(), worker.label)];
    let sessions = daemon.sessions_in(&worker.path);
    if !sessions.is_empty() {
        lines.push(format!("Its {} live session(s) stop.", sessions.len()));
    }
    if workspace.starts_with(&worker.path) {
        lines.push("This session runs in it and ends when it's deleted.".into());
    }
    lines
}

/// Runs `work` as its own task, so it finishes even when the agent's connection closes; waits for its answer.
async fn detached(work: impl std::future::Future<Output = WorkerResponse> + Send + 'static) -> WorkerResponse {
    tokio::spawn(work)
        .await
        .unwrap_or_else(|e| not_done(format!("the operation failed: {e}")))
}

/// Decides `op` from `[workers.permissions]`, asking the user on `"ask"`: `None` when it may run, otherwise the answer.
async fn approve(
    daemon: &Daemon,
    session: SessionKey,
    source: &Path,
    op: WorkerOp,
    lines: Vec<String>,
) -> Option<WorkerResponse> {
    let paths = daemon.config().paths.clone();
    let filter = daemon.source_filter(source);
    let permissions = match tokio::task::spawn_blocking({
        let source = source.to_path_buf();
        move || crate::project::worker_permissions(&source, &paths, &filter)
    })
    .await
    {
        Ok(Ok(permissions)) => permissions,
        Ok(Err(refusal)) => return Some(WorkerResponse::WorkerRefused { refusal }),
        Err(e) => return Some(not_done(e.to_string())),
    };
    let key = op.permission();
    match permissions.get(key) {
        PermissionMode::Allow => return None,
        PermissionMode::Deny => {
            return Some(not_done(format!(
                "off: `{} = \"deny\"` in [workers.permissions]; don't ask again",
                key.key()
            )));
        }
        PermissionMode::Ask => {}
    }
    let (reply, answer) = oneshot::channel();
    let answer = match ask(daemon, session, Asked::Operation { op, lines, reply }, answer).await {
        Ok(answer) => answer,
        Err(response) => return Some(response),
    };
    match answer {
        WorkerAnswer::Narrowest(Once(())) | WorkerAnswer::Session(ForSession) => None,
        WorkerAnswer::Always(scope) => {
            match persist(&daemon.config().paths, source, scope, key) {
                Ok(file) => crate::log!(
                    "session {session}: saved `{} = \"allow\"` in {}",
                    key.path(),
                    file.display()
                ),
                Err(e) => crate::log!("session {session}: couldn't save `{} = \"allow\"`: {e}", key.path()),
            }
            None
        }
        WorkerAnswer::Deny(()) => Some(not_done(format!(
            "the user denied it; don't {} this worker again unless they ask",
            op.verb()
        ))),
    }
}

/// Asks the session's user: the answer, or what the agent hears when there's none.
async fn ask<T>(
    daemon: &Daemon,
    session: SessionKey,
    asked: Asked,
    answer: oneshot::Receiver<T>,
) -> Result<T, WorkerResponse> {
    let Some(handle) = daemon.session(session) else {
        return Err(WorkerResponse::Ended);
    };
    // Waits its turn in the session's questions.
    if handle.tx.send(SessionCmd::WorkerQuestion { asked }).is_err() {
        return Err(WorkerResponse::Ended);
    }
    // Wakes the session when the asker stops waiting, so it drops the question or closes its dialog at once.
    let _wake = super::socket::WakeOnDrop(handle.tx.clone());
    answer
        .await
        .map_err(|_| not_done("the user didn't answer (dismissed, timed out, or the run ended)"))
}

/// Saves `<key> = "allow"` in `scope`'s file: this project's `.piton/config.local.toml`, or the global `config.toml`.
fn persist(
    paths: &crate::config::Paths,
    source: &Path,
    scope: PersistScope,
    key: WorkerPermission,
) -> Result<PathBuf, String> {
    use crate::config::{ConfigFile, KeyChange, KeyPath, Sharing, WriteLevel};
    let file = match scope {
        PersistScope::Global => ConfigFile {
            level: WriteLevel::Global,
            sharing: Sharing::Shared,
        },
        PersistScope::Project => ConfigFile {
            level: WriteLevel::Repository {
                source: source.to_path_buf(),
            },
            sharing: Sharing::Local,
        },
    };
    let path = file.path(paths).map_err(|e| e.to_string())?;
    file.write(
        paths,
        &[KeyChange {
            key: KeyPath::new(vec!["workers".into(), "permissions".into(), key.key().into()]),
            value: Some(json!("allow")),
        }],
    )
    .map_err(|e| e.to_string())?;
    Ok(path)
}

/// Creates the worker, asking about a protected-files update when the setting says `"ask"`.
async fn create(daemon: std::sync::Arc<Daemon>, session: SessionKey, mut request: CreateRequest) -> WorkerResponse {
    loop {
        match daemon.create_worker(request.clone()).await {
            Ok(worker) => {
                crate::log!("session {session}: created worker {}", worker.path.display());
                return WorkerResponse::WorkerCreated { worker };
            }
            Err(WorkerRefusal::ProtectedFilesUpdate(blocked)) if matches!(request.update, CreateUpdate::Ask) => {
                let target = blocked.target.clone();
                let (reply, answer) = oneshot::channel();
                match ask(&daemon, session, Asked::CreateUpdate { blocked, reply }, answer).await {
                    Ok(Approval::Approved) => request.update = CreateUpdate::Approve { target },
                    Ok(Approval::Denied) => return not_done("the user declined the source update; nothing changed"),
                    Err(response) => return response,
                }
            }
            Err(refusal) => return WorkerResponse::WorkerRefused { refusal },
        }
    }
}

/// Deletes the worker, asking about leftovers ("Delete anyway", once) and a protected-files update.
async fn delete(daemon: std::sync::Arc<Daemon>, session: SessionKey, source: PathBuf, path: PathBuf) -> WorkerResponse {
    let mut force = false;
    let mut update = DeleteUpdate::Ask;
    loop {
        match daemon.delete_worker(path.clone(), force, update.clone()).await {
            Ok(Deleted { stopped, warning }) => {
                crate::log!(
                    "session {session}: deleted worker {} (in {})",
                    path.display(),
                    source.display()
                );
                return WorkerResponse::WorkerDeleted { stopped, warning };
            }
            Err(WorkerRefusal::Leftovers { path: at, lines }) if !force => {
                let (reply, answer) = oneshot::channel();
                match ask(&daemon, session, Asked::Leftovers { path: at, lines, reply }, answer).await {
                    Ok(Approval::Approved) => force = true,
                    Ok(Approval::Denied) => return not_done("the user kept the worker"),
                    Err(response) => return response,
                }
            }
            Err(WorkerRefusal::ProtectedFilesUpdate(blocked)) if matches!(update, DeleteUpdate::Ask) => {
                let target = blocked.target.clone();
                let (reply, answer) = oneshot::channel();
                match ask(&daemon, session, Asked::DeleteUpdate { blocked, reply }, answer).await {
                    Ok(DeleteUpdateAnswer::Approve) => update = DeleteUpdate::Approve { target },
                    Ok(DeleteUpdateAnswer::Skip) => update = DeleteUpdate::Skip,
                    Ok(DeleteUpdateAnswer::Deny) => return not_done("the user kept the worker"),
                    Err(response) => return response,
                }
            }
            Err(refusal) => return WorkerResponse::WorkerRefused { refusal },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(value: &str) -> serde_json::Map<String, Value> {
        json!({ "value": value }).as_object().unwrap().clone()
    }

    fn operation() -> (Asked, oneshot::Receiver<WorkerAnswer>) {
        let (reply, rx) = oneshot::channel();
        (
            Asked::Operation {
                op: WorkerOp::Create,
                lines: vec!["Worker: /p/r--x".into()],
                reply,
            },
            rx,
        )
    }

    /// Each option answers what it says; the first asks again; a dismissal denies.
    #[test]
    fn options_answer_what_they_say() {
        let cases = [
            ("Allow once", WorkerAnswer::Narrowest(Once(()))),
            ("Allow for this session", WorkerAnswer::Session(ForSession)),
            (
                "Always allow in this project",
                WorkerAnswer::Always(PersistScope::Project),
            ),
            ("Always allow everywhere", WorkerAnswer::Always(PersistScope::Global)),
            ("Deny", WorkerAnswer::Deny(())),
        ];
        for (label, expected) in cases {
            let (asked, mut rx) = operation();
            let for_session = match asked.answer(&answered(label)) {
                Answered::Done { for_session } => for_session,
                Answered::AskAgain(_) => panic!("{label} asked again"),
            };
            assert_eq!(
                for_session,
                matches!(expected, WorkerAnswer::Session(_)).then_some(WorkerOp::Create),
                "{label}"
            );
            assert_eq!(rx.try_recv().unwrap(), expected, "{label}");
        }
        let (asked, _rx) = operation();
        assert!(matches!(asked.answer(&answered(CHOOSE_BELOW)), Answered::AskAgain(_)));
        let (asked, mut rx) = operation();
        asked.answer(&json!({"cancelled": true}).as_object().unwrap().clone());
        assert_eq!(rx.try_recv().unwrap(), WorkerAnswer::Deny(()));
    }

    #[test]
    fn the_answer_type_offers_every_shared_answer() {
        use super::super::answer::{Narrowest, Offered};
        assert_eq!(
            WorkerAnswer::OFFERED,
            Offered {
                narrowest: Narrowest::Once,
                session: true,
                always: true
            }
        );
    }

    /// Leftovers offer "Delete anyway" or keep; a delete's update offers skipping it.
    #[test]
    fn follow_ups_offer_their_own_answers() {
        let (reply, mut rx) = oneshot::channel();
        let asked = Asked::Leftovers {
            path: "/p/r--x".into(),
            lines: vec!["Unpushed commits:".into()],
            reply,
        };
        let dialog = asked.dialog("piton:worker:1");
        assert_eq!(dialog["method"], "select");
        assert!(dialog["title"].as_str().unwrap().contains("Unpushed commits:"));
        asked.answer(&answered("Delete anyway (this work is lost)"));
        assert_eq!(rx.try_recv().unwrap(), Approval::Approved);

        let blocked = Blocked {
            checkout: "/p/r".into(),
            upstream: "origin/main".into(),
            target: crate::project::source::Sha::try_from("a".repeat(40)).unwrap(),
            files: vec![".piton/config.toml".into()],
        };
        let (reply, mut rx) = oneshot::channel();
        let asked = Asked::DeleteUpdate { blocked, reply };
        assert!(
            asked.dialog("i")["title"]
                .as_str()
                .unwrap()
                .contains("- .piton/config.toml")
        );
        asked.answer(&answered("Delete, and leave the source as it is"));
        assert_eq!(rx.try_recv().unwrap(), DeleteUpdateAnswer::Skip);
    }

    /// "Always" saves `"allow"` in the chosen file's `[workers.permissions]`.
    #[test]
    fn always_saves_allow() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let source = root.join("repo");
        std::fs::create_dir_all(source.join(".piton")).unwrap();
        let paths = crate::config::Paths {
            home: root.clone(),
            global: root.join(".config/piton"),
        };
        let file = persist(&paths, &source, PersistScope::Project, WorkerPermission::CreateByAgent).unwrap();
        assert_eq!(file, source.join(".piton/config.local.toml"));
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["workers"]["permissions"]["create_by_agent"].as_str(),
            Some("allow")
        );
    }
}
