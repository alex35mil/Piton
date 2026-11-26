//! The session's questions (#416): one scheduler for every question the daemon asks the user, whatever its kind.
//!
//! One question shows at a time, the rest wait in arrival order. A question waits while pi has a dialog of its own
//! pending (a file review, an extension's prompt), so the daemon never opens one on top of it. Nothing is refused for
//! a busy slot: a waiting question whose asker stopped waiting is dropped, and the showing one ends then.
//!
//! ```text
//!   enqueue ──▶ waiting ──next (slot free, no pi dialog)──▶ showing ──answered──▶ (gone)
//!                  ▲  └──asker left──▶ (dropped)              │  └──reshown (ask again, next stage)──▶ showing
//!                  └──────── its dialog left with the run (process, network) ◀┘
//!                            asker left, timed out, run ended (the rest) ──▶ (gone, told)
//! ```
//!
//! Each kind supplies its dialog (rendered by the session, which holds what the dialog names), how its answer is read
//! (also the session's), its [`Timeout`], what it does when it ends unanswered ([`Question::unanswered`]), and whether
//! its asker stopped waiting ([`Question::abandoned`]).

use std::{collections::VecDeque, path::PathBuf};

use tokio::{sync::oneshot, time::Instant};

use super::{commands, network};
use crate::{agent, launch::sandbox::grant::LiveLift};

/// A question for the user, of one of the kinds the session asks.
pub enum Question {
    /// A protected write the agent asked for (`agent.sock` `protected_write`).
    ProtectedWrite(PendingWrite),
    /// The block question for a bash call: the agent's rerun request, or Piton's own after the call failed on a block.
    Block(commands::PendingQuestion),
    /// A blocked process that isn't a bash call (an MCP server, an extension, pi).
    Process(commands::ProcessQuestion),
    /// A connection the gate holds, with every connection waiting on the same destination.
    Network(network::NetworkQuestion),
    /// A worker operation's question.
    Worker(agent::workers::Asked),
    /// An explicit lift from code in pi's process.
    Lift(PendingLift),
}

/// A protected write waiting for the user, and where the outcome goes.
pub struct PendingWrite {
    /// The folder the write starts from (the checkout, a configuration level, pi's agent directory, a linked file's
    /// folder): it walks from here without following symlinks.
    pub root: PathBuf,
    pub path: PathBuf,
    pub controls: agent::Controls,
    pub content: String,
    pub reply: oneshot::Sender<agent::WriteOutcome>,
}

/// An explicit lift waiting for the user.
pub struct PendingLift {
    pub targets: Vec<LiveLift>,
    pub reason: String,
    pub reply: oneshot::Sender<agent::lift::LiftDecision>,
}

/// Whether a kind's question times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// It pauses an action: `approval_timeout` after it's shown, the action doesn't happen.
    Approval,
    /// Nothing waits on it: it shows until it's answered.
    Never,
}

/// Why a question ended without an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanswered {
    /// Its dialog left with the run it showed in (pi's `agent_settled`): no client shows it any more.
    RunEnded,
    /// No answer within its deadline.
    TimedOut,
    /// Whoever asked stopped waiting (its connection closed).
    AskerLeft,
}

impl Question {
    /// The kind's name, in dialog ids (`piton:<kind>:<n>`) and the daemon log.
    pub fn kind(&self) -> &'static str {
        match self {
            Question::ProtectedWrite(_) => "write",
            Question::Block(_) => "block",
            Question::Process(_) => "process",
            Question::Network(_) => "network",
            Question::Worker(_) => "worker",
            Question::Lift(_) => "lift",
        }
    }

    pub fn timeout(&self) -> Timeout {
        match self {
            Question::ProtectedWrite(_)
            | Question::Block(_)
            | Question::Network(_)
            | Question::Worker(_)
            | Question::Lift(_) => Timeout::Approval,
            Question::Process(_) => Timeout::Never,
        }
    }

    /// The deadline it gets when it's shown from the queue at `now`.
    pub fn deadline(&self, now: Instant, approval: crate::config::Timeout) -> Option<Instant> {
        match self.timeout() {
            Timeout::Approval => approval.deadline(now),
            Timeout::Never => None,
        }
    }

    /// Whether whoever asked stopped waiting. A network question drops the connections that stopped waiting, and is
    /// abandoned when none is left.
    pub fn abandoned(&mut self) -> bool {
        match self {
            Question::ProtectedWrite(write) => write.reply.is_closed(),
            Question::Block(block) => block.reply.is_closed(),
            Question::Process(_) => false,
            Question::Network(network) => network.prune(),
            Question::Worker(asked) => asked.abandoned(),
            Question::Lift(lift) => lift.reply.is_closed(),
        }
    }

    /// Ends the question unanswered: its asker hears why, or, when it's asked again later, it comes back.
    pub fn unanswered(self, why: Unanswered) -> Option<Question> {
        let ran_out = |what: &str| match why {
            Unanswered::RunEnded => format!("the run ended before the user answered; {what}"),
            Unanswered::TimedOut => format!("the user didn't answer in time; {what}"),
            Unanswered::AskerLeft => what.to_string(),
        };
        match (self, why) {
            // Nothing waits on its answer, and the block stays: it's asked again.
            (question @ Question::Process(_), Unanswered::RunEnded) => Some(question),
            (Question::Process(_), Unanswered::TimedOut | Unanswered::AskerLeft) => None,
            // The connections keep waiting.
            (question @ Question::Network(_), Unanswered::RunEnded) => Some(question),
            (Question::Network(network), Unanswered::TimedOut | Unanswered::AskerLeft) => {
                let verdict = network::refused(&network.to, "the user didn't answer in time");
                network.settle(&verdict);
                None
            }
            (Question::ProtectedWrite(write), why) => {
                let _ = write.reply.send(agent::WriteOutcome::Refused {
                    reason: match why {
                        Unanswered::TimedOut => ran_out("it wasn't written"),
                        Unanswered::RunEnded | Unanswered::AskerLeft => ran_out("ask again if it's still needed"),
                    },
                });
                None
            }
            (Question::Block(block), why) => {
                let _ = block.reply.send(agent::RunDecision::NotRun {
                    reason: match why {
                        Unanswered::TimedOut => ran_out("it didn't run"),
                        Unanswered::RunEnded | Unanswered::AskerLeft => "the run ended before the user answered".into(),
                    },
                });
                None
            }
            // Dropping it closes its reply: the operation doesn't happen.
            (Question::Worker(_), Unanswered::RunEnded | Unanswered::TimedOut | Unanswered::AskerLeft) => None,
            (Question::Lift(lift), _) => {
                let _ = lift.reply.send(agent::lift::LiftDecision::Refused {
                    reason: ran_out("ask again if it's still needed"),
                });
                None
            }
        }
    }
}

/// The question showing: its dialog, and when it times out.
pub struct Showing {
    pub question: Question,
    /// The dialog showing it now: a fresh id each time it's shown.
    pub dialog: String,
    pub deadline: Option<Instant>,
}

/// A question that ended unanswered, for the session: its dialog closes in every client unless it already left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub dialog: String,
    pub kind: &'static str,
    pub why: Unanswered,
}

/// A waiting question's deadline: none yet when it was never shown, else the one it got when it first showed (kept
/// when it shows again after its dialog left with the run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Deadline {
    Unshown,
    Shown(Option<Instant>),
}

struct Queued {
    question: Question,
    deadline: Deadline,
}

/// The session's questions: the one showing, and the ones waiting, oldest first.
#[derive(Default)]
pub struct Questions {
    showing: Option<Showing>,
    waiting: VecDeque<Queued>,
}

impl Questions {
    /// A question waits its turn at the back. A connection to a destination that already has a question, showing or
    /// waiting, joins it.
    pub fn enqueue(&mut self, question: Question) {
        let question = match question {
            Question::Network(mut asked) => {
                let showing = self.showing.as_mut().map(|s| &mut s.question);
                match showing
                    .into_iter()
                    .chain(self.waiting.iter_mut().map(|q| &mut q.question))
                    .find_map(|q| match q {
                        Question::Network(same) if same.to == asked.to => Some(same),
                        Question::Network(_)
                        | Question::ProtectedWrite(_)
                        | Question::Block(_)
                        | Question::Process(_)
                        | Question::Worker(_)
                        | Question::Lift(_) => None,
                    }) {
                    Some(same) => {
                        same.waiting.append(&mut asked.waiting);
                        return;
                    }
                    None => Question::Network(asked),
                }
            }
            question @ (Question::ProtectedWrite(_)
            | Question::Block(_)
            | Question::Process(_)
            | Question::Worker(_)
            | Question::Lift(_)) => question,
        };
        self.waiting.push_back(Queued {
            question,
            deadline: Deadline::Unshown,
        });
    }

    #[cfg(test)]
    pub fn showing(&self) -> Option<&Showing> {
        self.showing.as_ref()
    }

    /// The block question showing, for the session state's typed `questions`.
    pub fn showing_block(&self) -> Option<(&commands::PendingQuestion, &str)> {
        let showing = self.showing.as_ref()?;
        match &showing.question {
            Question::Block(block) => Some((block, showing.dialog.as_str())),
            Question::ProtectedWrite(_)
            | Question::Process(_)
            | Question::Network(_)
            | Question::Worker(_)
            | Question::Lift(_) => None,
        }
    }

    /// Whether a question waits for the slot.
    pub fn is_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// The next question to show, taken from the queue, when the slot is free: nothing showing, and no dialog of pi's
    /// pending. Questions whose asker stopped waiting are dropped on the way. Returns its deadline: from `now` when
    /// it's shown for the first time, the one it had otherwise. The caller renders it and calls [`Self::show`].
    pub fn next(
        &mut self,
        pi_dialog_pending: bool,
        now: Instant,
        approval: crate::config::Timeout,
    ) -> Option<(Question, Option<Instant>)> {
        if self.showing.is_some() || pi_dialog_pending {
            return None;
        }
        while let Some(Queued { mut question, deadline }) = self.waiting.pop_front() {
            if !question.abandoned() {
                let deadline = match deadline {
                    Deadline::Unshown => question.deadline(now, approval),
                    Deadline::Shown(deadline) => deadline,
                };
                return Some((question, deadline));
            }
        }
        None
    }

    /// Shows `question` under `dialog`: a question from [`Self::next`] with its deadline, or one taken with
    /// [`Self::take_answered`] shown again with the deadline it had.
    pub fn show(&mut self, question: Question, dialog: String, deadline: Option<Instant>) {
        debug_assert!(self.showing.is_none(), "one question shows at a time");
        self.showing = Some(Showing {
            question,
            dialog,
            deadline,
        });
    }

    /// The showing question, when a client answered its dialog.
    pub fn take_answered(&mut self, dialog: &str) -> Option<Showing> {
        match &self.showing {
            Some(showing) if showing.dialog == dialog => self.showing.take(),
            Some(_) | None => None,
        }
    }

    /// The showing block question, when a `view` client decided its typed question (`session.decide`), with the dialog
    /// its `rpc` rendering showed.
    pub fn take_decided(&mut self, question: &str) -> Option<(commands::PendingQuestion, String)> {
        match self.showing_block() {
            Some((block, _)) if block.question.id == question => {}
            Some(_) | None => return None,
        }
        let showing = self.showing.take()?;
        match showing.question {
            Question::Block(block) => Some((block, showing.dialog)),
            other @ (Question::ProtectedWrite(_)
            | Question::Process(_)
            | Question::Network(_)
            | Question::Worker(_)
            | Question::Lift(_)) => {
                self.showing = Some(Showing {
                    question: other,
                    ..showing
                });
                None
            }
        }
    }

    /// Drops waiting questions whose asker left, and ends the showing one when its dialog left with the run
    /// (`has_dialog` says it's gone) or its asker left. A process or network question whose dialog left goes back to
    /// the front, to be asked again.
    pub fn settle(&mut self, has_dialog: impl Fn(&str) -> bool) -> Option<Ended> {
        self.waiting.retain_mut(|q| !q.question.abandoned());
        let showing = self.showing.as_mut()?;
        let why = match (has_dialog(&showing.dialog), showing.question.abandoned()) {
            (false, _) => Unanswered::RunEnded,
            (true, true) => Unanswered::AskerLeft,
            (true, false) => return None,
        };
        self.end(why)
    }

    /// Ends the showing question when it's past its deadline.
    pub fn expire(&mut self, now: Instant) -> Option<Ended> {
        match self.showing.as_ref()?.deadline {
            Some(deadline) if deadline <= now => self.end(Unanswered::TimedOut),
            Some(_) | None => None,
        }
    }

    /// When the showing question times out.
    pub fn deadline(&self) -> Option<Instant> {
        self.showing.as_ref()?.deadline
    }

    fn end(&mut self, why: Unanswered) -> Option<Ended> {
        let showing = self.showing.take()?;
        let ended = Ended {
            dialog: showing.dialog,
            kind: showing.question.kind(),
            why,
        };
        if let Some(again) = showing.question.unanswered(why) {
            self.waiting.push_front(Queued {
                question: again,
                deadline: Deadline::Shown(showing.deadline),
            });
        }
        Some(ended)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        agent::{
            RunDecision, RunRequest, WriteOutcome,
            block::{Live, NoLive},
            lift::LiftDecision,
            question::{Asker, Target},
        },
        launch::sandbox::{
            Destination,
            network::{User, Verdict},
        },
    };

    fn process(target: &str) -> Question {
        Question::Process(commands::ProcessQuestion {
            process: "node".into(),
            operation: "file-write-create".into(),
            target: target.into(),
            pid: 1,
            refusal: format!("file-write-create {target}"),
            live: Live::No(NoLive::NoStream),
            offer: None,
        })
    }

    fn write() -> (Question, oneshot::Receiver<WriteOutcome>) {
        let (reply, outcome) = oneshot::channel();
        let question = Question::ProtectedWrite(PendingWrite {
            root: "/w".into(),
            path: "/w/.piton/config.toml".into(),
            controls: agent::Controls::Sandbox,
            content: String::new(),
            reply,
        });
        (question, outcome)
    }

    fn lift() -> (Question, oneshot::Receiver<LiftDecision>) {
        let (reply, decision) = oneshot::channel();
        let question = Question::Lift(PendingLift {
            targets: vec![LiveLift::Write { path: "/w/.git".into() }],
            reason: "prune".into(),
            reply,
        });
        (question, decision)
    }

    fn block(id: &str) -> (Question, oneshot::Receiver<RunDecision>) {
        let (reply, decision) = oneshot::channel();
        let request = RunRequest {
            call_id: "c1".into(),
            command: "husky install".into(),
            cwd: "/w".into(),
            timeout: None,
            env: Default::default(),
            reason: None,
            rerun: None,
        };
        let question = Question::Block(commands::PendingQuestion {
            question: agent::question::Question {
                id: id.into(),
                command: request.command.clone(),
                asker: Asker::Piton,
                refusals: Vec::new(),
                note: None,
                writable_vars: Vec::new(),
                runtime: Target::Sandbox,
            },
            stage: commands::Stage::Choice,
            request,
            tag: "t-1".into(),
            reply,
        });
        (question, decision)
    }

    fn connection(to: &str) -> (Question, oneshot::Receiver<Verdict>) {
        let (reply, verdict) = oneshot::channel();
        let to = Destination::parse(to).unwrap();
        let question = Question::Network(network::NetworkQuestion {
            to: to.clone(),
            waiting: vec![network::Waiting {
                user: User("pi".into()),
                to,
                reply,
            }],
        });
        (question, verdict)
    }

    const NEVER: crate::config::Timeout = crate::config::Timeout::Never;

    /// The next question, without its deadline.
    fn next(questions: &mut Questions, pi_dialog: bool) -> Option<Question> {
        questions.next(pi_dialog, Instant::now(), NEVER).map(|(q, _)| q)
    }

    /// Shows the next question under `dialog`, as the session does; its kind.
    fn show(questions: &mut Questions, dialog: &str, pi_dialog: bool) -> Option<&'static str> {
        let (question, deadline) = questions.next(pi_dialog, Instant::now(), NEVER)?;
        let kind = question.kind();
        questions.show(question, dialog.into(), deadline);
        Some(kind)
    }

    fn target(question: &Question) -> String {
        match question {
            Question::Process(p) => p.target.clone(),
            Question::ProtectedWrite(_)
            | Question::Block(_)
            | Question::Network(_)
            | Question::Worker(_)
            | Question::Lift(_) => String::new(),
        }
    }

    /// One question shows at a time, the rest in arrival order across kinds.
    #[test]
    fn shows_one_at_a_time_in_arrival_order() {
        let mut questions = Questions::default();
        let (w, _w) = write();
        let (l, _l) = lift();
        questions.enqueue(process("/a"));
        questions.enqueue(w);
        questions.enqueue(l);
        assert_eq!(show(&mut questions, "d1", false), Some("process"));
        assert!(next(&mut questions, false).is_none(), "the slot is taken");
        assert!(
            questions.take_answered("other").is_none(),
            "only its own dialog answers it"
        );
        assert!(questions.take_answered("d1").is_some());
        assert_eq!(show(&mut questions, "d2", false), Some("write"));
        questions.take_answered("d2");
        assert_eq!(show(&mut questions, "d3", false), Some("lift"));
        questions.take_answered("d3");
        assert!(next(&mut questions, false).is_none() && !questions.is_waiting());
    }

    /// A dialog of pi's holds the slot: the question waits until it closes.
    #[test]
    fn a_pi_dialog_holds_the_slot() {
        let mut questions = Questions::default();
        questions.enqueue(process("/a"));
        assert_eq!(show(&mut questions, "d1", true), None);
        assert!(questions.is_waiting());
        assert_eq!(show(&mut questions, "d1", false), Some("process"));
    }

    /// An asker that stops waiting: dropped from the queue without showing; the showing one ends, and its dialog
    /// closes.
    #[test]
    fn a_question_whose_asker_left_is_dropped() {
        let mut questions = Questions::default();
        let (first, first_outcome) = write();
        let (gone, gone_outcome) = lift();
        questions.enqueue(first);
        questions.enqueue(gone);
        questions.enqueue(process("/a"));
        show(&mut questions, "d1", false);
        drop(gone_outcome);
        assert_eq!(questions.settle(|_| true), None, "the showing one's asker still waits");
        drop(first_outcome);
        assert_eq!(
            questions.settle(|_| true),
            Some(Ended {
                dialog: "d1".into(),
                kind: "write",
                why: Unanswered::AskerLeft
            })
        );
        assert_eq!(
            show(&mut questions, "d2", false),
            Some("process"),
            "the lift was dropped"
        );
    }

    /// A dialog that left with the run: an approval is refused; a process or network question is asked again, first.
    #[tokio::test]
    async fn a_dialog_that_left_with_the_run() {
        let mut questions = Questions::default();
        let (w, outcome) = write();
        questions.enqueue(w);
        questions.enqueue(process("/a"));
        show(&mut questions, "d1", false);
        let ended = questions.settle(|_| false).unwrap();
        assert_eq!(ended.why, Unanswered::RunEnded);
        assert!(matches!(outcome.await, Ok(WriteOutcome::Refused { reason }) if reason.contains("the run ended")));

        questions.enqueue(process("/b"));
        show(&mut questions, "d2", false);
        questions.settle(|_| false);
        let again = next(&mut questions, false).unwrap();
        assert_eq!(target(&again), "/a", "asked again before the one behind it");
        questions.show(again, "d3".into(), None);

        questions.take_answered("d3");
        let (c, verdict) = connection("example.com:443");
        questions.enqueue(c);
        // `/b` is first; the connection waits behind it.
        show(&mut questions, "d4", false);
        questions.take_answered("d4");
        assert_eq!(show(&mut questions, "d5", false), Some("network"));
        questions.settle(|_| false);
        assert_eq!(
            show(&mut questions, "d6", false),
            Some("network"),
            "the connection keeps waiting"
        );
        drop(verdict);
    }

    /// Approvals time out after their deadline; questions without one never do.
    #[tokio::test]
    async fn approvals_time_out() {
        let approval = crate::config::Timeout::After(crate::config::ConfigDuration::from_millis(1000));
        let now = Instant::now();
        let mut questions = Questions::default();
        let (b, decision) = block("piton:question:1");
        assert_eq!(b.deadline(now, approval), Some(now + Duration::from_secs(1)));
        assert_eq!(b.deadline(now, crate::config::Timeout::Never), None);
        assert_eq!(process("/a").deadline(now, approval), None, "a notice never times out");
        let deadline = b.deadline(now, approval);
        questions.enqueue(b);
        let (next, given) = questions.next(false, now, approval).unwrap();
        assert_eq!(given, deadline, "first shown: from now");
        questions.show(next, "d1".into(), given);
        assert_eq!(questions.deadline(), deadline);
        assert_eq!(questions.expire(now), None, "not yet");
        // Shown again (the next stage) with the deadline it had.
        let showing = questions.take_answered("d1").unwrap();
        questions.show(showing.question, "d2".into(), showing.deadline);
        let ended = questions.expire(now + Duration::from_secs(1)).unwrap();
        assert_eq!((ended.dialog.as_str(), ended.why), ("d2", Unanswered::TimedOut));
        assert!(matches!(decision.await, Ok(RunDecision::NotRun { reason }) if reason.contains("in time")));
        assert!(questions.showing().is_none());

        // A network question whose dialog left with the run goes back first, and shows again with the deadline it had.
        let (c, _verdict) = connection("a.com:443");
        questions.enqueue(c);
        let (question, first) = questions.next(false, now, approval).unwrap();
        questions.show(question, "d3".into(), first);
        questions.settle(|_| false);
        let later = now + Duration::from_millis(600);
        let (question, again) = questions.next(false, later, approval).unwrap();
        assert_eq!(again, first, "kept, not restarted");
        questions.show(question, "d4".into(), again);
        assert_eq!(
            questions.expire(now + Duration::from_secs(1)).map(|e| e.why),
            Some(Unanswered::TimedOut)
        );
    }

    /// A connection joins the question for its destination, showing or waiting; another destination waits its turn.
    #[tokio::test]
    async fn connections_join_the_question_for_their_destination() {
        let mut questions = Questions::default();
        let (a1, a1_verdict) = connection("a.com:443");
        let (b1, b1_verdict) = connection("b.com:443");
        let (a2, a2_verdict) = connection("a.com:443");
        let (b2, b2_verdict) = connection("b.com:443");
        questions.enqueue(a1);
        show(&mut questions, "d1", false);
        questions.enqueue(b1);
        questions.enqueue(a2);
        questions.enqueue(b2);
        let showing = questions.take_answered("d1").unwrap();
        let Question::Network(a) = showing.question else {
            panic!("a network question")
        };
        assert_eq!(a.waiting.len(), 2, "joined the showing question");
        a.settle(&Verdict::Allow { literal: false });
        assert!(matches!(a1_verdict.await, Ok(Verdict::Allow { .. })));
        assert!(matches!(a2_verdict.await, Ok(Verdict::Allow { .. })));
        let Some(Question::Network(b)) = next(&mut questions, false) else {
            panic!("the other destination")
        };
        assert_eq!(b.waiting.len(), 2, "joined the waiting question");
        assert!(!questions.is_waiting());
        drop((b, b1_verdict, b2_verdict));
    }

    /// `session.decide` takes only the showing block question with that typed id.
    #[test]
    fn a_typed_answer_takes_only_its_block_question() {
        let mut questions = Questions::default();
        let (b, _decision) = block("piton:question:7");
        assert!(questions.take_decided("piton:question:7").is_none(), "not showing yet");
        questions.enqueue(b);
        show(&mut questions, "piton:block:8", false);
        assert!(questions.take_decided("piton:question:1").is_none());
        let (taken, dialog) = questions.take_decided("piton:question:7").unwrap();
        assert_eq!(
            (taken.question.id.as_str(), dialog.as_str()),
            ("piton:question:7", "piton:block:8")
        );
        assert!(questions.showing().is_none());

        questions.enqueue(process("/a"));
        show(&mut questions, "d1", false);
        assert!(questions.take_decided("piton:question:7").is_none());
        assert!(questions.showing().is_some(), "another kind stays showing");
    }
}
