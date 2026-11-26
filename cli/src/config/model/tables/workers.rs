//! `[workers]`: how `piton worker create` clones a source, and `[workers.permissions]`: what the daemon may do for
//! workers without asking the user.

use serde::{Deserialize, Serialize};

use super::{Merge, TableValue, permissions::PermissionMode, union};
use crate::{
    config::model::{
        grants::{GrantEntry, GrantFilter, Grants},
        key::{Issue, Key},
    },
    project::validate_relative,
};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkersTable {
    /// Paths relative to the source not copied into a new worker. Union across levels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
    #[serde(default, skip_serializing_if = "WorkerPermissions::is_empty")]
    pub permissions: WorkerPermissions,
}

/// `[workers.permissions]`: each key is `"ask"`, `"allow"`, or `"deny"`, and the nearest level that sets it wins.
/// Unset is `"ask"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPermissions {
    /// A session's agent creating a worker (`create_worker`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_by_agent: Option<PermissionMode>,
    /// A session's agent deleting a worker (`delete_worker`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_by_agent: Option<PermissionMode>,
    /// A source update that changes protected files, before creating or after deleting a worker, whoever asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_files_update: Option<PermissionMode>,
}

/// One key of `[workers.permissions]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPermission {
    CreateByAgent,
    DeleteByAgent,
    ProtectedFilesUpdate,
}

impl WorkerPermission {
    pub const ALL: [WorkerPermission; 3] = [
        WorkerPermission::CreateByAgent,
        WorkerPermission::DeleteByAgent,
        WorkerPermission::ProtectedFilesUpdate,
    ];

    /// The key's name in `[workers.permissions]`.
    pub fn key(self) -> &'static str {
        match self {
            WorkerPermission::CreateByAgent => "create_by_agent",
            WorkerPermission::DeleteByAgent => "delete_by_agent",
            WorkerPermission::ProtectedFilesUpdate => "protected_files_update",
        }
    }

    /// The full dotted key, `workers.permissions.<key>`.
    pub fn path(self) -> String {
        format!("workers.permissions.{}", self.key())
    }
}

impl WorkerPermissions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The decision for `permission`: unset asks.
    pub fn get(&self, permission: WorkerPermission) -> PermissionMode {
        self.slot(permission).unwrap_or(PermissionMode::Ask)
    }

    fn slot(&self, permission: WorkerPermission) -> Option<PermissionMode> {
        match permission {
            WorkerPermission::CreateByAgent => self.create_by_agent,
            WorkerPermission::DeleteByAgent => self.delete_by_agent,
            WorkerPermission::ProtectedFilesUpdate => self.protected_files_update,
        }
    }

    fn slot_mut(&mut self, permission: WorkerPermission) -> &mut Option<PermissionMode> {
        match permission {
            WorkerPermission::CreateByAgent => &mut self.create_by_agent,
            WorkerPermission::DeleteByAgent => &mut self.delete_by_agent,
            WorkerPermission::ProtectedFilesUpdate => &mut self.protected_files_update,
        }
    }

    fn grants(&self) -> Grants {
        WorkerPermission::ALL
            .into_iter()
            .filter_map(|permission| self.slot(permission).and_then(|mode| grant(permission, mode)))
            .collect()
    }

    fn filtered(mut self, filter: &GrantFilter, out: &mut Vec<GrantEntry>) -> Self {
        for permission in WorkerPermission::ALL {
            filter.retain_option(self.slot_mut(permission), |mode| grant(permission, *mode), out);
        }
        self
    }
}

/// `"allow"` loosens what a session may do, so it's a grant in a repository's committed file; `"ask"` and `"deny"`
/// only restrict.
fn grant(permission: WorkerPermission, mode: PermissionMode) -> Option<GrantEntry> {
    match mode {
        PermissionMode::Allow => Some(GrantEntry::set(&permission.path(), "allow")),
        PermissionMode::Ask | PermissionMode::Deny => None,
    }
}

impl TableValue for WorkersTable {
    fn check(&self) -> Result<(), Issue> {
        for (i, skip) in self.skip.iter().enumerate() {
            validate_relative(skip)
                .map_err(|reason| Issue::new(Key::of(&["workers", "skip"]).index(i), format!("{skip:?}: {reason}")))?;
        }
        Ok(())
    }
    fn grants(&self) -> Grants {
        self.permissions.grants()
    }
    fn gated(self, filter: &GrantFilter) -> (Self, Vec<GrantEntry>) {
        let mut out = Vec::new();
        let permissions = self.permissions.filtered(filter, &mut out);
        (
            Self {
                skip: self.skip,
                permissions,
            },
            out,
        )
    }
}

impl Merge for WorkersTable {
    fn merge(&mut self, n: Self) {
        union(&mut self.skip, n.skip);
        for permission in WorkerPermission::ALL {
            if let Some(mode) = n.permissions.slot(permission) {
                *self.permissions.slot_mut(permission) = Some(mode);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_stay_inside_the_source() {
        let workers = WorkersTable {
            skip: vec!["../x".into()],
            permissions: WorkerPermissions::default(),
        };
        assert!(workers.check().is_err());
    }

    fn parse(text: &str) -> Result<WorkersTable, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn permissions_are_ask_allow_or_deny_and_default_to_ask() {
        let workers = parse("[permissions]\ncreate_by_agent = \"allow\"\nprotected_files_update = \"deny\"\n").unwrap();
        let p = workers.permissions;
        assert_eq!(p.get(WorkerPermission::CreateByAgent), PermissionMode::Allow);
        assert_eq!(p.get(WorkerPermission::DeleteByAgent), PermissionMode::Ask);
        assert_eq!(p.get(WorkerPermission::ProtectedFilesUpdate), PermissionMode::Deny);
        assert!(parse("[permissions]\ncreate = \"allow\"\n").is_err(), "strict keys");
        assert!(parse("[permissions]\ncreate_by_agent = \"yes\"\n").is_err());
    }

    /// The nearest level that sets a key wins; one that doesn't leaves it.
    #[test]
    fn the_nearest_setting_wins() {
        let mut merged = parse("[permissions]\ncreate_by_agent = \"allow\"\ndelete_by_agent = \"deny\"\n").unwrap();
        merged.merge(parse("[permissions]\ncreate_by_agent = \"ask\"\n").unwrap());
        assert_eq!(
            merged.permissions.get(WorkerPermission::CreateByAgent),
            PermissionMode::Ask
        );
        assert_eq!(
            merged.permissions.get(WorkerPermission::DeleteByAgent),
            PermissionMode::Deny
        );
    }

    /// In a repository's committed file only `"allow"` is a grant: trust keeps it or leaves it out; restrictions stay.
    #[test]
    fn allow_is_a_grant_trust_decides() {
        let workers = parse("[permissions]\ncreate_by_agent = \"allow\"\ndelete_by_agent = \"deny\"\n").unwrap();
        let grants: Vec<String> = workers.grants().iter().map(ToString::to_string).collect();
        assert_eq!(grants, ["workers.permissions.create_by_agent = \"allow\""]);
        let (kept, out) = workers.clone().gated(&GrantFilter::None);
        assert_eq!(
            kept.permissions.get(WorkerPermission::CreateByAgent),
            PermissionMode::Ask
        );
        assert_eq!(
            kept.permissions.get(WorkerPermission::DeleteByAgent),
            PermissionMode::Deny
        );
        assert_eq!(out.len(), 1);
        let (kept, out) = workers.gated(&GrantFilter::All);
        assert_eq!(
            kept.permissions.get(WorkerPermission::CreateByAgent),
            PermissionMode::Allow
        );
        assert!(out.is_empty());
    }
}
