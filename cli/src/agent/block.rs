//! The block question for a process that isn't a bash call (an MCP server, an extension, pi), and what every block
//! question shares. Only a client can answer; the agent can ask but never approve. The question for a command is
//! `question.rs`.
//!
//! A block in pi's own sandbox is lifted live (#408): until the run settles, for this session, or always (the entry is
//! saved and lifted now, no restart). A block of another process (a bash call's background job) can't take pi's token:
//! only always (for later sessions and calls) or not now. A question saves no denial, as Codex's prompt doesn't: a
//! block the user never wants is a `"deny"` entry they write themselves, and is then only logged.
//!
//! The dialog is pi's `select` (`extension_ui_request`), so the apps and pi.nvim show it with the dialog UI they already
//! have.

use serde::Serialize;
use serde_json::{Value, json};

use super::answer::{Answer, UntilSettled};
use crate::launch::sandbox::grant::{Lift, LiveLift, Offer};

/// Where a persisting answer saves its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistScope {
    /// `~/.config/piton/config.toml`: every project.
    Global,
    /// The source checkout's `.piton/config.local.toml`: this project.
    Project,
}

impl PersistScope {
    pub fn label(self) -> &'static str {
        match self {
            PersistScope::Global => "everywhere",
            PersistScope::Project => "in this project",
        }
    }
}

/// The first option of every block question, which clients highlight: choosing it shows the question again, so Enter
/// alone never decides.
pub const CHOOSE_BELOW: &str = "Choose an answer below";

/// Whether a block can be lifted in pi's running sandbox, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    /// pi's own sandbox, with its stream open: this is what a token opens.
    Lift(LiveLift),
    /// Not live: only an entry for later sessions (or nothing). The reason is shown.
    No(NoLive),
}

/// Why a block isn't lifted live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoLive {
    /// Another process's (a bash call's background job): it can't take pi's token.
    OtherProcess,
    /// pi has no stream: the addon didn't load, or not macOS.
    NoStream,
    /// The live check refused it (a floor deny or the user's own covers it, or no token kind for it).
    Refused(String),
}

impl NoLive {
    fn describe(&self) -> String {
        match self {
            NoLive::OtherProcess => "it isn't pi's own process, so it can't be widened while it runs".into(),
            NoLive::NoStream => "pi's sandbox can't be widened while it runs (Piton's addon isn't loaded)".into(),
            NoLive::Refused(why) => format!("it can't be widened while it runs: {why}"),
        }
    }
}

/// The user's answer to a process question: until the run settles, for this session, always (saved where the scope
/// says), or not now.
pub type ProcessAnswer = Answer<UntilSettled<()>, (), PersistScope, ()>;

impl super::answer::Offer for PersistScope {
    const OFFERED: bool = true;
}

/// The question for a process that isn't a bash call: the refusal, what each answer does, and for always the exact
/// entry it saves. The options offered follow what's possible: live answers only for `Live::Lift`, always only with a
/// saved form.
pub fn process_dialog(id: &str, process: &str, refusal: &str, live: &Live, offer: Option<&Offer>) -> Value {
    let entry = |lift: &Lift| format!("`{}`", lift.entry());
    let mut lines = Vec::new();
    match live {
        Live::Lift(lift) => {
            lines.push(
                "- Allow until the end of this turn: open now, closed when the agent hands the turn back to you"
                    .to_string(),
            );
            lines.push("- Allow for this session: open until the session ends, nothing saved".to_string());
            match lift {
                LiveLift::Write { .. } | LiveLift::Read { .. } => {
                    lines.push("While it's open, the agent's write and edit tools can change it too.".to_string())
                }
                LiveLift::Service { .. } | LiveLift::Listen => {}
            }
        }
        Live::No(why) => lines.push(format!("({}.)", why.describe())),
    }
    if let Some(offer) = offer {
        for (scope, at) in [
            (PersistScope::Project, &offer.project),
            (PersistScope::Global, &offer.everywhere),
        ] {
            let now = match live {
                Live::Lift(_) => ", open now",
                Live::No(_) => ", for new sessions and commands",
            };
            lines.push(format!("- Always {}: {} in [sandbox]{now}", scope.label(), entry(at)));
        }
    }
    lines.push("- Not now: it stays blocked; asked again later".into());
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": format!("The sandbox blocked {process}: {refusal}\n\n{}", lines.join("\n")),
        "options": process_options(live, offer.is_some()).into_iter().map(|(label, _)| label).collect::<Vec<_>>(),
    })
}

const SETTLES: &str = "Allow until the end of this turn";
const SESSION: &str = "Allow for this session";
const NOT_NOW: &str = "Not now";

/// The options a question offers, each with its answer; `None` is [`CHOOSE_BELOW`], which asks again.
fn process_options(live: &Live, saved: bool) -> Vec<(String, Option<ProcessAnswer>)> {
    use PersistScope::{Global, Project};
    let mut options = vec![(CHOOSE_BELOW.to_string(), None)];
    match live {
        Live::Lift(_) => {
            options.push((SETTLES.to_string(), Some(Answer::Narrowest(UntilSettled(())))));
            options.push((SESSION.to_string(), Some(Answer::Session(()))));
        }
        Live::No(_) => {}
    }
    if saved {
        for scope in [Project, Global] {
            options.push((format!("Always {}", scope.label()), Some(Answer::Always(scope))));
        }
    }
    options.push((NOT_NOW.to_string(), Some(Answer::Deny(()))));
    options
}

/// A client's answer to a process question: `None` shows it again; a dismissal or an unknown value is "Not now".
pub fn read_process_answer(answer: &serde_json::Map<String, Value>, live: &Live, saved: bool) -> Option<ProcessAnswer> {
    let value = answer.get("value").and_then(Value::as_str);
    process_options(live, saved)
        .into_iter()
        .find(|(label, _)| Some(label.as_str()) == value)
        .map_or(Some(Answer::Deny(())), |(_, answer)| answer)
}

/// The question for a connection the gate holds: who asked, where to, and per answer the exact entry it saves. Allow
/// applies at once, no restart (the gate reads the configuration again).
pub fn network_dialog(id: &str, who: &str, to: &str) -> Value {
    let entry = match crate::config::NetworkTarget::try_from(to.to_string()) {
        Ok(target) => format!("`{}`", Lift::Network(target).entry()),
        Err(_) => format!(
            "`network.connections.{} = \"allow\"`",
            toml::Value::String(to.to_string())
        ),
    };
    let mut lines = vec!["- Allow for this session: until it ends, nothing saved".to_string()];
    for scope in [PersistScope::Project, PersistScope::Global] {
        lines.push(format!("- Allow {}: {entry} in [sandbox]", scope.label()));
    }
    lines.push("- Not now: refused this time; asked again next time".into());
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": format!(
            "{who} wants to connect to {to}, which the sandbox's network doesn't list. It waits for your answer.\n\n{}",
            lines.join("\n")
        ),
        "options": network_options().map(|(label, _)| label),
    })
}

/// The answer to a network question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAnswer {
    /// [`CHOOSE_BELOW`]: no answer yet; the question is shown again.
    AskAgain,
    /// Allow this host until the session ends.
    Session,
    /// Save `"host:port" = "allow"` in `[sandbox.network.connections]`.
    Persist(PersistScope),
    /// Refuse it this time.
    NotNow,
}

fn network_options() -> [(String, NetworkAnswer); 5] {
    use PersistScope::{Global, Project};
    [
        (CHOOSE_BELOW.to_string(), NetworkAnswer::AskAgain),
        ("Allow for this session".to_string(), NetworkAnswer::Session),
        (format!("Allow {}", Project.label()), NetworkAnswer::Persist(Project)),
        (format!("Allow {}", Global.label()), NetworkAnswer::Persist(Global)),
        ("Not now".to_string(), NetworkAnswer::NotNow),
    ]
}

/// A client's answer to a network question; a dismissal or an unknown value is "Not now".
pub fn read_network_answer(answer: &serde_json::Map<String, Value>) -> NetworkAnswer {
    let value = answer.get("value").and_then(Value::as_str);
    network_options()
        .into_iter()
        .find(|(label, _)| Some(label.as_str()) == value)
        .map_or(NetworkAnswer::NotNow, |(_, answer)| answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The network question names the entry each Allow saves, offers no Deny, and every option reads back.
    #[test]
    fn network_answers_read_back() {
        let d = network_dialog("n", "curl", "api.example.com:443");
        let title = d["title"].as_str().unwrap();
        assert!(
            title.contains("curl wants to connect to api.example.com:443"),
            "{title}"
        );
        assert!(
            title.contains("`network.connections.\"api.example.com:443\" = \"allow\"`"),
            "{title}"
        );
        assert!(!title.contains("Deny"), "{title}");
        let answers: Vec<NetworkAnswer> = d["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| read_network_answer(json!({"value": v}).as_object().unwrap()))
            .collect();
        assert_eq!(
            answers,
            [
                NetworkAnswer::AskAgain,
                NetworkAnswer::Session,
                NetworkAnswer::Persist(PersistScope::Project),
                NetworkAnswer::Persist(PersistScope::Global),
                NetworkAnswer::NotNow,
            ]
        );
        assert_eq!(
            read_network_answer(json!({"cancelled": true}).as_object().unwrap()),
            NetworkAnswer::NotNow
        );
    }

    fn answers(d: &Value, live: &Live, saved: bool) -> Vec<Option<ProcessAnswer>> {
        d["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| read_process_answer(json!({"value": v}).as_object().unwrap(), live, saved))
            .collect()
    }

    /// pi's own block: live answers, then each Always names the exact entry it saves (no restart), then Not now; no
    /// Deny; a dismissal is "Not now".
    #[test]
    fn process_answers_read_back() {
        let live = Live::Lift(LiveLift::Write {
            path: "/h/c/-w-p".into(),
        });
        let offer = Offer {
            project: Lift::Write("/h/c/-w-p".into()),
            everywhere: Lift::Write("/h/c".into()),
        };
        let process = process_dialog("p", "claude", "file-write-create /h/c/-w-p", &live, Some(&offer));
        let title = process["title"].as_str().unwrap();
        assert!(title.contains("claude"), "{title}");
        for saved in [
            "in this project: `filesystem.\"/h/c/-w-p\" = \"write\"`",
            "everywhere: `filesystem.\"/h/c\" = \"write\"`",
        ] {
            assert!(title.contains(saved), "{saved} in {title}");
        }
        assert!(title.contains("write and edit tools"), "the risk: {title}");
        assert!(!title.contains("restart") && !title.contains("Deny"), "{title}");
        assert_eq!(
            answers(&process, &live, true),
            [
                None,
                Some(Answer::Narrowest(UntilSettled(()))),
                Some(Answer::Session(())),
                Some(Answer::Always(PersistScope::Project)),
                Some(Answer::Always(PersistScope::Global)),
                Some(Answer::Deny(())),
            ]
        );
        let dismissed = read_process_answer(json!({"cancelled": true}).as_object().unwrap(), &live, true);
        assert_eq!(dismissed, Some(Answer::Deny(())));
    }

    /// Git's control files: live answers, no saved form, so no Always.
    #[test]
    fn no_saved_form_no_always() {
        let live = Live::Lift(LiveLift::Write {
            path: "/w/.git/config".into(),
        });
        let d = process_dialog("p", "node", "file-write-data /w/.git/config", &live, None);
        assert_eq!(
            answers(&d, &live, false),
            [
                None,
                Some(Answer::Narrowest(UntilSettled(()))),
                Some(Answer::Session(())),
                Some(Answer::Deny(())),
            ]
        );
    }

    /// Another process's block, or no stream: only Always (for new sessions) and Not now, with why.
    #[test]
    fn without_a_live_lift_only_always_and_not_now() {
        let offer = Offer {
            project: Lift::Write("/x".into()),
            everywhere: Lift::Write("/x".into()),
        };
        for live in [Live::No(NoLive::OtherProcess), Live::No(NoLive::NoStream)] {
            let d = process_dialog("p", "node", "file-write-data /x", &live, Some(&offer));
            let title = d["title"].as_str().unwrap();
            assert!(title.contains("can't be widened while it runs"), "{title}");
            assert!(title.contains("for new sessions"), "{title}");
            assert_eq!(
                answers(&d, &live, true),
                [
                    None,
                    Some(Answer::Always(PersistScope::Project)),
                    Some(Answer::Always(PersistScope::Global)),
                    Some(Answer::Deny(())),
                ]
            );
        }
    }

    /// The option clients highlight first decides nothing: Enter alone never answers.
    #[test]
    fn the_first_option_decides_nothing() {
        let live = Live::Lift(LiveLift::Listen);
        assert_eq!(process_dialog("p", "n", "r", &live, None)["options"][0], CHOOSE_BELOW);
    }

    /// A blocked Mach service and a blocked TCP listen name their entries.
    #[test]
    fn service_and_listen_offers_name_their_entries() {
        let name = crate::config::ServiceName::Exact("com.example.agent".into());
        let offer = Offer {
            project: Lift::Service(name.clone()),
            everywhere: Lift::Service(name),
        };
        let live = Live::Lift(LiveLift::Service {
            name: "com.example.agent".into(),
        });
        let d = process_dialog("p", "tool", "mach-lookup com.example.agent", &live, Some(&offer));
        let title = d["title"].as_str().unwrap();
        assert!(
            title.contains("`macos.services.\"com.example.agent\" = \"allow\"`"),
            "{title}"
        );
        let offer = Offer {
            project: Lift::TcpListen,
            everywhere: Lift::TcpListen,
        };
        let d = process_dialog(
            "p",
            "node",
            "network-inbound local:*:50958",
            &Live::Lift(LiveLift::Listen),
            Some(&offer),
        );
        let title = d["title"].as_str().unwrap();
        assert!(title.contains("`allow_tcp_listening = true`"), "{title}");
    }
}
