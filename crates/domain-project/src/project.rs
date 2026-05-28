//! The `Project` aggregate — a mirror of a GitHub Projects v2 board.

use crate::status::{StatusMapping, StatusOption};
use domain_core::{DomainError, ProjectId, Result, Timestamp};
use domain_task::TaskStatus;
use serde::{Deserialize, Serialize};

/// Mirror of a GitHub Projects v2 board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub owner_login: String,
    pub number: u64,
    pub title: String,
    /// The project's Status field's `PVTSSF_…` node ID. Picked at link
    /// time: the field literally named "Status" if present, else the
    /// first single-select field (see RFC 0001 §3 D1).
    pub status_field_id: String,
    pub status_options: Vec<StatusOption>,
    pub status_mappings: Vec<StatusMapping>,
    /// Mirrored from GitHub. Cosmetic only — archiving a remote project
    /// does NOT cascade-archive local workspaces.
    pub archived: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Project {
    /// Construct from a fresh remote fetch. Validates each mapping
    /// references an `option_id` this project owns; empty mappings are
    /// fine (the link flow seeds them by name match before save).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ProjectId,
        owner_login: String,
        number: u64,
        title: String,
        status_field_id: String,
        status_options: Vec<StatusOption>,
        status_mappings: Vec<StatusMapping>,
        archived: bool,
        now: Timestamp,
    ) -> Result<Self> {
        Self::validate_mappings(&status_mappings, &status_options)?;
        Ok(Self {
            id,
            owner_login,
            number,
            title,
            status_field_id,
            status_options,
            status_mappings,
            archived,
            created_at: now,
            updated_at: now,
        })
    }

    /// Replace the mapping wholesale. Same option-ownership invariant as
    /// `new` — callers may not reference an option that isn't in
    /// `status_options`.
    pub fn set_mappings(&mut self, mappings: Vec<StatusMapping>, now: Timestamp) -> Result<()> {
        Self::validate_mappings(&mappings, &self.status_options)?;
        self.status_mappings = mappings;
        self.updated_at = now;
        Ok(())
    }

    /// Refresh the option catalog from the remote (e.g. periodic poll
    /// caught a field change). Drops mapping rows that point at options
    /// that no longer exist — those rebuild on next `project map`.
    pub fn replace_status_options(&mut self, options: Vec<StatusOption>, now: Timestamp) {
        self.status_mappings
            .retain(|m| options.iter().any(|o| o.option_id == m.option_id));
        self.status_options = options;
        self.updated_at = now;
    }

    pub fn option_id_for(&self, status: TaskStatus) -> Option<&str> {
        self.status_mappings
            .iter()
            .find(|m| m.status == status)
            .map(|m| m.option_id.as_str())
    }

    fn validate_mappings(mappings: &[StatusMapping], options: &[StatusOption]) -> Result<()> {
        let mut seen_statuses = std::collections::HashSet::new();
        for m in mappings {
            if !options.iter().any(|o| o.option_id == m.option_id) {
                return Err(DomainError::validation(format!(
                    "status mapping references unknown option_id '{}'",
                    m.option_id
                )));
            }
            // Multiple statuses MAY share one option_id (e.g. Open + Blocked
            // → "Backlog"), but a single status cannot legitimately map to
            // two options — `option_id_for` returns the first match and the
            // result would otherwise depend on insertion order, masking a
            // user error as a sometimes-works lookup.
            if !seen_statuses.insert(m.status) {
                return Err(DomainError::validation(format!(
                    "duplicate status mapping for '{:?}'",
                    m.status
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(id: &str, name: &str, ordinal: u32) -> StatusOption {
        StatusOption {
            option_id: id.into(),
            name: name.into(),
            ordinal,
        }
    }

    fn pid() -> ProjectId {
        ProjectId::parse("PVT_test_abc").unwrap()
    }

    #[test]
    fn new_accepts_empty_mappings() {
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "Backlog", 0)],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert!(p.status_mappings.is_empty());
    }

    #[test]
    fn new_rejects_mapping_to_unknown_option() {
        let err = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "Backlog", 0)],
            vec![StatusMapping {
                status: TaskStatus::Open,
                option_id: "ghost".into(),
            }],
            false,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn option_id_for_returns_the_mapped_value() {
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "Backlog", 0), opt("o2", "Done", 1)],
            vec![
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    status: TaskStatus::Done,
                    option_id: "o2".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(p.option_id_for(TaskStatus::Open), Some("o1"));
        assert_eq!(p.option_id_for(TaskStatus::Done), Some("o2"));
        assert_eq!(p.option_id_for(TaskStatus::InProgress), None);
    }

    #[test]
    fn new_rejects_duplicate_status_mappings() {
        // Same status mapped twice — option_id_for would return the first
        // match and silently mask the user error. Reject at construction.
        let err = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "Backlog", 0), opt("o2", "In Progress", 1)],
            vec![
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o2".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn replace_status_options_drops_now_orphan_mappings() {
        let mut p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "Backlog", 0), opt("o2", "Done", 1)],
            vec![
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    status: TaskStatus::Done,
                    option_id: "o2".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        // GitHub renamed "Backlog" → option "o1b" (new id). The stale
        // mapping to "o1" must be dropped so it doesn't outlive its
        // referent.
        p.replace_status_options(
            vec![opt("o1b", "Backlog", 0), opt("o2", "Done", 1)],
            Timestamp::now(),
        );
        assert_eq!(p.status_mappings.len(), 1);
        assert_eq!(p.status_mappings[0].option_id, "o2");
    }
}
