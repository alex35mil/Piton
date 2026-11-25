//! `[tracker]`: where workflow items live (a GitHub Project or a Linear team), how lifecycle, Priority, and Phase map
//! to the tracker's own fields, and the label groups workflows apply. Off when missing, or with `provider = "none"`.
//!
//! Keys are snake_case; the keys of `labels.groups` and of each group's `values` are names, kept as written.

pub mod labels;

use serde::{Deserialize, Serialize};
use toml::{Spanned, de::DeValue};

use self::labels::{GitHubLabels, LabelDefinition, LinearLabels, LinearPhaseLabels, LinearScope};
use super::{
    ReadError,
    integration::{GitHubProvider, Line, McpServerName, Number, Providers, Repository, RoleTable, Text, read_provider},
};
use crate::config::model::key::{Issue, Key};

/// `[tracker]` as written.
pub type TrackerTable = RoleTable<TrackerProvider>;

/// The tracker's provider and its configuration.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TrackerProvider {
    GitHub(Box<GitHubTracker>),
    Linear(Box<LinearTracker>),
}

impl Providers for TrackerProvider {
    const TABLE: &'static str = "tracker";
    const NAMES: &'static [&'static str] = &["github", "linear"];

    fn read(provider: &str, value: Spanned<DeValue<'_>>) -> Option<Result<Self, ReadError>> {
        match provider {
            "github" => Some(read_provider(value).map(|table| TrackerProvider::GitHub(Box::new(table)))),
            "linear" => Some(read_provider(value).map(|table| TrackerProvider::Linear(Box::new(table)))),
            _ => None,
        }
    }

    fn check(&self) -> Result<(), Issue> {
        let key = Key::of(&["tracker"]);
        match self {
            TrackerProvider::GitHub(github) => github.check(&key),
            TrackerProvider::Linear(linear) => linear.check(&key),
        }
    }
}

/// Issues of one repository on a GitHub Project board.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubTracker {
    pub provider: GitHubProvider,
    pub mcp_server: McpServerName,
    pub repository: Repository,
    pub project: GitHubProject,
    /// Managed labels; applied only when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<GitHubLabels>,
    pub fields: GitHubFields,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubProject {
    pub owner: Text,
    pub owner_type: OwnerType,
    pub number: Number,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerType {
    User,
    Org,
}

/// The Project's fields workflows set.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubFields {
    pub status: StatusField,
    /// Visibility only; without it, workflows track no Phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<PhaseField>,
    pub priority: PriorityField,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusField {
    pub field: Text,
    pub values: StatusValues,
}

/// The tracker's Status option for each lifecycle state.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatusValues {
    pub backlog: Text,
    pub todo: Text,
    pub in_progress: Text,
    pub in_review: Text,
    pub done: Text,
    pub canceled: Text,
}

impl StatusValues {
    fn names(&self) -> [&Text; 6] {
        [
            &self.backlog,
            &self.todo,
            &self.in_progress,
            &self.in_review,
            &self.done,
            &self.canceled,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseField {
    pub field: Text,
    pub values: PhaseValues,
}

/// The Phase option for each Phase.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseValues {
    pub planning: Text,
    pub execution: Text,
    pub self_review: Text,
}

/// Priority: a field of the issue or of the Project item, with its options from most to least urgent.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriorityField {
    pub scope: PriorityScope,
    pub field: Text,
    pub values: Vec<Line>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityScope {
    Issue,
    Project,
}

/// What workflows write for an item with no Priority; never an option's name.
const PRIORITY_NOT_SET: &str = "not set";

impl GitHubTracker {
    fn check(&self, key: &Key) -> Result<(), Issue> {
        let fields = key.name("fields");
        distinct(
            &fields.name("status").name("values"),
            "Status values",
            self.fields.status.values.names(),
        )?;
        if let Some(phase) = &self.fields.phase {
            distinct(
                &fields.name("phase").name("values"),
                "Phase values",
                [
                    &phase.values.planning,
                    &phase.values.execution,
                    &phase.values.self_review,
                ],
            )?;
        }
        self.fields.priority.check(&fields.name("priority"))?;
        let mut project_fields = vec![&self.fields.status.field];
        project_fields.extend(self.fields.phase.as_ref().map(|phase| &phase.field));
        match self.fields.priority.scope {
            PriorityScope::Project => project_fields.push(&self.fields.priority.field),
            PriorityScope::Issue => {}
        }
        distinct(&fields, "Project field names", project_fields)?;
        labels::check_github(self.labels.as_ref(), &key.name("labels"))
    }
}

impl PriorityField {
    fn check(&self, key: &Key) -> Result<(), Issue> {
        let values = key.name("values");
        if self.values.is_empty() {
            return Err(Issue::new(values, "must list at least one option"));
        }
        for (i, value) in self.values.iter().enumerate() {
            if value.as_str() == PRIORITY_NOT_SET {
                return Err(Issue::new(
                    values.index(i),
                    format!("{PRIORITY_NOT_SET:?} means no Priority; it can't be an option"),
                ));
            }
            if self.values[..i].contains(value) {
                return Err(Issue::new(
                    values.index(i),
                    format!("{:?} is listed twice", value.as_str()),
                ));
            }
        }
        Ok(())
    }
}

/// Issues and Projects of one Linear team.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearTracker {
    pub provider: LinearProvider,
    pub mcp_server: McpServerName,
    /// The team's name, key, or ID.
    pub team: Text,
    /// Phase label groups; without them, workflows track no Phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<LinearPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<LinearLabels>,
    pub statuses: LinearStatuses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum LinearProvider {
    #[serde(rename = "linear")]
    Linear,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearStatuses {
    pub issues: StatusValues,
    pub projects: StatusValues,
}

/// One single-select label group per item type, with one label per Phase.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearPhase {
    pub issues: LinearPhaseGroup,
    pub projects: LinearPhaseGroup,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinearPhaseGroup {
    pub scope: LinearScope,
    pub group: LabelDefinition,
    pub values: LinearPhaseLabels,
}

impl LinearTracker {
    fn check(&self, key: &Key) -> Result<(), Issue> {
        let statuses = key.name("statuses");
        distinct(
            &statuses.name("issues"),
            "issue Status values",
            self.statuses.issues.names(),
        )?;
        distinct(
            &statuses.name("projects"),
            "Project Status values",
            self.statuses.projects.names(),
        )?;
        labels::check_linear(self.labels.as_ref(), self.phase.as_ref(), key)
    }
}

/// Refuses a name given twice among `names`.
fn distinct<'a>(key: &Key, what: &str, names: impl IntoIterator<Item = &'a Text>) -> Result<(), Issue> {
    let mut seen: Vec<&str> = Vec::new();
    for name in names {
        if seen.contains(&name.as_str()) {
            return Err(Issue::new(
                key.clone(),
                format!("{what} must be distinct: {:?} is used twice", name.as_str()),
            ));
        }
        seen.push(name.as_str());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
