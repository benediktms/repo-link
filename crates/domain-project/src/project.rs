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

    /// The canonical local-status → project-option resolver, applying the
    /// RFC §3 absence-of-row rule: a `Blocked` task on a board with no
    /// Blocked-like option (so no `blocked` mapping row was stored) resolves
    /// to the `Open` option. Returns `None` only when even `Open` is unmapped
    /// (an option-less board) — or for `Archived`, which is never mapped to a
    /// project option (REST `close as not_planned` handles it).
    ///
    /// This is the single definition of the fallback shared by the outbox
    /// enqueue/drain paths (via `application_sync::option_id_for_status_with_fallback`,
    /// which delegates here) AND Stage 8 drift detection, so the "what option
    /// does this status map to?" question has exactly one answer everywhere.
    pub fn resolved_option_id_for(&self, status: TaskStatus) -> Option<&str> {
        if let Some(opt) = self.option_id_for(status) {
            return Some(opt);
        }
        if status == TaskStatus::Blocked {
            // No row for Blocked ⇒ resolve to the Open option (app-level
            // fallback; never stored as a row — see RFC §3).
            return self.option_id_for(TaskStatus::Open);
        }
        None
    }

    /// Resolve a cached `option_id` to its human-readable display name (e.g.
    /// `"In progress"`). `None` when the project doesn't own that option —
    /// e.g. a stale cached id whose option was renamed/removed on GitHub.
    /// Used by drift + `rl task show` to render the cached/expected board
    /// status as a name rather than an opaque id.
    pub fn option_name_for(&self, option_id: &str) -> Option<&str> {
        self.status_options
            .iter()
            .find(|o| o.option_id == option_id)
            .map(|o| o.name.as_str())
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
    fn resolved_option_id_for_normal_mapping_and_blocked_fallback() {
        // Open + Done are mapped; Blocked is intentionally NOT mapped (no
        // Blocked-like option on the board), so it must resolve to the Open
        // option via the §3 fallback — not None.
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o_open", "Backlog", 0), opt("o_done", "Done", 1)],
            vec![
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o_open".into(),
                },
                StatusMapping {
                    status: TaskStatus::Done,
                    option_id: "o_done".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        // Normal mapping resolves to its own option.
        assert_eq!(p.resolved_option_id_for(TaskStatus::Done), Some("o_done"));
        // Blocked has no row → falls back to the Open option.
        assert_eq!(
            p.resolved_option_id_for(TaskStatus::Blocked),
            Some("o_open")
        );
        // InProgress has no row and no fallback → None.
        assert_eq!(p.resolved_option_id_for(TaskStatus::InProgress), None);
        // Archived is never mapped to a project option.
        assert_eq!(p.resolved_option_id_for(TaskStatus::Archived), None);
    }

    #[test]
    fn resolved_option_id_for_blocked_with_explicit_mapping_uses_it() {
        // When a Blocked row *is* stored, it wins over the Open fallback.
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o_open", "Backlog", 0), opt("o_block", "Blocked", 1)],
            vec![
                StatusMapping {
                    status: TaskStatus::Open,
                    option_id: "o_open".into(),
                },
                StatusMapping {
                    status: TaskStatus::Blocked,
                    option_id: "o_block".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(
            p.resolved_option_id_for(TaskStatus::Blocked),
            Some("o_block")
        );
    }

    #[test]
    fn option_name_for_hit_and_miss() {
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "In progress", 0)],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(p.option_name_for("o1"), Some("In progress"));
        // An id the project doesn't own (e.g. renamed/removed remotely) → None.
        assert_eq!(p.option_name_for("ghost"), None);
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
