//! The block question for a command, a question Piton owns: the agent asks to rerun a bash call after the sandbox
//! blocked it, naming where (`rerun`: the runtime and grant of a `[[sandbox.commands]]` entry), and the user answers in
//! a client: where it runs (the requested sandbox grant, `all`, or the host) and whether to save that for the command.
//! Only a client can answer; the agent can ask but never approve.
//!
//! The question opens on what was asked. Its sandbox side is the requested grant, or `all` when the agent asked for the
//! host: the agent asks for more access, so it can't remove the safer choice. A narrowed grant can be widened to `all`.
//! An answer names one of those runtimes exactly ([`Question::offers`]); editing the grant lists is #338.
//!
//! The daemon holds the question as a typed [`Question`] and renders it per connection, by the protocol its hello
//! chose: `view` clients (the apps) get it in the session state ([`Question::view`]) and answer with a typed
//! [`Answer`] (`session.decide`); `rpc` clients (`piton attach`, so pi.nvim) get pi's `select` ([`rpc_dialog`]), where
//! options switch the runtime or widen the grant and show the question again, and pi's `input` for a pattern. Both map
//! to the same [`Answer`].

use serde_json::{Map, Value, json};

use super::block::{CHOOSE_BELOW, PersistScope};
use crate::{
    config::{AllGrant, CommandPattern, CommandRuntime, Grant},
    launch::env::WritableVars,
};

/// `all`: everything except the floor.
const ALL: Grant = Grant::All(AllGrant::All);

/// What running at `runtime` means, shown in both renderings.
pub fn wording(runtime: &CommandRuntime) -> String {
    match runtime {
        CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => {
            "It runs again in the sandbox with everything open except protected files, configuration, and the daemon's \
             state."
                .into()
        }
        CommandRuntime::Sandbox(Grant::Lists(lists)) => format!(
            "It runs again in the sandbox with only this added: `grant = {}`. Protected files, configuration, and the \
             daemon's state stay closed.",
            lists.inline()
        ),
        CommandRuntime::Host => {
            "It runs again on your Mac with no sandbox, as you, including whatever the agent wrote \
                                 into the files it runs and the environment variables those files set."
                .into()
        }
    }
}

/// Where `runtime` runs, in a sentence: the logs and the `rpc` rendering.
pub fn describe(runtime: &CommandRuntime) -> String {
    match runtime {
        CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => "in the sandbox".into(),
        CommandRuntime::Sandbox(grant @ Grant::Lists(_)) => format!("in the sandbox with `grant = {}`", grant.inline()),
        CommandRuntime::Host => "on the host".into(),
    }
}

/// The block question.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// Stable for the question's life: what `session.decide` names.
    pub id: String,
    pub command: String,
    pub reason: Option<String>,
    /// The refusals the kernel recorded for the call's last run (`operation path`), as context.
    pub refusals: Vec<String>,
    /// What the command opens, for `open`.
    pub note: Option<String>,
    /// The session's environment variables set by files the agent could write: shown on the host side.
    pub writable_vars: Vec<WritableVars>,
    /// What the agent asked for.
    pub requested: CommandRuntime,
    /// What the `rpc` rendering shows: one of the runtimes the question offers, starting at `requested`. A `view` client
    /// keeps its own switch.
    pub showing: CommandRuntime,
}

/// The user's answer, from either rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Once(CommandRuntime),
    Deny,
    /// Run it, and save a `[[sandbox.commands]]` entry for `pattern` with exactly `runtime`.
    Always {
        runtime: CommandRuntime,
        scope: PersistScope,
        pattern: CommandPattern,
    },
}

impl Question {
    /// A question opening on what the agent asked for.
    pub fn new(
        id: String,
        command: String,
        reason: Option<String>,
        refusals: Vec<String>,
        note: Option<String>,
        writable_vars: Vec<WritableVars>,
        requested: CommandRuntime,
    ) -> Self {
        Self {
            id,
            command,
            reason: reason.filter(|r| !r.trim().is_empty()),
            refusals,
            note,
            writable_vars,
            showing: requested.clone(),
            requested,
        }
    }

    /// The sandbox side's grant: the requested one, or `all` when the agent asked for the host.
    fn sandbox_grant(&self) -> &Grant {
        match &self.requested {
            CommandRuntime::Sandbox(grant) => grant,
            CommandRuntime::Host => &ALL,
        }
    }

    /// Whether an answer may run at `runtime`: the host, `all`, or exactly the requested grant.
    pub fn offers(&self, runtime: &CommandRuntime) -> bool {
        match runtime {
            CommandRuntime::Host | CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => true,
            CommandRuntime::Sandbox(Grant::Lists(_)) => *runtime == self.requested,
        }
    }

    /// Refuses an answer that runs somewhere the question doesn't offer.
    pub fn check(&self, answer: &Answer) -> Result<(), String> {
        let runtime = match answer {
            Answer::Deny => return Ok(()),
            Answer::Once(runtime) | Answer::Always { runtime, .. } => runtime,
        };
        match self.offers(runtime) {
            true => Ok(()),
            false => Err(format!(
                "the question doesn't offer running {}: the host, `all`, or the requested grant",
                describe(runtime)
            )),
        }
    }

    /// The same question on the other side: the host, or the sandbox side's grant.
    pub fn switched(&self) -> Self {
        let showing = match self.showing {
            CommandRuntime::Host => CommandRuntime::Sandbox(self.sandbox_grant().clone()),
            CommandRuntime::Sandbox(_) => CommandRuntime::Host,
        };
        Self {
            showing,
            ..self.clone()
        }
    }

    /// The same question at `all`: the `rpc` rendering's widen.
    pub fn widened(&self) -> Self {
        Self {
            showing: CommandRuntime::Sandbox(ALL),
            ..self.clone()
        }
    }

    /// The question as `view` clients get it in the session state.
    pub fn view(&self) -> Value {
        let (runtime, narrowed) = match &self.requested {
            CommandRuntime::Host => ("host", None),
            CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => ("sandbox", None),
            CommandRuntime::Sandbox(Grant::Lists(lists)) => ("sandbox", Some(lists)),
        };
        json!({
            "id": self.id,
            "kind": "command",
            "command": self.command,
            "reason": self.reason,
            "refusals": self.refusals,
            "note": self.note,
            "runtime": runtime,
            "requested": self.requested,
            "grant_entry": narrowed.map(|lists| lists.inline()),
            "wording": {
                "all": wording(&CommandRuntime::Sandbox(ALL)),
                "narrowed": narrowed.map(|lists| wording(&CommandRuntime::Sandbox(Grant::Lists(lists.clone())))),
                "host": wording(&CommandRuntime::Host),
            },
            "suggested_pattern": suggested_pattern(&self.command),
            "writable_vars": self.writable_vars,
        })
    }
}

/// A `session.decide` answer: `{"once": {"runtime", "grant"?}}`, `"deny"`, or
/// `{"always": {"runtime", "grant"?, "scope": "project" | "global", "pattern"}}`. The runtime and grant are read as a
/// `[[sandbox.commands]]` entry's.
pub fn parse_decision(value: &Value) -> Result<Answer, String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "snake_case", deny_unknown_fields)]
    enum Wire {
        Once(CommandRuntime),
        Deny,
        Always(Map<String, Value>),
    }
    let invalid = |e: serde_json::Error| format!("invalid answer: {e}");
    let wire: Wire = serde_json::from_value(value.clone()).map_err(invalid)?;
    Ok(match wire {
        Wire::Once(runtime) => Answer::Once(runtime),
        Wire::Deny => Answer::Deny,
        Wire::Always(mut fields) => {
            let scope = fields
                .remove("scope")
                .ok_or_else(|| "invalid answer: `always` needs `scope`".to_string())?;
            let scope: PersistScope = serde_json::from_value(scope).map_err(invalid)?;
            let pattern = fields
                .remove("pattern")
                .and_then(|p| p.as_str().map(str::to_string))
                .ok_or_else(|| "invalid answer: `always` needs `pattern`".to_string())?;
            // What's left is the runtime and its grant, and nothing else.
            let runtime: CommandRuntime = serde_json::from_value(Value::Object(fields)).map_err(invalid)?;
            Answer::Always {
                runtime,
                scope,
                pattern: CommandPattern::new(&pattern).map_err(|why| format!("pattern: {why}"))?,
            }
        }
    })
}

/// An option of the `rpc` rendering's first dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcChoice {
    /// [`CHOOSE_BELOW`]: no answer yet; the question is shown again.
    AskAgain,
    /// Show the question again on the other side.
    Switch,
    /// Show the question again at `all`: only while a narrowed grant shows.
    Widen,
    Once,
    Deny,
    /// Save the exact command.
    Command(PersistScope),
    /// Save a pattern the user edits next.
    Pattern(PersistScope),
}

/// The options for the side showing. Each side's labels are its own, so a stale answer from another side denies rather
/// than running elsewhere.
fn rpc_options(showing: &CommandRuntime) -> Vec<(String, RpcChoice)> {
    use PersistScope::{Global, Project};
    let (once, prefix, switch) = match showing {
        CommandRuntime::Sandbox(Grant::All(AllGrant::All)) => (
            "Allow once in the sandbox (the whole command runs again)",
            "",
            "Switch to running on the host",
        ),
        CommandRuntime::Sandbox(Grant::Lists(_)) => (
            "Allow once in the sandbox with this grant (the whole command runs again)",
            "With this grant: ",
            "Switch to running on the host",
        ),
        CommandRuntime::Host => (
            "Run once on the host, with no sandbox (the whole command runs again)",
            "On the host: ",
            "Switch to running in the sandbox",
        ),
    };
    let mut options = vec![
        (CHOOSE_BELOW.to_string(), RpcChoice::AskAgain),
        (once.to_string(), RpcChoice::Once),
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
    ];
    match showing {
        CommandRuntime::Sandbox(Grant::Lists(_)) => options.push((
            "Widen to everything but the floor (`grant = \"all\"`)".to_string(),
            RpcChoice::Widen,
        )),
        CommandRuntime::Sandbox(Grant::All(AllGrant::All)) | CommandRuntime::Host => {}
    }
    options.push((switch.to_string(), RpcChoice::Switch));
    options
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

/// The `rpc` rendering's first dialog, in pi's `extension_ui_request` format, for the side the question shows.
pub fn rpc_dialog(dialog_id: &str, question: &Question) -> Value {
    let mut message = format!(
        "The agent asks to run this again {}:\n\n{}\n\n{}",
        describe(&question.showing),
        question.command,
        wording(&question.showing)
    );
    if let Some(note) = &question.note {
        message.push_str(&format!("\n\nIt {note}."));
    }
    if let Some(reason) = &question.reason {
        message.push_str(&format!("\n\nWhy: {}", reason.trim()));
    }
    if !question.refusals.is_empty() {
        message.push_str("\n\nThe sandbox refused:");
        listed(&mut message, &question.refusals);
    }
    match question.showing {
        CommandRuntime::Sandbox(_) => {}
        CommandRuntime::Host => {
            for group in &question.writable_vars {
                match &group.source {
                    Some(file) => message.push_str(&format!("\n\nVariables set by {}:", file.display())),
                    None => message.push_str("\n\nVariables set by a tool that named no file:"),
                }
                listed(&mut message, &group.names);
            }
        }
    }
    let title = match question.showing {
        CommandRuntime::Sandbox(_) => "Run again in the sandbox?",
        CommandRuntime::Host => "Run on the host, with no sandbox?",
    };
    json!({
        "type": "extension_ui_request",
        "id": dialog_id,
        "method": "select",
        "title": format!("{title}\n\n{message}"),
        "options": rpc_options(&question.showing).into_iter().map(|(label, _)| label).collect::<Vec<_>>(),
    })
}

/// The `rpc` rendering's pattern dialog, prefilled with the suggested pattern.
pub fn rpc_pattern_dialog(dialog_id: &str, command: &str, runtime: &CommandRuntime, scope: PersistScope) -> Value {
    let where_ = match runtime {
        CommandRuntime::Host => "on the host, with no sandbox,".to_string(),
        CommandRuntime::Sandbox(_) => describe(runtime),
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

/// A client's answer to the `rpc` first dialog shown for `showing`: an option's label; anything else (a dismissal, an
/// unknown value) denies.
pub fn read_rpc_choice(showing: &CommandRuntime, answer: &Map<String, Value>) -> RpcChoice {
    let value = answer.get("value").and_then(Value::as_str);
    rpc_options(showing)
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
    use crate::config::GrantLists;

    fn narrowed() -> CommandRuntime {
        CommandRuntime::Sandbox(Grant::Lists(Box::new(GrantLists {
            connect: vec![crate::config::Connect::Socket("$TMPDIR".into())],
            ..GrantLists::default()
        })))
    }

    fn all() -> CommandRuntime {
        CommandRuntime::Sandbox(ALL)
    }

    fn question(requested: CommandRuntime) -> Question {
        Question::new(
            "piton:question:1".into(),
            "husky install".into(),
            Some("hooks".into()),
            (0..12).map(|i| format!("file-write-create /h/{i}")).collect(),
            Some("opens https://x in your browser".into()),
            vec![
                WritableVars {
                    source: Some("/w/mise.toml".into()),
                    names: (0..12).map(|i| format!("V{i:02}")).collect(),
                },
                WritableVars {
                    source: None,
                    names: vec!["PATH".into()],
                },
            ],
            requested,
        )
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

    fn choice(showing: &CommandRuntime, value: Value) -> RpcChoice {
        read_rpc_choice(showing, json!({"value": value}).as_object().unwrap())
    }

    fn choices(q: &Question) -> Vec<RpcChoice> {
        rpc_dialog("d", q)["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| choice(&q.showing, v.clone()))
            .collect()
    }

    /// Every option of each side reads back as its choice; anything else denies. The first decides nothing; a narrowed
    /// grant adds widen.
    #[test]
    fn rpc_options_read_back_on_every_side() {
        use PersistScope::{Global, Project};
        let common = [
            RpcChoice::AskAgain,
            RpcChoice::Once,
            RpcChoice::Deny,
            RpcChoice::Command(Project),
            RpcChoice::Command(Global),
            RpcChoice::Pattern(Project),
            RpcChoice::Pattern(Global),
        ];
        for requested in [all(), CommandRuntime::Host] {
            let q = question(requested);
            assert_eq!(choices(&q), [&common[..], &[RpcChoice::Switch]].concat(), "{q:?}");
            assert_eq!(rpc_dialog("d", &q)["options"][0], CHOOSE_BELOW);
            assert_eq!(choice(&q.showing, json!("yes")), RpcChoice::Deny);
        }
        let q = question(narrowed());
        assert_eq!(
            choices(&q),
            [&common[..], &[RpcChoice::Widen, RpcChoice::Switch]].concat()
        );
        // A label of another side isn't this one's: a stale answer denies rather than running elsewhere.
        assert_eq!(
            choice(
                &all(),
                json!("Run once on the host, with no sandbox (the whole command runs again)")
            ),
            RpcChoice::Deny
        );
        assert_eq!(
            choice(&all(), json!("With this grant: Allow this command in this project")),
            RpcChoice::Deny
        );
        assert_eq!(
            choice(
                &narrowed(),
                json!("Allow once in the sandbox (the whole command runs again)")
            ),
            RpcChoice::Deny
        );
        assert_eq!(
            read_rpc_choice(&CommandRuntime::Host, json!({"cancelled": true}).as_object().unwrap()),
            RpcChoice::Deny
        );
    }

    /// The sandbox side says what it lifts; the host side warns and lists the agent-writable variables, capped per file.
    #[test]
    fn the_rpc_title_follows_the_side() {
        let sandbox = rpc_dialog("d", &question(all()));
        let title = sandbox["title"].as_str().unwrap();
        assert!(title.starts_with("Run again in the sandbox?"), "{title}");
        assert!(title.contains("everything open except"), "{title}");
        assert!(
            title.contains("Why: hooks") && title.contains("It opens https://x in your browser."),
            "{title}"
        );
        assert!(title.contains("/h/9") && title.contains("and 2 more"), "{title}");
        assert!(!title.contains("V00"), "no variables on the sandbox side: {title}");

        let narrow = rpc_dialog("d", &question(narrowed()));
        let title = narrow["title"].as_str().unwrap();
        assert!(title.starts_with("Run again in the sandbox?"), "{title}");
        assert!(
            title.contains("again in the sandbox with `grant = { connect = [\"$TMPDIR\"] }`"),
            "{title}"
        );
        assert!(title.contains("only this added"), "{title}");

        let host = rpc_dialog("d", &question(CommandRuntime::Host));
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

    /// The question opens on what was asked; switching moves between the host and the sandbox side, which keeps the
    /// requested grant, or `all` when the host was asked for.
    #[test]
    fn the_question_opens_on_the_request() {
        let q = question(narrowed());
        assert_eq!(q.showing, narrowed());
        let host = q.switched();
        assert_eq!(host.showing, CommandRuntime::Host);
        assert_eq!(host.id, q.id);
        assert_eq!(host.switched(), q, "back to the requested grant");
        assert_eq!(q.widened().showing, all());
        assert_eq!(
            q.widened().switched().switched().showing,
            narrowed(),
            "the sandbox side is the request"
        );

        let q = question(CommandRuntime::Host);
        assert_eq!(q.showing, CommandRuntime::Host);
        assert_eq!(
            q.switched().showing,
            all(),
            "the sandbox side stays when the host is asked for"
        );
    }

    /// An answer runs only where the question offers: the host, `all`, or exactly the requested grant.
    #[test]
    fn answers_run_only_where_offered() {
        let other = CommandRuntime::Sandbox(Grant::Lists(Box::new(GrantLists {
            write: vec!["/".into()],
            ..GrantLists::default()
        })));
        let q = question(narrowed());
        for ok in [narrowed(), all(), CommandRuntime::Host] {
            assert!(q.check(&Answer::Once(ok.clone())).is_ok(), "{ok:?}");
        }
        assert!(q.check(&Answer::Once(other.clone())).is_err());
        assert!(
            q.check(&Answer::Always {
                runtime: other.clone(),
                scope: PersistScope::Project,
                pattern: CommandPattern::new("x").unwrap(),
            })
            .is_err()
        );
        assert!(q.check(&Answer::Deny).is_ok());
        let host = question(CommandRuntime::Host);
        assert!(host.check(&Answer::Once(all())).is_ok());
        assert!(host.check(&Answer::Once(narrowed())).is_err(), "no grant was asked for");
    }

    /// The view rendering carries everything the apps draw, every variable included.
    #[test]
    fn the_view_rendering_is_typed() {
        let v = question(all()).view();
        assert_eq!(v["kind"], "command");
        assert_eq!(v["runtime"], "sandbox");
        assert_eq!(v["requested"], json!({"runtime": "sandbox", "grant": "all"}));
        assert_eq!(v["grant_entry"], Value::Null);
        assert_eq!(v["wording"]["narrowed"], Value::Null);
        assert_eq!(v["suggested_pattern"], "husky install *");
        assert_eq!(v["refusals"].as_array().unwrap().len(), 12);
        assert_eq!(v["writable_vars"][0]["source"], "/w/mise.toml");
        assert_eq!(v["writable_vars"][0]["names"].as_array().unwrap().len(), 12);
        assert_eq!(v["writable_vars"][1]["source"], Value::Null);
        assert!(v["wording"]["host"].as_str().unwrap().contains("no sandbox"));
        assert!(v["wording"]["all"].as_str().unwrap().contains("everything open"));

        let v = question(narrowed()).view();
        assert_eq!(v["runtime"], "sandbox");
        assert_eq!(
            v["requested"],
            json!({"runtime": "sandbox", "grant": {"connect": ["$TMPDIR"]}})
        );
        assert_eq!(v["grant_entry"], "{ connect = [\"$TMPDIR\"] }");
        assert!(v["wording"]["narrowed"].as_str().unwrap().contains("only this added"));

        let v = question(CommandRuntime::Host).view();
        assert_eq!(v["runtime"], "host");
        assert_eq!(v["requested"], json!({"runtime": "host"}));
    }

    #[test]
    fn decisions_parse_strictly() {
        assert_eq!(
            parse_decision(&json!({"once": {"runtime": "host"}})),
            Ok(Answer::Once(CommandRuntime::Host))
        );
        assert_eq!(
            parse_decision(&json!({"once": {"runtime": "sandbox", "grant": "all"}})),
            Ok(Answer::Once(all()))
        );
        assert_eq!(
            parse_decision(&json!({"once": {"runtime": "sandbox", "grant": {"connect": ["$TMPDIR"]}}})),
            Ok(Answer::Once(narrowed()))
        );
        assert_eq!(parse_decision(&json!("deny")), Ok(Answer::Deny));
        assert_eq!(
            parse_decision(&json!({"always": {"runtime": "host", "scope": "project", "pattern": "log show *"}})),
            Ok(Answer::Always {
                runtime: CommandRuntime::Host,
                scope: PersistScope::Project,
                pattern: CommandPattern::new("log show *").unwrap(),
            })
        );
        assert_eq!(
            parse_decision(&json!({"always": {
                "runtime": "sandbox", "grant": {"connect": ["$TMPDIR"]}, "scope": "global", "pattern": "x *"
            }})),
            Ok(Answer::Always {
                runtime: narrowed(),
                scope: PersistScope::Global,
                pattern: CommandPattern::new("x *").unwrap(),
            })
        );
        for (bad, says) in [
            (json!({"once": "host"}), ""),
            (json!({"once": {"runtime": "vm"}}), ""),
            (json!({"once": {"runtime": "host", "grant": "all"}}), "`grant` is for"),
            (json!({"once": {"runtime": "sandbox"}}), "needs `grant`"),
            (json!({"once": {"grant": "all"}}), "`runtime` is required"),
            (json!({"once": {"runtime": "sandbox", "grant": {"deny": ["/"]}}}), ""),
            (json!("allow"), ""),
            (
                json!({"always": {"runtime": "host", "scope": "project", "pattern": "  "}}),
                "pattern",
            ),
            (
                json!({"always": {"runtime": "host", "scope": "galaxy", "pattern": "x"}}),
                "",
            ),
            (json!({"always": {"runtime": "host", "pattern": "x"}}), "needs `scope`"),
            (
                json!({"always": {"runtime": "host", "scope": "project", "pattern": "x", "extra": 1}}),
                "extra",
            ),
            (
                json!({"always": {"runtime": "host", "grant": "all", "scope": "project", "pattern": "x"}}),
                "`grant` is for",
            ),
            (json!({"once": {"runtime": "host"}, "extra": 1}), ""),
        ] {
            let err = parse_decision(&bad).expect_err(&bad.to_string());
            assert!(err.contains(says), "{bad}: {err}");
        }
        let pattern = |v: &str| read_pattern(json!({"value": v}).as_object().unwrap());
        assert_eq!(pattern("git push *").unwrap().pattern(), "git push *");
        assert!(pattern("  ").is_none());
    }
}
