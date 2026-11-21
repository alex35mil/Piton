//! The block question for a command: the agent asks to rerun a bash call outside the sandbox, the user answers in a
//! client. Only a client can answer; the agent can ask but never approve.
//!
//! The dialog is pi's `select` (`extension_ui_request`), so the apps and pi.nvim show it with the dialog UI they already
//! have. A pattern is asked with a second dialog, an `input` prefilled with the command and its subcommand plus ` *`.

use serde::Serialize;
use serde_json::{Value, json};

use crate::config::CommandPattern;

/// Where "Allow and persist" saves the grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistScope {
    /// `~/.config/piton/config.toml`: every project.
    Global,
    /// The source checkout's `.piton/config.local.toml`: this project.
    Project,
}

impl PersistScope {
    fn label(self) -> &'static str {
        match self {
            PersistScope::Global => "everywhere",
            PersistScope::Project => "in this project",
        }
    }
}

/// The user's answer to the first dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// [`CHOOSE_BELOW`]: no answer yet; the question is shown again.
    AskAgain,
    Once,
    Deny,
    /// Persist the exact command.
    Persist(PersistScope),
    /// Persist a pattern the user edits next.
    Pattern(PersistScope),
}

/// The first option of every block question, which clients highlight: choosing it shows the question again, so Enter
/// alone never decides.
pub const CHOOSE_BELOW: &str = "Choose an answer below";

/// The options, in order, each with its answer.
fn options() -> [(String, Answer); 7] {
    use PersistScope::{Global, Project};
    [
        (CHOOSE_BELOW.to_string(), Answer::AskAgain),
        ("Allow once (the whole command runs again)".to_string(), Answer::Once),
        ("Deny".to_string(), Answer::Deny),
        (
            format!("Allow this command {}", Project.label()),
            Answer::Persist(Project),
        ),
        (
            format!("Allow this command {}", Global.label()),
            Answer::Persist(Global),
        ),
        (
            format!("Allow a pattern {}…", Project.label()),
            Answer::Pattern(Project),
        ),
        (format!("Allow a pattern {}…", Global.label()), Answer::Pattern(Global)),
    ]
}

/// What the question shows: the command, why the agent asks, and what the kernel refused while it ran.
#[derive(Debug, Clone, Default)]
pub struct Question<'a> {
    pub command: &'a str,
    pub reason: Option<&'a str>,
    /// The refusals the kernel recorded for this call (`operation path`), as context.
    pub refusals: &'a [String],
    /// What the command opens, for `open`.
    pub note: Option<&'a str>,
}

/// How many refusals the question lists.
const LISTED: usize = 10;

/// The first dialog, in pi's `extension_ui_request` format.
pub fn dialog(id: &str, question: &Question) -> Value {
    let mut message = format!(
        "The agent asks to run this again outside the sandbox (everything except protected files, configuration, \
         and the daemon's state):\n\n{}",
        question.command
    );
    if let Some(note) = question.note {
        message.push_str(&format!("\n\nIt {note}."));
    }
    if let Some(reason) = question.reason.filter(|r| !r.trim().is_empty()) {
        message.push_str(&format!("\n\nWhy: {}", reason.trim()));
    }
    if !question.refusals.is_empty() {
        message.push_str("\n\nThe sandbox refused:");
        for refusal in question.refusals.iter().take(LISTED) {
            message.push_str(&format!("\n- {refusal}"));
        }
        if question.refusals.len() > LISTED {
            message.push_str(&format!("\n- and {} more", question.refusals.len() - LISTED));
        }
    }
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "select",
        "title": format!("Run outside the sandbox?\n\n{message}"),
        "options": options().map(|(label, _)| label),
    })
}

/// The second dialog: the pattern to persist, prefilled.
pub fn pattern_dialog(id: &str, command: &str, scope: PersistScope) -> Value {
    json!({
        "type": "extension_ui_request",
        "id": id,
        "method": "input",
        "title": format!("Commands to allow {} without asking (`*` matches any text)", scope.label()),
        "placeholder": suggested_pattern(command),
        "prefill": suggested_pattern(command),
    })
}

/// What a process question offers in each scope: the `[sandbox]` key and the path it saves.
pub struct ProcessOffer<'a> {
    pub project: (&'a str, &'a str),
    pub everywhere: (&'a str, &'a str),
}

/// The question for a process that isn't a bash call (an MCP server, an extension, pi): the refusal, and per answer
/// the exact entry it saves. Allow adds a `[sandbox]` entry and restarts; Deny adds a `silence` entry, so the block is
/// only logged from then on.
pub fn process_dialog(id: &str, process: &str, refusal: &str, offer: &ProcessOffer) -> Value {
    let entry = |(key, value): (&str, &str)| format!("`{key} = [\"{value}\"]`");
    let silence = |(_, value): (&str, &str)| format!("`silence = [\"{value}\"]`");
    let scopes = [
        (PersistScope::Project, offer.project),
        (PersistScope::Global, offer.everywhere),
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

/// A client's answer to the first dialog: an option's label; anything else (a dismissal, an unknown value) denies.
pub fn read_answer(answer: &serde_json::Map<String, Value>) -> Answer {
    let value = answer.get("value").and_then(Value::as_str);
    options()
        .into_iter()
        .find(|(label, _)| Some(label.as_str()) == value)
        .map_or(Answer::Deny, |(_, answer)| answer)
}

/// A client's answer to the pattern dialog: the pattern, or `None` when dismissed or empty.
pub fn read_pattern(answer: &serde_json::Map<String, Value>) -> Option<CommandPattern> {
    answer
        .get("value")
        .and_then(Value::as_str)
        .and_then(|p| CommandPattern::new(p).ok())
}

/// The command and its subcommand plus ` *`: `git push origin main` → `git push *`, `husky install` → `husky install *`.
/// A first word that looks like an option or a path stops the subcommand.
pub fn suggested_pattern(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().collect();
    let take = match words.get(1) {
        Some(sub) if !sub.starts_with('-') && !sub.contains('/') && !sub.contains('=') => 2,
        Some(_) | None => 1,
    };
    let head = words.iter().take(take).copied().collect::<Vec<_>>().join(" ");
    match head.is_empty() {
        true => command.trim().to_string(),
        false => format!("{head} *"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_the_command_and_subcommand() {
        assert_eq!(suggested_pattern("git push origin main"), "git push *");
        assert_eq!(suggested_pattern("husky install"), "husky install *");
        assert_eq!(suggested_pattern("mise install node@22"), "mise install *");
        assert_eq!(suggested_pattern("ls -la"), "ls *");
        assert_eq!(suggested_pattern("./run.sh x"), "./run.sh x *");
        assert_eq!(suggested_pattern("node ./build.js"), "node *");
        assert_eq!(suggested_pattern("cargo build --release"), "cargo build *");
        assert_eq!(suggested_pattern("make"), "make *");
    }

    /// Every option reads back as its answer; anything else denies.
    #[test]
    fn answers_read_back() {
        let d = dialog(
            "x",
            &Question {
                command: "husky install",
                reason: Some("hooks"),
                refusals: &(0..12).map(|i| format!("file-write-create /h/{i}")).collect::<Vec<_>>(),
                note: Some("opens https://x in your browser"),
            },
        );
        let title = d["title"].as_str().unwrap();
        assert!(title.contains("It opens https://x in your browser."), "{title}");
        assert!(
            title.contains("husky install") && title.contains("Why: hooks"),
            "{title}"
        );
        assert!(
            title.contains("/h/9") && !title.contains("/h/10\n") && title.contains("and 2 more"),
            "{title}"
        );
        let answer = |value: Value| {
            let map = json!({"value": value}).as_object().unwrap().clone();
            read_answer(&map)
        };
        let labels: Vec<Value> = d["options"].as_array().unwrap().clone();
        let answers: Vec<Answer> = labels.into_iter().map(answer).collect();
        assert_eq!(
            answers,
            [
                Answer::AskAgain,
                Answer::Once,
                Answer::Deny,
                Answer::Persist(PersistScope::Project),
                Answer::Persist(PersistScope::Global),
                Answer::Pattern(PersistScope::Project),
                Answer::Pattern(PersistScope::Global),
            ]
        );
        assert_eq!(answer(json!("yes")), Answer::Deny);
        assert_eq!(
            read_answer(&json!({"cancelled": true}).as_object().unwrap().clone()),
            Answer::Deny
        );
        let pattern = |v: &str| read_pattern(json!({"value": v}).as_object().unwrap());
        assert_eq!(pattern("git push *").unwrap().pattern(), "git push *");
        assert!(pattern("  ").is_none());

        let process = process_dialog(
            "p",
            "claude",
            "file-write-create /h/c/-w-p",
            &ProcessOffer {
                project: ("write", "/h/c/-w-p"),
                everywhere: ("write", "/h/c"),
            },
        );
        let title = process["title"].as_str().unwrap();
        assert!(title.contains("claude"), "{title}");
        // Each answer names the exact entry it saves.
        for saved in [
            "in this project: `write = [\"/h/c/-w-p\"]`",
            "everywhere: `write = [\"/h/c\"]`",
            "in this project: `silence = [\"/h/c/-w-p\"]`",
            "everywhere: `silence = [\"/h/c\"]`",
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
        let first = |d: Value| d["options"][0].as_str().unwrap().to_string();
        assert_eq!(first(dialog("x", &Question::default())), CHOOSE_BELOW);
        let offer = ProcessOffer {
            project: ("write", "/x"),
            everywhere: ("write", "/x"),
        };
        assert_eq!(first(process_dialog("p", "n", "r", &offer)), CHOOSE_BELOW);
    }
}
