//! The block question for a process that isn't a bash call (an MCP server, an extension, pi), and what every block
//! question shares. Only a client can answer; the agent can ask but never approve. The question for a command is
//! `question.rs`.
//!
//! A question saves no denial, as Codex's prompt doesn't: Allow saves the entry that lifts the block, Not now leaves it
//! blocked this time. A block the user never wants is a `"deny"` entry they write themselves, and is then only logged.
//!
//! The dialog is pi's `select` (`extension_ui_request`), so the apps and pi.nvim show it with the dialog UI they already
//! have.

use serde::Serialize;
use serde_json::{Value, json};

use crate::launch::sandbox::grant::{Lift, Offer};

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

/// The question for a process that isn't a bash call (an MCP server, an extension, pi): the refusal, and per answer
/// the exact entry it saves. Allow adds a `[sandbox]` entry and restarts.
pub fn process_dialog(id: &str, process: &str, refusal: &str, offer: &Offer) -> Value {
    let entry = |lift: &Lift| format!("`{}`", lift.entry());
    let mut lines = Vec::new();
    for (scope, at) in [
        (PersistScope::Project, &offer.project),
        (PersistScope::Global, &offer.everywhere),
    ] {
        lines.push(format!(
            "- Allow {}: {} in [sandbox], and this session restarts",
            scope.label(),
            entry(at)
        ));
    }
    lines.push("- Not now: it stays blocked; a new session asks again".into());
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": format!(
            "The sandbox blocked {process}: {refusal}\n\n(A restart keeps the conversation but stops a running turn.)\n{}",
            lines.join("\n")
        ),
        "options": process_options().map(|(label, _)| label),
    })
}

/// The answer to a process question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessAnswer {
    /// [`CHOOSE_BELOW`]: no answer yet; the question is shown again.
    AskAgain,
    /// Save the entry that lifts it, and restart.
    Persist(PersistScope),
    /// Leave it blocked for now: asked again in a new session.
    NotNow,
}

fn process_options() -> [(String, ProcessAnswer); 4] {
    use PersistScope::{Global, Project};
    [
        (CHOOSE_BELOW.to_string(), ProcessAnswer::AskAgain),
        (
            format!("Allow {} and restart", Project.label()),
            ProcessAnswer::Persist(Project),
        ),
        (
            format!("Allow {} and restart", Global.label()),
            ProcessAnswer::Persist(Global),
        ),
        ("Not now".to_string(), ProcessAnswer::NotNow),
    ]
}

/// A client's answer to a process question; a dismissal or an unknown value is "Not now".
pub fn read_process_answer(answer: &serde_json::Map<String, Value>) -> ProcessAnswer {
    let value = answer.get("value").and_then(Value::as_str);
    process_options()
        .into_iter()
        .find(|(label, _)| Some(label.as_str()) == value)
        .map_or(ProcessAnswer::NotNow, |(_, answer)| answer)
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

    /// Each Allow names the exact entry it saves; there's no Deny; a dismissal is "Not now".
    #[test]
    fn process_answers_read_back() {
        let process = process_dialog(
            "p",
            "claude",
            "file-write-create /h/c/-w-p",
            &Offer {
                project: Lift::Write("/h/c/-w-p".into()),
                everywhere: Lift::Write("/h/c".into()),
            },
        );
        let title = process["title"].as_str().unwrap();
        assert!(title.contains("claude"), "{title}");
        for saved in [
            "in this project: `filesystem.\"/h/c/-w-p\" = \"write\"`",
            "everywhere: `filesystem.\"/h/c\" = \"write\"`",
        ] {
            assert!(title.contains(saved), "{saved} in {title}");
        }
        assert!(!title.contains("silence") && !title.contains("Deny"), "{title}");
        let answers: Vec<ProcessAnswer> = process["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| read_process_answer(json!({"value": v}).as_object().unwrap()))
            .collect();
        assert_eq!(
            answers,
            [
                ProcessAnswer::AskAgain,
                ProcessAnswer::Persist(PersistScope::Project),
                ProcessAnswer::Persist(PersistScope::Global),
                ProcessAnswer::NotNow,
            ]
        );
        let dismissed = read_process_answer(json!({"cancelled": true}).as_object().unwrap());
        assert_eq!(dismissed, ProcessAnswer::NotNow);
    }

    /// The option clients highlight first decides nothing: Enter alone never answers.
    #[test]
    fn the_first_option_decides_nothing() {
        let offer = Offer {
            project: Lift::Write("/x".into()),
            everywhere: Lift::Write("/x".into()),
        };
        assert_eq!(process_dialog("p", "n", "r", &offer)["options"][0], CHOOSE_BELOW);
    }

    /// A blocked Mach service and a blocked TCP listen name their entries.
    #[test]
    fn service_and_listen_offers_name_their_entries() {
        let name = crate::config::ServiceName::Exact("com.example.agent".into());
        let offer = Offer {
            project: Lift::Service(name.clone()),
            everywhere: Lift::Service(name),
        };
        let d = process_dialog("p", "tool", "mach-lookup com.example.agent", &offer);
        let title = d["title"].as_str().unwrap();
        assert!(
            title.contains("`macos.services.\"com.example.agent\" = \"allow\"`"),
            "{title}"
        );
        let offer = Offer {
            project: Lift::TcpListen,
            everywhere: Lift::TcpListen,
        };
        let d = process_dialog("p", "node", "network-inbound local:*:50958", &offer);
        let title = d["title"].as_str().unwrap();
        assert!(title.contains("`allow_tcp_listening = true`"), "{title}");
    }
}
