//! The block question for a command, a question Piton owns: the agent asks to rerun a bash call after the sandbox
//! blocked it, and the user answers in a client, choosing where it runs (a widened sandbox or the host) and whether to
//! save that for the command. Only a client can answer; the agent can ask but never approve.
//!
//! The daemon holds the question as a typed [`Question`] and renders it per connection, by the protocol its hello
//! chose: `view` clients (the apps) get it in the session state ([`Question::view`]) and answer with a typed
//! [`Answer`] (`session.decide`); `rpc` clients (`piton attach`, so pi.nvim) get pi's `select` ([`rpc_dialog`]), where
//! one option switches the runtime and shows the question again, and pi's `input` for a pattern. Both map to the same
//! [`Answer`].

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{
    answer::Once,
    block::{CHOOSE_BELOW, PersistScope},
};
use crate::{config::CommandPattern, launch::env::WritableVars};

/// Where an approved rerun runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// The session's sandbox with everything lifted except the floor (`all`).
    Sandbox,
    /// The host, with no sandbox.
    Host,
}

impl Target {
    fn other(self) -> Self {
        match self {
            Target::Sandbox => Target::Host,
            Target::Host => Target::Sandbox,
        }
    }
}

/// What running there means, shown in both renderings.
pub fn wording(target: Target) -> &'static str {
    match target {
        Target::Sandbox => {
            "It runs again in the sandbox with everything open except protected files, configuration, and the \
             daemon's state."
        }
        Target::Host => {
            "It runs again on your Mac with no sandbox, as you, including whatever the agent wrote into the files it \
             runs and the environment variables those files set."
        }
    }
}

/// The block question.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// Stable for the question's life: what `session.decide` names.
    pub id: String,
    pub command: String,
    /// Who asked: the agent's rerun request, or Piton itself after the command failed on a block it recorded.
    pub asker: Asker,
    /// The refusals the kernel recorded for the call's last run (`operation path`), as context.
    pub refusals: Vec<String>,
    /// What the command opens, for `open`.
    pub note: Option<String>,
    /// The session's environment variables set by files the agent could write: shown on the host side.
    pub writable_vars: Vec<WritableVars>,
    /// The runtime the `rpc` rendering shows; a `view` client keeps its own switch.
    pub runtime: Target,
}

/// Who brought the question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// The agent asked to rerun it, with why (when it said).
    Agent { reason: Option<String> },
    /// The command failed and the daemon recorded a block an approved rerun lifts.
    Piton,
}

impl Asker {
    fn reason(&self) -> Option<&str> {
        match self {
            Asker::Agent { reason } => reason.as_deref().map(str::trim).filter(|r| !r.is_empty()),
            Asker::Piton => None,
        }
    }
}

/// The user's answer, from either rendering: run it once where chosen (a rerun is one run the daemon bounds), run this
/// exact command there without asking for the rest of the session, save a `[[sandbox.commands]]` entry and run it, or
/// deny.
pub type Answer = super::answer::Answer<Once<Target>, Target, Always, ()>;

impl super::answer::Offer for Target {
    const OFFERED: bool = true;
}

impl super::answer::Offer for Always {
    const OFFERED: bool = true;
}

/// An "always" answer: run it, and save a `[[sandbox.commands]]` entry for `pattern` in `scope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Always {
    pub target: Target,
    pub scope: PersistScope,
    pub pattern: CommandPattern,
}

impl Question {
    /// The question as `view` clients get it in the session state.
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "kind": "command",
            "command": self.command,
            "reason": self.asker.reason(),
            "refusals": self.refusals,
            "note": self.note,
            "runtime": self.runtime,
            "wording": { "sandbox": wording(Target::Sandbox), "host": wording(Target::Host) },
            "suggested_pattern": suggested_pattern(&self.command),
            "writable_vars": self.writable_vars,
        })
    }
}

/// A `session.decide` answer: `{"once": "sandbox" | "host"}`, `{"session": "sandbox" | "host"}`, `"deny"`, or
/// `{"always": {"runtime", "scope": "project" | "global", "pattern"}}`.
pub fn parse_decision(value: &Value) -> Result<Answer, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum Wire {
        Once(Target),
        Session(Target),
        Deny,
        Always(WireAlways),
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WireAlways {
        runtime: Target,
        scope: PersistScope,
        pattern: String,
    }
    let wire: Wire = serde_json::from_value(value.clone()).map_err(|e| format!("invalid answer: {e}"))?;
    Ok(match wire {
        Wire::Once(target) => Answer::Narrowest(Once(target)),
        Wire::Session(target) => Answer::Session(target),
        Wire::Deny => Answer::Deny(()),
        Wire::Always(always) => Answer::Always(Always {
            target: always.runtime,
            scope: always.scope,
            pattern: CommandPattern::new(&always.pattern).map_err(|why| format!("pattern: {why}"))?,
        }),
    })
}

/// An option of the `rpc` rendering's first dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcChoice {
    /// [`CHOOSE_BELOW`]: no answer yet; the question is shown again.
    AskAgain,
    /// Show the question again in the other runtime.
    Switch,
    Once,
    /// Run this exact command there without asking for the rest of the session.
    Session,
    Deny,
    /// Save the exact command.
    Command(PersistScope),
    /// Save a pattern the user edits next.
    Pattern(PersistScope),
}

fn rpc_options(target: Target) -> [(String, RpcChoice); 9] {
    use PersistScope::{Global, Project};
    let (once, prefix, switch) = match target {
        Target::Sandbox => (
            "Allow once in the sandbox (the whole command runs again)",
            "",
            "Switch to running on the host",
        ),
        Target::Host => (
            "Run once on the host, with no sandbox (the whole command runs again)",
            "On the host: ",
            "Switch to running in the sandbox",
        ),
    };
    [
        (CHOOSE_BELOW.to_string(), RpcChoice::AskAgain),
        (once.to_string(), RpcChoice::Once),
        (
            format!("{prefix}Allow this command for this session"),
            RpcChoice::Session,
        ),
        ("Deny".to_string(), RpcChoice::Deny),
        (
            format!("{prefix}Allow this command {}", Project.label()),
            RpcChoice::Command(Project),
        ),
        (
            format!("{prefix}Allow this command {}", Global.label()),
            RpcChoice::Command(Global),
        ),
        (
            format!("{prefix}Allow a pattern {}…", Project.label()),
            RpcChoice::Pattern(Project),
        ),
        (
            format!("{prefix}Allow a pattern {}…", Global.label()),
            RpcChoice::Pattern(Global),
        ),
        (switch.to_string(), RpcChoice::Switch),
    ]
}

/// How many refusals, and agent-writable variables per file, the `rpc` rendering lists: pi's `select` can't fold.
const LISTED: usize = 10;

fn listed(out: &mut String, items: &[String]) {
    for item in items.iter().take(LISTED) {
        out.push_str(&format!("\n- {item}"));
    }
    if items.len() > LISTED {
        out.push_str(&format!("\n- and {} more", items.len() - LISTED));
    }
}

/// The `rpc` rendering's first dialog, in pi's `extension_ui_request` format, for the question's current runtime.
pub fn rpc_dialog(dialog_id: &str, question: &Question) -> Value {
    let where_ = match question.runtime {
        Target::Sandbox => "in the sandbox",
        Target::Host => "on the host",
    };
    let lead = match question.asker {
        Asker::Agent { .. } => format!("The agent asks to run this again {where_}"),
        Asker::Piton => format!("The sandbox blocked this command; run it again {where_}?"),
    };
    let mut message = format!("{lead}:\n\n{}\n\n{}", question.command, wording(question.runtime));
    if let Some(note) = &question.note {
        message.push_str(&format!("\n\nIt {note}."));
    }
    if let Some(reason) = question.asker.reason() {
        message.push_str(&format!("\n\nWhy: {reason}"));
    }
    if !question.refusals.is_empty() {
        message.push_str("\n\nThe sandbox refused:");
        listed(&mut message, &question.refusals);
    }
    match question.runtime {
        Target::Sandbox => {}
        Target::Host => {
            for group in &question.writable_vars {
                match &group.source {
                    Some(file) => message.push_str(&format!("\n\nVariables set by {}:", file.display())),
                    None => message.push_str("\n\nVariables set by a tool that named no file:"),
                }
                listed(&mut message, &group.names);
            }
        }
    }
    let title = match question.runtime {
        Target::Sandbox => "Run again in the sandbox?",
        Target::Host => "Run on the host, with no sandbox?",
    };
    json!({
        "type": "extension_ui_request",
        "id": dialog_id,
        "method": "select",
        "title": format!("{title}\n\n{message}"),
        "options": rpc_options(question.runtime).map(|(label, _)| label),
    })
}

/// The `rpc` rendering's pattern dialog, prefilled with the suggested pattern.
pub fn rpc_pattern_dialog(dialog_id: &str, command: &str, target: Target, scope: PersistScope) -> Value {
    let where_ = match target {
        Target::Sandbox => "in the sandbox",
        Target::Host => "on the host, with no sandbox,",
    };
    json!({
        "type": "extension_ui_request",
        "id": dialog_id,
        "method": "input",
        "title": format!("Commands to run {where_} {} without asking (`*` matches any text)", scope.label()),
        "placeholder": suggested_pattern(command),
        "prefill": suggested_pattern(command),
    })
}

/// A client's answer to the `rpc` first dialog shown for `target`: an option's label; anything else (a dismissal, an
/// unknown value) denies.
pub fn read_rpc_choice(target: Target, answer: &Map<String, Value>) -> RpcChoice {
    let value = answer.get("value").and_then(Value::as_str);
    rpc_options(target)
        .into_iter()
        .find(|(label, _)| Some(label.as_str()) == value)
        .map_or(RpcChoice::Deny, |(_, choice)| choice)
}

/// A client's answer to the pattern dialog: the pattern, or `None` when dismissed or empty.
pub fn read_pattern(answer: &Map<String, Value>) -> Option<CommandPattern> {
    answer
        .get("value")
        .and_then(Value::as_str)
        .and_then(|p| CommandPattern::new(p).ok())
}

impl Question {
    /// The same question in the other runtime: the `rpc` switch.
    pub fn switched(&self) -> Self {
        Self {
            runtime: self.runtime.other(),
            ..self.clone()
        }
    }
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

    fn question(runtime: Target) -> Question {
        Question {
            id: "piton:question:1".into(),
            command: "husky install".into(),
            asker: Asker::Agent {
                reason: Some("hooks".into()),
            },
            refusals: (0..12).map(|i| format!("file-write-create /h/{i}")).collect(),
            note: Some("opens https://x in your browser".into()),
            writable_vars: vec![
                WritableVars {
                    source: Some("/w/mise.toml".into()),
                    names: (0..12).map(|i| format!("V{i:02}")).collect(),
                },
                WritableVars {
                    source: None,
                    names: vec!["PATH".into()],
                },
            ],
            runtime,
        }
    }

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

    fn choice(target: Target, value: Value) -> RpcChoice {
        read_rpc_choice(target, json!({"value": value}).as_object().unwrap())
    }

    /// Every option of each runtime reads back as its choice; anything else denies. The first decides nothing.
    #[test]
    fn rpc_options_read_back_in_both_runtimes() {
        use PersistScope::{Global, Project};
        for target in [Target::Sandbox, Target::Host] {
            let d = rpc_dialog("d", &question(target));
            let choices: Vec<RpcChoice> = d["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| choice(target, v.clone()))
                .collect();
            assert_eq!(
                choices,
                [
                    RpcChoice::AskAgain,
                    RpcChoice::Once,
                    RpcChoice::Session,
                    RpcChoice::Deny,
                    RpcChoice::Command(Project),
                    RpcChoice::Command(Global),
                    RpcChoice::Pattern(Project),
                    RpcChoice::Pattern(Global),
                    RpcChoice::Switch,
                ],
                "{target:?}"
            );
            assert_eq!(d["options"][0], CHOOSE_BELOW);
            assert_eq!(choice(target, json!("yes")), RpcChoice::Deny);
        }
        // A label of the other runtime isn't this one's: a stale answer denies rather than running elsewhere.
        assert_eq!(
            choice(
                Target::Sandbox,
                json!("Run once on the host, with no sandbox (the whole command runs again)")
            ),
            RpcChoice::Deny
        );
        assert_eq!(
            read_rpc_choice(Target::Host, json!({"cancelled": true}).as_object().unwrap()),
            RpcChoice::Deny
        );
    }

    /// The sandbox side says what it lifts; the host side warns and lists the agent-writable variables, capped per file.
    #[test]
    fn the_rpc_title_follows_the_runtime() {
        let sandbox = rpc_dialog("d", &question(Target::Sandbox));
        let title = sandbox["title"].as_str().unwrap();
        assert!(title.starts_with("Run again in the sandbox?"), "{title}");
        assert!(
            title.contains("Why: hooks") && title.contains("It opens https://x in your browser."),
            "{title}"
        );
        assert!(title.contains("/h/9") && title.contains("and 2 more"), "{title}");
        assert!(!title.contains("V00"), "no variables on the sandbox side: {title}");
        assert!(title.contains("The agent asks"), "{title}");

        // Piton's own question says the sandbox blocked it, with no reason.
        let piton = Question {
            asker: Asker::Piton,
            ..question(Target::Sandbox)
        };
        let title = rpc_dialog("d", &piton)["title"].as_str().unwrap().to_string();
        assert!(title.contains("The sandbox blocked this command"), "{title}");
        assert!(!title.contains("Why:"), "{title}");
        assert_eq!(piton.view()["reason"], Value::Null);

        let host = rpc_dialog("d", &question(Target::Host));
        let title = host["title"].as_str().unwrap();
        assert!(title.starts_with("Run on the host, with no sandbox?"), "{title}");
        assert!(title.contains("no sandbox, as you"), "{title}");
        assert!(title.contains("Variables set by /w/mise.toml:\n- V00"), "{title}");
        assert!(
            title.contains("V09") && !title.contains("V10") && title.contains("and 2 more"),
            "{title}"
        );
        assert!(title.contains("a tool that named no file:\n- PATH"), "{title}");
    }

    #[test]
    fn switching_flips_only_the_runtime() {
        let q = question(Target::Sandbox);
        let s = q.switched();
        assert_eq!(s.runtime, Target::Host);
        assert_eq!(s.id, q.id);
        assert_eq!(s.switched(), q);
    }

    /// The view rendering carries everything the apps draw, every variable included.
    #[test]
    fn the_view_rendering_is_typed() {
        let v = question(Target::Sandbox).view();
        assert_eq!(v["kind"], "command");
        assert_eq!(v["runtime"], "sandbox");
        assert_eq!(v["suggested_pattern"], "husky install *");
        assert_eq!(v["refusals"].as_array().unwrap().len(), 12);
        assert_eq!(v["writable_vars"][0]["source"], "/w/mise.toml");
        assert_eq!(v["writable_vars"][0]["names"].as_array().unwrap().len(), 12);
        assert_eq!(v["writable_vars"][1]["source"], Value::Null);
        assert!(v["wording"]["host"].as_str().unwrap().contains("no sandbox"));
    }

    #[test]
    fn decisions_parse_strictly() {
        assert_eq!(
            parse_decision(&json!({"once": "host"})),
            Ok(Answer::Narrowest(Once(Target::Host)))
        );
        assert_eq!(
            parse_decision(&json!({"once": "sandbox"})),
            Ok(Answer::Narrowest(Once(Target::Sandbox)))
        );
        assert_eq!(
            parse_decision(&json!({"session": "sandbox"})),
            Ok(Answer::Session(Target::Sandbox))
        );
        assert_eq!(parse_decision(&json!("deny")), Ok(Answer::Deny(())));
        assert_eq!(
            parse_decision(&json!({"always": {"runtime": "host", "scope": "project", "pattern": "log show *"}})),
            Ok(Answer::Always(Always {
                target: Target::Host,
                scope: PersistScope::Project,
                pattern: CommandPattern::new("log show *").unwrap(),
            }))
        );
        for bad in [
            json!({"once": "vm"}),
            json!({"session": "galaxy"}),
            json!("allow"),
            json!({"always": {"runtime": "host", "scope": "project", "pattern": "  "}}),
            json!({"always": {"runtime": "host", "scope": "galaxy", "pattern": "x"}}),
            json!({"always": {"runtime": "host", "pattern": "x"}}),
            json!({"once": "host", "extra": 1}),
        ] {
            assert!(parse_decision(&bad).is_err(), "{bad}");
        }
        let pattern = |v: &str| read_pattern(json!({"value": v}).as_object().unwrap());
        assert_eq!(pattern("git push *").unwrap().pattern(), "git push *");
        assert!(pattern("  ").is_none());
    }
}
