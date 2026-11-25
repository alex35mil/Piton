//! The tracker schema and every rule its checks enforce, each refused with the key at fault.

use serde_json::{Value, json};

use crate::config::{ConfigError, validate_table};

fn status() -> Value {
    json!({"backlog": "Backlog", "todo": "Todo", "in_progress": "In Progress", "in_review": "In Review", "done": "Done", "canceled": "Canceled"})
}

fn label(name: &str) -> Value {
    json!({"name": name, "color": "#1D76DB", "description": ""})
}

fn kind_github() -> Value {
    let values: serde_json::Map<String, Value> =
        ["epic", "feature", "bugfix", "research", "refactor", "audit", "chore"]
            .into_iter()
            .map(|kind| (kind.to_string(), label(&format!("Kind: {kind}"))))
            .collect();
    json!({"applies_to": ["epic", "deliverable"], "cardinality": "exactlyOne", "values": values})
}

fn github() -> Value {
    json!({
        "provider": "github",
        "mcp_server": "github",
        "repository": {"owner": "o", "repo": "r"},
        "project": {"owner": "o", "owner_type": "user", "number": 2},
        "labels": {"groups": {"kind": kind_github()}},
        "fields": {
            "status": {"field": "Status", "values": status()},
            "phase": {"field": "Phase", "values": {"planning": "Planning", "execution": "Execution", "self_review": "Self-Review"}},
            "priority": {"scope": "project", "field": "Priority", "values": ["High", "Low"]},
        },
    })
}

fn phase_group(prefix: &str) -> Value {
    json!({
        "scope": "team",
        "group": label(&format!("{prefix} Phase")),
        "values": {
            "planning": label(&format!("{prefix} Planning")),
            "execution": label(&format!("{prefix} Execution")),
            "self_review": label(&format!("{prefix} Self-Review")),
        },
    })
}

fn linear() -> Value {
    json!({
        "provider": "linear",
        "mcp_server": "linear",
        "team": "ENG",
        "phase": {"issues": phase_group("Issue"), "projects": phase_group("Project")},
        "statuses": {"issues": status(), "projects": status()},
    })
}

/// `value` with `edit` applied at the JSON pointer `at` (`null` removes the key).
fn with(mut value: Value, at: &str, edit: Value) -> Value {
    let (parent, key) = at.rsplit_once('/').unwrap();
    let parent = value.pointer_mut(parent).unwrap();
    match (parent, edit) {
        (Value::Object(map), Value::Null) => {
            map.remove(key);
        }
        (Value::Object(map), edit) => {
            map.insert(key.to_string(), edit);
        }
        (Value::Array(items), edit) => items[key.parse::<usize>().unwrap()] = edit,
        (other, _) => panic!("not a container: {other}"),
    }
    value
}

/// The refused key and reason.
fn refused(value: Value) -> (String, String) {
    match validate_table("tracker", &value) {
        Err(ConfigError::Invalid {
            at: Some(at), reason, ..
        }) => {
            assert_eq!(at.line, None, "a value on its own has no lines");
            (at.key.to_string(), reason)
        }
        other => panic!("expected a refused key, got {other:?}"),
    }
}

#[test]
fn valid_trackers_read_back_as_written() {
    for tracker in [github(), linear(), json!({"provider": "none"})] {
        assert_eq!(validate_table("tracker", &tracker).unwrap(), tracker);
    }
}

#[test]
fn the_provider_picks_the_schema() {
    assert_eq!(refused(json!({"provider": "jira"})).0, "tracker.provider");
    assert_eq!(refused(json!({"mcp_server": "github"})).0, "tracker");
    assert_eq!(refused(json!({"provider": "none", "team": "x"})).0, "tracker.team");
    // A key of the other provider is unknown.
    assert_eq!(refused(with(github(), "/team", json!("ENG"))).0, "tracker.team");
    assert_eq!(refused(with(linear(), "/project", json!({}))).0, "tracker.project");
}

#[test]
fn values_are_checked_where_they_are() {
    for (at, value, key) in [
        ("/mcp_server", json!("git hub"), "tracker.mcp_server"),
        ("/repository/repo", json!(""), "tracker.repository.repo"),
        ("/project/number", json!(0), "tracker.project.number"),
        ("/project/owner_type", json!("team"), "tracker.project.owner_type"),
        (
            "/fields/status/values/in_progress",
            Value::Null,
            "tracker.fields.status.values",
        ),
        (
            "/fields/priority/scope",
            json!("board"),
            "tracker.fields.priority.scope",
        ),
        ("/fields/priority/values", json!([]), "tracker.fields.priority.values"),
        (
            "/fields/priority/values/1",
            json!("not set"),
            "tracker.fields.priority.values[1]",
        ),
        (
            "/fields/priority/values/1",
            json!("High"),
            "tracker.fields.priority.values[1]",
        ),
        // toml gives an array item's schema error the array's position.
        (
            "/fields/priority/values/1",
            json!("a\nb"),
            "tracker.fields.priority.values",
        ),
    ] {
        assert_eq!(refused(with(github(), at, value)).0, key, "{at}");
    }
}

#[test]
fn mapped_options_and_fields_are_distinct() {
    let (key, reason) = refused(with(github(), "/fields/status/values/done", json!("Todo")));
    assert_eq!(key, "tracker.fields.status.values");
    assert!(reason.contains("\"Todo\""), "{reason}");
    assert_eq!(
        refused(with(github(), "/fields/phase/values/execution", json!("Planning"))).0,
        "tracker.fields.phase.values"
    );
    assert_eq!(
        refused(with(github(), "/fields/phase/field", json!("Status"))).0,
        "tracker.fields"
    );
    // An issue-scoped Priority isn't a Project field.
    let issue = with(github(), "/fields/priority/scope", json!("issue"));
    assert!(validate_table("tracker", &with(issue, "/fields/priority/field", json!("Status"))).is_ok());
    assert_eq!(
        refused(with(linear(), "/statuses/projects/todo", json!("Backlog"))).0,
        "tracker.statuses.projects"
    );
}

#[test]
fn reserved_groups_have_their_shape() {
    let kind = "/labels/groups/kind";
    assert_eq!(
        refused(with(github(), &format!("{kind}/cardinality"), json!("zeroOrOne"))).0,
        "tracker.labels.groups.kind.cardinality"
    );
    assert_eq!(
        refused(with(github(), &format!("{kind}/applies_to"), json!(["deliverable"]))).0,
        "tracker.labels.groups.kind.applies_to"
    );
    assert_eq!(
        refused(with(github(), &format!("{kind}/values/epic"), Value::Null)).0,
        "tracker.labels.groups.kind.values"
    );
    let gate = |values: Value, applies: Value, cardinality: &str| {
        with(
            github(),
            "/labels/groups/gate",
            json!({"applies_to": applies, "cardinality": cardinality, "values": values}),
        )
    };
    assert!(
        validate_table(
            "tracker",
            &gate(
                json!({"plan": label("Gate: Plan")}),
                json!(["deliverable"]),
                "zeroOrMore"
            )
        )
        .is_ok()
    );
    assert_eq!(
        refused(gate(
            json!({"later": label("Gate: Later")}),
            json!(["deliverable"]),
            "zeroOrMore"
        ))
        .0,
        "tracker.labels.groups.gate.values.later"
    );
    assert_eq!(
        refused(gate(
            json!({"plan": label("Gate: Plan")}),
            json!(["epic"]),
            "zeroOrMore"
        ))
        .0,
        "tracker.labels.groups.gate.applies_to"
    );
    assert_eq!(
        refused(gate(
            json!({"plan": label("Gate: Plan")}),
            json!(["deliverable"]),
            "exactlyOne"
        ))
        .0,
        "tracker.labels.groups.gate.cardinality"
    );
    // On Linear, an Epic is a Project: `kind` covers only Deliverables.
    let deliverable_kinds: serde_json::Map<String, Value> =
        ["feature", "bugfix", "research", "refactor", "audit", "chore"]
            .into_iter()
            .map(|kind| (kind.to_string(), label(&format!("Kind {kind}"))))
            .collect();
    let linear_kind = json!({"applies_to": ["deliverable"], "cardinality": "exactlyOne", "scope": "team", "values": deliverable_kinds});
    let linear = with(linear(), "/labels", json!({"groups": {"kind": linear_kind}}));
    assert!(validate_table("tracker", &linear).is_ok());
    assert_eq!(
        refused(with(
            linear,
            "/labels/groups/kind/applies_to",
            json!(["epic", "deliverable"])
        ))
        .0,
        "tracker.labels.groups.kind.applies_to"
    );
}

#[test]
fn groups_have_a_shape() {
    let group = |value: Value| with(github(), "/labels/groups/area", value);
    let area = json!({"applies_to": ["epic"], "cardinality": "oneOrMore", "values": {"cli": label("Area: CLI")}});
    assert!(validate_table("tracker", &group(area.clone())).is_ok());
    for (at, value, key) in [
        ("/applies_to", json!([]), "tracker.labels.groups.area.applies_to"),
        (
            "/applies_to",
            json!(["epic", "epic"]),
            "tracker.labels.groups.area.applies_to[1]",
        ),
        ("/values", json!({}), "tracker.labels.groups.area.values"),
        ("/cardinality", json!("many"), "tracker.labels.groups.area.cardinality"),
        (
            "/values/cli/color",
            json!("red"),
            "tracker.labels.groups.area.values.cli.color",
        ),
    ] {
        assert_eq!(refused(group(with(area.clone(), at, value))).0, key, "{at}");
    }
    assert_eq!(
        refused(with(github(), "/labels/groups/Area", area.clone())).0,
        "tracker.labels.groups.Area"
    );
}

#[test]
fn managed_names_are_unique_where_items_see_them() {
    // GitHub: one repository, case-insensitive.
    let dup = with(
        github(),
        "/labels/groups/area",
        json!({"applies_to": ["epic"], "cardinality": "oneOrMore", "values": {"x": label("kind: epic")}}),
    );
    let (key, reason) = refused(dup);
    assert_eq!(key, "tracker.labels.groups.area.values.x.name");
    assert!(reason.contains("GitHub repository"), "{reason}");
    // GitHub limits descriptions.
    let long = with(
        github(),
        "/labels/groups/kind/values/epic/description",
        json!("x".repeat(101)),
    );
    assert_eq!(refused(long).0, "tracker.labels.groups.kind.values.epic.description");
    // Linear: Phase labels count, per catalog; a reserved name is refused.
    let clash = with(
        linear(),
        "/phase/projects/values/planning/name",
        json!("Project Execution"),
    );
    assert_eq!(refused(clash).0, "tracker.phase.projects.values.execution.name");
    let same_name_other_catalog = with(linear(), "/phase/projects/group/name", json!("Issue Phase"));
    assert!(validate_table("tracker", &same_name_other_catalog).is_ok());
    let reserved = with(linear(), "/phase/issues/group/name", json!("Status"));
    assert_eq!(refused(reserved).0, "tracker.phase.issues.group.name");
    // A Linear group's values count only in the catalogs of the items it applies to.
    let epic_only = json!({"applies_to": ["epic"], "cardinality": "zeroOrMore", "scope": "team", "values": {"x": label("Issue Planning")}});
    assert!(
        validate_table(
            "tracker",
            &with(linear(), "/labels", json!({"groups": {"area": epic_only}}))
        )
        .is_ok()
    );
}

#[test]
fn keep_and_retired_lists_name_unmanaged_labels_once() {
    let lists = |keep: Value, retired: Value| {
        with(
            with(github(), "/labels/unmanaged", json!({"keep": keep})),
            "/labels/retired",
            retired,
        )
    };
    assert!(
        validate_table(
            "tracker",
            &lists(json!([{"name": "wontfix"}]), json!([{"name": "old"}]))
        )
        .is_ok()
    );
    let (key, reason) = refused(lists(json!([{"name": "a"}, {"name": "A"}]), json!([])));
    assert_eq!(key, "tracker.labels.unmanaged.keep[1]");
    assert!(reason.contains("twice in keep"), "{reason}");
    let (key, reason) = refused(lists(json!([{"name": "a"}]), json!([{"name": "a"}])));
    assert_eq!(key, "tracker.labels.retired[0]");
    assert!(reason.contains("both kept and retired"), "{reason}");
    assert_eq!(
        refused(lists(json!([{"name": "Kind: epic"}]), json!([]))).0,
        "tracker.labels.unmanaged.keep[0]"
    );
    // On Linear, the same name in another catalog or scope is another label.
    let entry = |catalog: &str, scope: &str| json!({"name": "a", "catalog": catalog, "scope": scope});
    let linear_lists = |keep: Value| with(linear(), "/labels", json!({"unmanaged": {"keep": keep}}));
    assert!(
        validate_table(
            "tracker",
            &linear_lists(json!([
                entry("issue", "team"),
                entry("project", "team"),
                entry("issue", "workspace")
            ]))
        )
        .is_ok()
    );
    assert_eq!(
        refused(linear_lists(json!([entry("issue", "team"), entry("issue", "team")]))).0,
        "tracker.labels.unmanaged.keep[1]"
    );
    assert_eq!(
        refused(linear_lists(
            json!([{"name": "Issue Planning", "catalog": "issue", "scope": "team"}])
        ))
        .0,
        "tracker.labels.unmanaged.keep[0]"
    );
}

#[test]
fn the_forge_is_github_or_off() {
    let forge = json!({"provider": "github", "mcp_server": "github", "repository": {"owner": "o", "repo": "r"}});
    assert_eq!(validate_table("forge", &forge).unwrap(), forge);
    assert!(validate_table("forge", &json!({"provider": "none"})).is_ok());
    assert!(validate_table("forge", &json!({"provider": "linear"})).is_err());
    assert!(validate_table("forge", &with(forge, "/repository", Value::Null)).is_err());
    assert!(validate_table("nope", &json!({})).is_err());
}

/// A value checked on its own has no file, so no placement rule applies: keys that only some files may hold pass.
#[test]
fn validate_applies_no_placement_rule() {
    for (table, value) in [
        ("trust", json!({"reconfirm": "on-change"})),
        ("web_search", json!({"api_key": "k"})),
        ("branches", json!({"username": "me"})),
    ] {
        assert_eq!(validate_table(table, &value).unwrap(), value, "{table}");
    }
}
