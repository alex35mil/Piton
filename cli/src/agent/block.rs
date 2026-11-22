//! The block question for a process that isn't a bash call (an MCP server, an extension, pi), and what every block
//! question shares. Only a client can answer; the agent can ask but never approve. The question for a command is
//! `question.rs`.
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
/// the exact entry it saves. Allow adds a `[sandbox]` entry and restarts; Deny adds a `silence` entry, so the block is
/// only logged from then on.
pub fn process_dialog(id: &str, process: &str, refusal: &str, offer: &Offer) -> Value {
    let entry = |lift: &Lift| {
        let (key, value) = lift.entry();
        format!("`{key} = [{}]`", toml::Value::String(value))
    };
    let silence = |lift: &Lift| {
        let entry = crate::config::Silence {
            target: lift.silence_target(),
            process: None,
        };
        format!("`silence = [{}]`", entry.inline())
    };
    let scopes = [
        (PersistScope::Project, &offer.project),
        (PersistScope::Global, &offer.everywhere),
    ];
    let mut lines = Vec::new();
    for (scope, at) in scopes {
        lines.push(format!(
            "- Allow {}: {}, and this session restarts",
            scope.label(),
            entry(at)
        ));
    }
    for (scope, at) in scopes {
        lines.push(format!(
            "- Deny {}: {}; it stays blocked without asking",
            scope.label(),
            silence(at)
        ));
    }
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": format!(
            "The sandbox blocked {process}: {refusal}\n\nEach answer saves to [sandbox] (a restart keeps the conversation \
             but stops a running turn):\n{}",
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
    /// Save a `silence` entry: it stays blocked, and isn't asked about again.
    Silence(PersistScope),
    /// Leave it blocked for now: asked again in a new session.
    NotNow,
}

fn process_options() -> [(String, ProcessAnswer); 6] {
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
        (format!("Deny {}", Project.label()), ProcessAnswer::Silence(Project)),
        (format!("Deny {}", Global.label()), ProcessAnswer::Silence(Global)),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each answer names the exact entry it saves, and reads back as its answer; a dismissal is "Not now".
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
            "in this project: `write = [\"/h/c/-w-p\"]`",
            "everywhere: `write = [\"/h/c\"]`",
            "in this project: `silence = [{ target = \"/h/c/-w-p\" }]`",
            "everywhere: `silence = [{ target = \"/h/c\" }]`",
        ] {
            assert!(title.contains(saved), "{saved} in {title}");
        }
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
                ProcessAnswer::Silence(PersistScope::Project),
                ProcessAnswer::Silence(PersistScope::Global),
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

    /// A blocked Mach service offers `services = [name]`, and Deny silences that service.
    #[test]
    fn a_service_offer_names_the_service() {
        let name = crate::config::ServiceName::Exact("com.example.agent".into());
        let offer = Offer {
            project: Lift::Service(name.clone()),
            everywhere: Lift::Service(name),
        };
        let d = process_dialog("p", "tool", "mach-lookup com.example.agent", &offer);
        let title = d["title"].as_str().unwrap();
        assert!(title.contains("`services = [\"com.example.agent\"]`"), "{title}");
        assert!(
            title.contains("`silence = [{ target = \"com.example.agent\" }]`"),
            "{title}"
        );
    }
}
