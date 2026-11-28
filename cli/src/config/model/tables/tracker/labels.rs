//! `tracker.labels`: the label groups workflows manage, and the labels outside them the user keeps or has retired.
//! Linear's Phase label groups (`tracker.phase`) are managed labels too, so their names are checked here with the
//! groups'.
//!
//! Workflows apply labels by name, so every managed name must be unique where an item sees it (case-insensitive): the
//! repository on GitHub; each Linear catalog (issues, Projects), workspace and team labels together.

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

use super::{LinearPhase, LinearPhaseGroup};
use crate::config::model::{
    key::{Issue, Key},
    tables::integration::Line,
};

/// A group or value key: lowercase letters, digits, and `-`, starting with a letter (`kind`, `pi-workflows`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct LabelKey(String);

impl LabelKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for LabelKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = String::deserialize(deserializer)?;
        let mut chars = key.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        match valid {
            true => Ok(Self(key)),
            false => Err(serde::de::Error::custom(format!(
                "{key:?} isn't a label key: lowercase letters, digits, and `-`, starting with a letter"
            ))),
        }
    }
}

impl fmt::Display for LabelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A label's color: `#` and six hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Color(String);

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let color = String::deserialize(deserializer)?;
        let valid = color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit());
        match valid {
            true => Ok(Self(color)),
            false => Err(serde::de::Error::custom(format!(
                "{color:?} isn't a color like \"#1D76DB\""
            ))),
        }
    }
}

/// One tracker label as setup provisions it. Runtime reads only `name`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LabelDefinition {
    pub name: Line,
    pub color: Color,
    pub description: String,
}

/// The workflow items a group applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    Epic,
    Deliverable,
}

/// How many of a group's values an item carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Cardinality {
    #[serde(rename = "exactlyOne")]
    ExactlyOne,
    #[serde(rename = "zeroOrOne")]
    ZeroOrOne,
    #[serde(rename = "oneOrMore")]
    OneOrMore,
    #[serde(rename = "zeroOrMore")]
    ZeroOrMore,
}

/// Where a Linear label lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinearScope {
    Workspace,
    Team,
}

/// Linear's two label catalogs: issue labels and Project labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinearCatalog {
    Issue,
    Project,
}

impl LinearCatalog {
    const ALL: [LinearCatalog; 2] = [LinearCatalog::Issue, LinearCatalog::Project];

    /// The workflow items a catalog's labels go on: a Linear Epic is a Project; Tasks are issues.
    fn item_type(self) -> ItemType {
        match self {
            LinearCatalog::Issue => ItemType::Deliverable,
            LinearCatalog::Project => ItemType::Epic,
        }
    }

    fn name(self) -> &'static str {
        match self {
            LinearCatalog::Issue => "issue",
            LinearCatalog::Project => "Project",
        }
    }
}

/// A group's fields every tracker has.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubLabelGroup {
    pub applies_to: Vec<ItemType>,
    pub cardinality: Cardinality,
    pub values: IndexMap<LabelKey, LabelDefinition>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearLabelGroup {
    pub applies_to: Vec<ItemType>,
    pub cardinality: Cardinality,
    pub values: IndexMap<LabelKey, LabelDefinition>,
    pub scope: LinearScope,
    /// The values' native Linear label group; absent: the values are labels with no group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_group: Option<LabelDefinition>,
}

/// A label outside the groups, named on the keep or retired list.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubLabelEntry {
    pub name: Line,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearLabelEntry {
    pub name: Line,
    pub catalog: LinearCatalog,
    pub scope: LinearScope,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Unmanaged<E> {
    pub keep: Vec<E>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, bound(deserialize = "G: Deserialize<'de>, E: Deserialize<'de>"))]
pub struct Labels<G, E> {
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub groups: Option<IndexMap<LabelKey, G>>,
    /// Labels outside the configuration that the user chose to keep.
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub unmanaged: Option<Unmanaged<E>>,
    /// Retired (archived) labels that are accounted for, so runtime doesn't report them.
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub retired: Option<Vec<E>>,
}

fn none<T>() -> Option<T> {
    None
}

pub type GitHubLabels = Labels<GitHubLabelGroup, GitHubLabelEntry>;
pub type LinearLabels = Labels<LinearLabelGroup, LinearLabelEntry>;

/// One label per Phase.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearPhaseLabels {
    pub planning: LabelDefinition,
    pub execution: LabelDefinition,
    pub self_review: LabelDefinition,
}

/// The reserved group whose value comes from the item's workflow Kind.
const KIND: &str = "kind";
/// The reserved group whose values mark the points where a Task waits for the user's approval.
const GATE: &str = "gate";
/// The gates workflows know how to honor.
const GATES: [&str; 1] = ["plan"];
/// A Deliverable's Kinds.
const DELIVERABLE_KINDS: [&str; 6] = ["feature", "bugfix", "research", "refactor", "audit", "chore"];
/// Linear refuses these label names: they duplicate its own features.
const LINEAR_RESERVED: [&str; 9] = [
    "assignee", "cycle", "effort", "estimate", "hours", "priority", "project", "state", "status",
];
/// GitHub's limit on a label's description, in characters.
const GITHUB_DESCRIPTION_LIMIT: usize = 100;

/// Which tracker's rules apply to a group.
#[derive(Debug, Clone, Copy)]
enum Tracker {
    GitHub,
    Linear,
}

impl Tracker {
    /// The item types a `kind` group must cover. GitHub Epics and Deliverables are both issues, so `Kind: Epic` tells
    /// them apart; a Linear Epic is a Project, which its object type already tells.
    fn kind_coverage(self) -> &'static [ItemType] {
        match self {
            Tracker::GitHub => &[ItemType::Epic, ItemType::Deliverable],
            Tracker::Linear => &[ItemType::Deliverable],
        }
    }
}

fn kinds_for(item: ItemType) -> &'static [&'static str] {
    match item {
        ItemType::Epic => &["epic"],
        ItemType::Deliverable => &DELIVERABLE_KINDS,
    }
}

/// A group's shared fields, for the rules both trackers apply.
struct Group<'a> {
    applies_to: &'a [ItemType],
    cardinality: Cardinality,
    values: &'a IndexMap<LabelKey, LabelDefinition>,
}

impl<'a> From<&'a GitHubLabelGroup> for Group<'a> {
    fn from(group: &'a GitHubLabelGroup) -> Self {
        Self {
            applies_to: &group.applies_to,
            cardinality: group.cardinality,
            values: &group.values,
        }
    }
}

impl<'a> From<&'a LinearLabelGroup> for Group<'a> {
    fn from(group: &'a LinearLabelGroup) -> Self {
        Self {
            applies_to: &group.applies_to,
            cardinality: group.cardinality,
            values: &group.values,
        }
    }
}

impl<'a> Group<'a> {
    /// The values an item of `item` can carry, by value key: none when the group doesn't apply to it; `kind` only the
    /// item's Kinds.
    fn values_for(&self, key: &LabelKey, item: ItemType) -> Vec<(&'a LabelKey, &'a LabelDefinition)> {
        if !self.applies_to.contains(&item) {
            return Vec::new();
        }
        self.values
            .iter()
            .filter(|(value, _)| key.as_str() != KIND || kinds_for(item).contains(&value.as_str()))
            .collect()
    }

    /// The shape every group has, and the reserved groups' own.
    fn check(&self, tracker: Tracker, key: &LabelKey, at: &Key) -> Result<(), Issue> {
        let applies_to = at.name("applies_to");
        if self.applies_to.is_empty() {
            return Err(Issue::new(applies_to, "must name at least one item type"));
        }
        for (i, item) in self.applies_to.iter().enumerate() {
            if self.applies_to[..i].contains(item) {
                return Err(Issue::new(applies_to.index(i), "is listed twice"));
            }
        }
        if self.values.is_empty() {
            return Err(Issue::new(at.name("values"), "must have at least one value"));
        }
        match key.as_str() {
            KIND => self.check_kind(tracker, at),
            GATE => self.check_gate(at),
            _ => Ok(()),
        }
    }

    fn check_kind(&self, tracker: Tracker, at: &Key) -> Result<(), Issue> {
        if self.cardinality != Cardinality::ExactlyOne {
            return Err(Issue::new(
                at.name("cardinality"),
                "the `kind` group must be `exactlyOne`",
            ));
        }
        let coverage = tracker.kind_coverage();
        if self.applies_to.len() != coverage.len() || coverage.iter().any(|item| !self.applies_to.contains(item)) {
            return Err(Issue::new(
                at.name("applies_to"),
                format!(
                    "the `kind` group must apply to exactly {}",
                    names(coverage.iter().map(item_name))
                ),
            ));
        }
        let mut expected: Vec<&str> = self
            .applies_to
            .iter()
            .flat_map(|item| kinds_for(*item).iter().copied())
            .collect();
        let mut actual: Vec<&str> = self.values.keys().map(LabelKey::as_str).collect();
        expected.sort_unstable();
        actual.sort_unstable();
        match expected == actual {
            true => Ok(()),
            false => Err(Issue::new(
                at.name("values"),
                format!(
                    "the `kind` group's values must be exactly {} for its `applies_to`",
                    names(expected.into_iter())
                ),
            )),
        }
    }

    fn check_gate(&self, at: &Key) -> Result<(), Issue> {
        if self.applies_to != [ItemType::Deliverable] {
            return Err(Issue::new(
                at.name("applies_to"),
                "the `gate` group must apply only to \"deliverable\"",
            ));
        }
        if self.cardinality != Cardinality::ZeroOrMore {
            return Err(Issue::new(
                at.name("cardinality"),
                "the `gate` group must be `zeroOrMore`",
            ));
        }
        match self.values.keys().find(|value| !GATES.contains(&value.as_str())) {
            Some(value) => Err(Issue::new(
                at.name("values").name(value.as_str()),
                format!(
                    "unknown gate {:?}; known gates: {}",
                    value.as_str(),
                    names(GATES.into_iter())
                ),
            )),
            None => Ok(()),
        }
    }
}

fn item_name(item: &ItemType) -> &'static str {
    match item {
        ItemType::Epic => "epic",
        ItemType::Deliverable => "deliverable",
    }
}

/// `"a", "b"`.
fn names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(|name| format!("{name:?}")).collect::<Vec<_>>().join(", ")
}

/// Managed names where an item sees them together, case-folded.
struct NameSpace {
    what: String,
    seen: Vec<String>,
}

impl NameSpace {
    fn new(what: String) -> Self {
        Self { what, seen: Vec::new() }
    }

    fn add(&mut self, name: &str, key: Key) -> Result<(), Issue> {
        if self.has(name) {
            return Err(Issue::new(
                key,
                format!("label name {name:?} is used more than once in {}", self.what),
            ));
        }
        self.seen.push(name.to_lowercase());
        Ok(())
    }

    fn has(&self, name: &str) -> bool {
        let folded = name.to_lowercase();
        self.seen.contains(&folded)
    }
}

/// The keep and retired lists: each label once, never on both, never a managed one. `place` is where an entry's label
/// lives (its catalog and scope on Linear); `managed` the names managed where that entry's label is seen.
fn check_lists<'a, E>(
    at: &Key,
    keep: &'a [E],
    retired: &'a [E],
    name: impl Fn(&E) -> &str,
    place: impl Fn(&E) -> String,
    managed: impl Fn(&E) -> bool,
) -> Result<(), Issue> {
    let mut seen: Vec<(String, &str)> = Vec::new();
    for (list, entries, key) in [
        ("keep", keep, at.name("unmanaged").name("keep")),
        ("retired", retired, at.name("retired")),
    ] {
        for (i, entry) in entries.iter().enumerate() {
            let label = name(entry);
            let id = format!("{}\u{0}{}", place(entry), label.to_lowercase());
            let key = key.index(i);
            match seen.iter().find(|(seen, _)| *seen == id) {
                Some((_, previous)) if *previous == list => {
                    return Err(Issue::new(key, format!("label {label:?} is listed twice in {list}")));
                }
                Some(_) => {
                    return Err(Issue::new(
                        key,
                        format!("label {label:?} can't be both kept and retired"),
                    ));
                }
                None => {}
            }
            seen.push((id, list));
            if managed(entry) {
                return Err(Issue::new(key, format!("{list} label {label:?} names a managed label")));
            }
        }
    }
    Ok(())
}

/// `tracker.labels` of a GitHub tracker.
pub fn check_github(labels: Option<&GitHubLabels>, at: &Key) -> Result<(), Issue> {
    let Some(labels) = labels else {
        return Ok(());
    };
    let groups = at.name("groups");
    let mut names = NameSpace::new("the GitHub repository".into());
    for (key, group) in labels.groups.iter().flatten() {
        let at = groups.name(key.as_str());
        Group::from(group).check(Tracker::GitHub, key, &at)?;
        for (value, definition) in &group.values {
            let at = at.name("values").name(value.as_str());
            if definition.description.chars().count() > GITHUB_DESCRIPTION_LIMIT {
                return Err(Issue::new(
                    at.name("description"),
                    format!("GitHub label descriptions have at most {GITHUB_DESCRIPTION_LIMIT} characters"),
                ));
            }
            names.add(definition.name.as_str(), at.name("name"))?;
        }
    }
    check_lists(
        at,
        labels.unmanaged.as_ref().map_or(&[], |unmanaged| &unmanaged.keep[..]),
        labels.retired.as_deref().unwrap_or_default(),
        |entry| entry.name.as_str(),
        |_| String::new(),
        |entry| names.has(entry.name.as_str()),
    )
}

/// `tracker.labels` and `tracker.phase` of a Linear tracker; `at` is `tracker`.
pub fn check_linear(labels: Option<&LinearLabels>, phase: Option<&LinearPhase>, at: &Key) -> Result<(), Issue> {
    let groups_at = at.name("labels").name("groups");
    let groups: Vec<(&LabelKey, &LinearLabelGroup)> = labels
        .iter()
        .flat_map(|labels| labels.groups.iter().flatten())
        .collect();
    for (key, group) in &groups {
        Group::from(*group).check(Tracker::Linear, key, &groups_at.name(key.as_str()))?;
    }
    let mut catalogs = Vec::new();
    for catalog in LinearCatalog::ALL {
        let item = catalog.item_type();
        let mut names = NameSpace::new(format!("the Linear {} label catalog", catalog.name()));
        let mut add = |name: &Line, key: Key| -> Result<(), Issue> {
            if LINEAR_RESERVED.contains(&name.as_str().to_lowercase().as_str()) {
                return Err(Issue::new(
                    key,
                    format!("Linear reserves the label name {:?}", name.as_str()),
                ));
            }
            names.add(name.as_str(), key)
        };
        if let Some(phase) = phase {
            let (group, part): (&LinearPhaseGroup, &str) = match catalog {
                LinearCatalog::Issue => (&phase.issues, "issues"),
                LinearCatalog::Project => (&phase.projects, "projects"),
            };
            let at = at.name("phase").name(part);
            add(&group.group.name, at.name("group").name("name"))?;
            let values = at.name("values");
            for (value, definition) in [
                ("planning", &group.values.planning),
                ("execution", &group.values.execution),
                ("self_review", &group.values.self_review),
            ] {
                add(&definition.name, values.name(value).name("name"))?;
            }
        }
        for (key, group) in &groups {
            let at = groups_at.name(key.as_str());
            let values = Group::from(*group).values_for(key, item);
            if values.is_empty() {
                continue;
            }
            if let Some(native) = &group.native_group {
                add(&native.name, at.name("native_group").name("name"))?;
            }
            for (value, definition) in values {
                add(&definition.name, at.name("values").name(value.as_str()).name("name"))?;
            }
        }
        catalogs.push((catalog, names));
    }
    let Some(labels) = labels else {
        return Ok(());
    };
    check_lists(
        &at.name("labels"),
        labels.unmanaged.as_ref().map_or(&[], |unmanaged| &unmanaged.keep[..]),
        labels.retired.as_deref().unwrap_or_default(),
        |entry| entry.name.as_str(),
        |entry| format!("{:?}\u{0}{:?}", entry.catalog, entry.scope),
        |entry| {
            catalogs
                .iter()
                .any(|(catalog, names)| *catalog == entry.catalog && names.has(entry.name.as_str()))
        },
    )
}
