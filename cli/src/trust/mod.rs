//! Whether a repository's grants apply: the user's decision, asked once per source checkout before anything is
//! launched there.
//!
//! A repository's committed `.piton/config.toml` always restricts; what it grants (`config::GrantEntry`) applies only
//! once the user trusts the repository. The same decision tells pi whether to load the checkout's `.pi/` resources,
//! skills, and the repository's MCP servers (`--approve` / `--no-approve`), so pi's own `trust.json` is never used or
//! written. The decision is kept in the state database, keyed on the canonical source checkout, so workers share it.
//!
//! With `[trust] reconfirm = "never"` (the default) a trusted repository stays trusted whatever its grants become; with
//! `"on-change"` only the grants the user trusted apply, and a launch that finds new ones asks again.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    config::{GrantEntry, GrantFilter, Grants, Reconfirm, TrustInputs},
    state::{Decision, TrustRecord},
};

/// What pi loads from a checkout only when the project is trusted, relative to the checkout (pi's docs, "Resources
/// protected by project trust").
const PI_RESOURCES: [&str; 9] = [
    ".pi/settings.json",
    ".pi/mcp.json",
    ".pi/extensions",
    ".pi/skills",
    ".pi/prompts",
    ".pi/themes",
    ".pi/SYSTEM.md",
    ".pi/APPEND_SYSTEM.md",
    ".agents/skills",
];

/// What trusting a repository lets in, beyond its grants: shown in the question, never fingerprinted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// pi's resources present in the checkout, by path relative to it.
    pub pi: Vec<String>,
    /// The servers of the repository's `.piton/mcp.json`.
    pub mcp_servers: Vec<String>,
}

impl Resources {
    pub fn of(inputs: &TrustInputs) -> Self {
        Self {
            pi: PI_RESOURCES
                .iter()
                .filter(|path| inputs.checkout.join(path).symlink_metadata().is_ok())
                .map(|path| (*path).to_string())
                .collect(),
            mcp_servers: inputs.mcp_servers.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        self.pi.is_empty() && self.mcp_servers.is_empty()
    }
}

/// A repository's trust, decided from its grants, what pi would load, the user's record, and `reconfirm`.
/// Serialized for clients without the filters (what applies is the daemon's business): read back, a filter is
/// `GrantFilter::None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TrustState {
    /// No grants and nothing pi gates, and no decision: nothing to ask. Grants committed later stay out until a launch
    /// asks about them.
    NothingToTrust,
    /// The user trusted it.
    Trusted {
        #[serde(skip, default = "no_grants")]
        filter: GrantFilter,
        grants: Grants,
        resources: Resources,
    },
    /// The user declined: restrictions only, and pi loads nothing of the project's.
    Declined { grants: Grants, resources: Resources },
    /// The user must decide before a launch: never asked, or (`"on-change"`) the grants gained some they didn't trust.
    Needed {
        grants: Grants,
        resources: Resources,
        /// The grants beyond what the user trusted (every grant when never asked).
        added: Grants,
        /// What applies until they answer: what they trusted earlier, else nothing.
        #[serde(skip, default = "no_grants")]
        until_answered: GrantFilter,
    },
}

impl TrustState {
    pub fn decide(inputs: &TrustInputs, record: Option<&TrustRecord>) -> Self {
        let grants = inputs.grants.clone();
        let resources = Resources::of(inputs);
        match record {
            None if grants.is_empty() && resources.is_empty() => TrustState::NothingToTrust,
            None => TrustState::Needed {
                added: grants.clone(),
                grants,
                resources,
                until_answered: GrantFilter::None,
            },
            Some(TrustRecord {
                decision: Decision::Declined,
                ..
            }) => TrustState::Declined { grants, resources },
            Some(TrustRecord {
                decision: Decision::Trusted,
                grants: trusted,
                ..
            }) => match inputs.trust.reconfirm() {
                Reconfirm::Never => TrustState::Trusted {
                    filter: GrantFilter::All,
                    grants,
                    resources,
                },
                Reconfirm::OnChange => {
                    let added: Grants = grants.difference(trusted).cloned().collect();
                    let filter = GrantFilter::Only(trusted.clone());
                    match added.is_empty() {
                        true => TrustState::Trusted {
                            filter,
                            grants,
                            resources,
                        },
                        false => TrustState::Needed {
                            grants,
                            resources,
                            added,
                            until_answered: filter,
                        },
                    }
                }
            },
        }
    }

    /// Which grants apply now: for a launch that doesn't ask (a restart), and for every reader outside one.
    pub fn filter(&self) -> GrantFilter {
        match self {
            TrustState::NothingToTrust => GrantFilter::Only(Grants::new()),
            TrustState::Trusted { filter, .. } => filter.clone(),
            TrustState::Declined { .. } => GrantFilter::None,
            TrustState::Needed { until_answered, .. } => until_answered.clone(),
        }
    }

    /// Whether pi loads the project's own resources: `--approve` or `--no-approve`.
    pub fn pi_trusts(&self) -> bool {
        match self {
            TrustState::NothingToTrust | TrustState::Trusted { .. } => true,
            TrustState::Declined { .. } => false,
            TrustState::Needed { until_answered, .. } => match until_answered {
                GrantFilter::All | GrantFilter::Only(_) => true,
                GrantFilter::None => false,
            },
        }
    }
}

fn no_grants() -> GrantFilter {
    GrantFilter::None
}

/// What a launch was refused with: the user must decide first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustNeeded {
    pub source: PathBuf,
    pub grants: Grants,
    pub resources: Resources,
    pub added: Grants,
}

impl std::fmt::Display for TrustNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} needs your trust before a launch: {}; answer with `piton trust allow {0}` or `piton trust deny {0}`",
            self.source.display(),
            describe(&self.grants, &self.resources, &self.added),
        )
    }
}

/// One line for a message: how many grants (and which are new) and what pi would load.
fn describe(grants: &Grants, resources: &Resources, added: &Grants) -> String {
    let mut parts = Vec::new();
    match (grants.len(), added.len()) {
        (0, _) => {}
        (all, new) if new == all => parts.push(format!("{all} committed grant(s)")),
        (all, new) => parts.push(format!("{new} new of {all} committed grant(s)")),
    }
    if !resources.pi.is_empty() {
        parts.push(format!("pi's project files ({})", resources.pi.join(", ")));
    }
    if !resources.mcp_servers.is_empty() {
        parts.push(format!("MCP servers ({})", resources.mcp_servers.join(", ")));
    }
    parts.join(", ")
}

/// Why a decision can't be recorded.
#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    /// The grants changed after the user was shown them: they never trust what they didn't see.
    #[error("the grants of {} changed since they were shown; ask again", .checkout.display())]
    Changed { checkout: PathBuf, state: Box<TrustState> },
    #[error(transparent)]
    State(#[from] crate::state::StateError),
}

/// A decision to record, checked against what the user was shown: `shown` must be the repository's grants now.
pub fn record(
    state: &crate::state::State,
    inputs: &TrustInputs,
    decision: Decision,
    shown: &Grants,
) -> Result<TrustState, DecideError> {
    if *shown != inputs.grants {
        let current = state.trust(&inputs.source)?;
        return Err(DecideError::Changed {
            checkout: inputs.source.clone(),
            state: Box::new(TrustState::decide(inputs, current.as_ref())),
        });
    }
    let record = TrustRecord {
        decision,
        grants: inputs.grants.clone(),
        decided_at: now(),
    };
    state.set_trust(&inputs.source, &record)?;
    crate::log!(
        "trust: {} {} ({} grant(s))",
        inputs.source.display(),
        match decision {
            Decision::Trusted => "trusted",
            Decision::Declined => "declined",
        },
        inputs.grants.len()
    );
    Ok(TrustState::decide(inputs, Some(&record)))
}

/// The current UTC time as RFC 3339, to the second.
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = secs / 86_400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// The trust state of a workspace from the state database at `db`, read-only: for `piton config` and `piton sandbox`
/// outside the daemon. No database means nothing decided.
pub fn read_state(db: &Path, inputs: &TrustInputs) -> Result<TrustState, crate::state::StateError> {
    let record = crate::state::read_trust(db, &inputs.source)?;
    Ok(TrustState::decide(inputs, record.as_ref()))
}

/// The filter `piton config resolve` and `piton sandbox` use: the session's (`facts`), when they run in one, so a
/// session keeps exactly what its launch kept; otherwise the decision in the state database (none: nothing is kept).
pub fn current_filter(view: &crate::agent::facts::View, db: &Path, inputs: &TrustInputs) -> GrantFilter {
    match view.grants() {
        Some(filter) => filter.clone(),
        None => read_state(db, inputs)
            .map(|state| state.filter())
            .unwrap_or(GrantFilter::None),
    }
}

/// The grants, newest first in display order, as lines: the question's list.
pub fn lines(grants: &Grants, added: &Grants) -> Vec<String> {
    grants
        .iter()
        .map(
            |grant: &GrantEntry| match added.contains(grant) && added.len() != grants.len() {
                true => format!("{grant}  (new)"),
                false => grant.to_string(),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrustInputs;

    fn grant(name: &str) -> GrantEntry {
        GrantEntry::item("sandbox.presets", name)
    }

    fn inputs(dir: &Path, grants: &[&str], reconfirm: Reconfirm) -> TrustInputs {
        TrustInputs {
            source: dir.to_path_buf(),
            checkout: dir.to_path_buf(),
            grants: grants.iter().map(|g| grant(g)).collect(),
            trust: crate::config::TrustTable {
                reconfirm: Some(reconfirm),
            },
            mcp_servers: Vec::new(),
        }
    }

    fn trusted(grants: &[&str]) -> TrustRecord {
        TrustRecord {
            decision: Decision::Trusted,
            grants: grants.iter().map(|g| grant(g)).collect(),
            decided_at: "t".into(),
        }
    }

    #[test]
    fn nothing_to_trust_keeps_later_grants_out() {
        let dir = tempfile::tempdir().unwrap();
        let state = TrustState::decide(&inputs(dir.path(), &[], Reconfirm::Never), None);
        assert_eq!(state, TrustState::NothingToTrust);
        assert!(state.pi_trusts());
        assert!(!state.filter().keeps(&grant("rust")), "a grant committed after launch");
    }

    #[test]
    fn pi_resources_alone_need_a_decision() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".pi/extensions")).unwrap();
        let state = TrustState::decide(&inputs(dir.path(), &[], Reconfirm::Never), None);
        let TrustState::Needed { resources, added, .. } = &state else {
            panic!("{state:?}")
        };
        assert_eq!(resources.pi, [".pi/extensions"]);
        assert!(added.is_empty());
        assert!(!state.pi_trusts());
    }

    #[test]
    fn never_keeps_trust_whatever_the_grants_become() {
        let dir = tempfile::tempdir().unwrap();
        let state = TrustState::decide(
            &inputs(dir.path(), &["rust", "swift"], Reconfirm::Never),
            Some(&trusted(&["rust"])),
        );
        assert!(matches!(state, TrustState::Trusted { .. }));
        assert_eq!(state.filter(), GrantFilter::All);
    }

    #[test]
    fn on_change_asks_about_added_grants_only() {
        let dir = tempfile::tempdir().unwrap();
        let record = trusted(&["rust", "swift"]);
        // Removing one: still trusted, only what was trusted applies.
        let state = TrustState::decide(&inputs(dir.path(), &["rust"], Reconfirm::OnChange), Some(&record));
        assert!(matches!(state, TrustState::Trusted { .. }), "{state:?}");
        assert!(state.filter().keeps(&grant("rust")));
        // Adding one: asked, with what was trusted applying meanwhile.
        let state = TrustState::decide(
            &inputs(dir.path(), &["rust", "node"], Reconfirm::OnChange),
            Some(&record),
        );
        let TrustState::Needed { added, .. } = &state else {
            panic!("{state:?}")
        };
        assert_eq!(added, &[grant("node")].into());
        assert!(state.filter().keeps(&grant("rust")));
        assert!(!state.filter().keeps(&grant("node")));
        assert!(state.pi_trusts());
    }

    #[test]
    fn declined_keeps_every_grant_out() {
        let dir = tempfile::tempdir().unwrap();
        let record = TrustRecord {
            decision: Decision::Declined,
            grants: Grants::new(),
            decided_at: "t".into(),
        };
        let state = TrustState::decide(&inputs(dir.path(), &["rust"], Reconfirm::OnChange), Some(&record));
        assert!(matches!(state, TrustState::Declined { .. }));
        assert_eq!(state.filter(), GrantFilter::None);
        assert!(!state.pi_trusts());
    }

    #[test]
    fn a_decision_is_for_the_grants_shown() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::state::State::open(&dir.path().join("piton.db")).unwrap();
        let now = inputs(dir.path(), &["rust", "swift"], Reconfirm::Never);
        let shown: Grants = [grant("rust")].into();
        assert!(matches!(
            record(&db, &now, Decision::Trusted, &shown),
            Err(DecideError::Changed { .. })
        ));
        assert_eq!(db.trust(dir.path()).unwrap(), None);
        let state = record(&db, &now, Decision::Trusted, &now.grants).unwrap();
        assert!(matches!(state, TrustState::Trusted { .. }));
        assert_eq!(read_state(&dir.path().join("piton.db"), &now).unwrap(), state);
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let t = now();
        assert_eq!(t.len(), 20, "{t}");
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T', "{t}");
    }
}
