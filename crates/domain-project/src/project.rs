//! The `Project` aggregate — a mirror of a GitHub Projects v2 board.

use crate::field::{FieldOption, ProjectField, ProjectFieldKind};
use crate::status::StatusMapping;
use domain_core::{DomainError, ProjectId, Result, Timestamp};
use serde::{Deserialize, Serialize};

/// Mirror of a GitHub Projects v2 board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub owner_login: String,
    pub number: u64,
    pub title: String,
    /// Every retained single-select field on the board (RFC 0006 D2), each
    /// tagged with its [`ProjectFieldKind`]. Lifecycle keys on the `Status`
    /// field (`kind == Status`), resolved via [`Self::status_field`] rather
    /// than by the field's literal name. At most one field is `Status`.
    pub fields: Vec<ProjectField>,
    pub status_mappings: Vec<StatusMapping>,
    /// Mirrored from GitHub. Cosmetic only — archiving a remote project
    /// does NOT cascade-archive local workspaces.
    pub archived: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Project {
    /// Generalized constructor over a keyed set of fields (RFC 0006 D2).
    /// Validates that:
    /// - at most one field is classified `Status`;
    /// - a `Status` field exists when `fields` is non-empty;
    /// - every mapping references an `option_id` owned by the `Status` field
    ///   (empty mappings are fine — the link flow seeds them by name match).
    #[allow(clippy::too_many_arguments)]
    pub fn from_fields(
        id: ProjectId,
        owner_login: String,
        number: u64,
        title: String,
        fields: Vec<ProjectField>,
        status_mappings: Vec<StatusMapping>,
        archived: bool,
        now: Timestamp,
    ) -> Result<Self> {
        let status_count = fields
            .iter()
            .filter(|f| f.kind == ProjectFieldKind::Status)
            .count();
        if status_count > 1 {
            return Err(DomainError::validation(format!(
                "project has {status_count} Status fields; expected at most one"
            )));
        }
        if !fields.is_empty() && status_count == 0 {
            return Err(DomainError::validation(
                "project has fields but none is classified as Status".to_string(),
            ));
        }
        let status_options = fields
            .iter()
            .find(|f| f.kind == ProjectFieldKind::Status)
            .map(|f| f.options.as_slice())
            .unwrap_or_default();
        Self::validate_mappings(&status_mappings, status_options)?;
        Ok(Self {
            id,
            owner_login,
            number,
            title,
            fields,
            status_mappings,
            archived,
            created_at: now,
            updated_at: now,
        })
    }

    /// Single-Status convenience over [`Self::from_fields`]: wraps the given
    /// Status field id + option catalog into one `Status`-kind [`ProjectField`]
    /// named "Status". Used by the hand-entered `rl project link` path and by
    /// tests; the GraphQL-fetched path uses `from_fields` with the retained set.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ProjectId,
        owner_login: String,
        number: u64,
        title: String,
        status_field_id: String,
        status_options: Vec<FieldOption>,
        status_mappings: Vec<StatusMapping>,
        archived: bool,
        now: Timestamp,
    ) -> Result<Self> {
        let fields = vec![ProjectField {
            field_id: status_field_id,
            name: "Status".to_string(),
            kind: ProjectFieldKind::Status,
            options: status_options,
        }];
        Self::from_fields(
            id,
            owner_login,
            number,
            title,
            fields,
            status_mappings,
            archived,
            now,
        )
    }

    /// The project's `Status`-kind field, if any. `None` for a fieldless
    /// project (never happens for a successfully-linked board, but the load
    /// path can't assume it).
    pub fn status_field(&self) -> Option<&ProjectField> {
        self.fields
            .iter()
            .find(|f| f.kind == ProjectFieldKind::Status)
    }

    /// The Status field's `PVTSSF_…` node id, if the project has a Status field.
    pub fn status_field_id(&self) -> Option<&str> {
        self.status_field().map(|f| f.field_id.as_str())
    }

    /// The Status field's option catalog (empty slice when there is no Status
    /// field).
    pub fn status_options(&self) -> &[FieldOption] {
        self.status_field()
            .map(|f| f.options.as_slice())
            .unwrap_or(&[])
    }

    /// Replace the mapping wholesale. Same option-ownership invariant as
    /// `new` — callers may not reference an option that isn't in the Status
    /// field's catalog.
    pub fn set_mappings(&mut self, mappings: Vec<StatusMapping>, now: Timestamp) -> Result<()> {
        Self::validate_mappings(&mappings, self.status_options())?;
        self.status_mappings = mappings;
        self.updated_at = now;
        Ok(())
    }

    /// Refresh the Status field's option catalog from the remote (e.g. a
    /// periodic poll caught a field change). Swaps the options in place and
    /// drops mapping rows that point at options that no longer exist — those
    /// rebuild on next `project map`.
    pub fn replace_status_options(&mut self, options: Vec<FieldOption>, now: Timestamp) {
        self.status_mappings
            .retain(|m| options.iter().any(|o| o.option_id == m.option_id));
        if let Some(field) = self
            .fields
            .iter_mut()
            .find(|f| f.kind == ProjectFieldKind::Status)
        {
            field.options = options;
        }
        self.updated_at = now;
    }

    pub fn option_id_for(&self, is_open: bool) -> Option<&str> {
        self.status_mappings
            .iter()
            .find(|m| m.is_open == is_open)
            .map(|m| m.option_id.as_str())
    }

    /// The canonical local-lifecycle → project-option resolver. Keyed on the
    /// open/closed bit (RFC 0004 D1): an open task maps to one board option, a
    /// closed task to another. Returns `None` when that bit is unmapped (e.g.
    /// an option-less board).
    ///
    /// "Blocked" is no longer a status — it became a relation (RFC 0004 D1) —
    /// so the old `Blocked → Open` fallback branch is gone; this method now
    /// simply delegates to `option_id_for`. It remains the single canonical
    /// resolver shared by the outbox enqueue/drain paths AND Stage 8 drift
    /// detection, so the "what option does this lifecycle bit map to?" question
    /// has exactly one answer everywhere.
    pub fn resolved_option_id_for(&self, is_open: bool) -> Option<&str> {
        self.option_id_for(is_open)
    }

    /// Resolve a cached `option_id` to its human-readable display name (e.g.
    /// `"In progress"`). `None` when the project doesn't own that option —
    /// e.g. a stale cached id whose option was renamed/removed on GitHub.
    /// Used by drift + `rl task show` to render the cached/expected board
    /// status as a name rather than an opaque id.
    pub fn option_name_for(&self, option_id: &str) -> Option<&str> {
        self.status_options()
            .iter()
            .find(|o| o.option_id == option_id)
            .map(|o| o.name.as_str())
    }

    fn validate_mappings(mappings: &[StatusMapping], options: &[FieldOption]) -> Result<()> {
        let mut seen_bits = std::collections::HashSet::new();
        for m in mappings {
            if !options.iter().any(|o| o.option_id == m.option_id) {
                return Err(DomainError::validation(format!(
                    "status mapping references unknown option_id '{}'",
                    m.option_id
                )));
            }
            // A single open/closed value cannot legitimately map to two
            // options — `option_id_for` returns the first match and the
            // result would otherwise depend on insertion order, masking a
            // user error as a sometimes-works lookup.
            if !seen_bits.insert(m.is_open) {
                return Err(DomainError::validation(format!(
                    "duplicate status mapping for is_open={}",
                    m.is_open
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(id: &str, name: &str, ordinal: u32) -> FieldOption {
        FieldOption {
            option_id: id.into(),
            name: name.into(),
            ordinal,
        }
    }

    fn pid() -> ProjectId {
        ProjectId::parse("PVT_test_abc").unwrap()
    }

    fn status_field(field_id: &str, options: Vec<FieldOption>) -> ProjectField {
        ProjectField {
            field_id: field_id.into(),
            name: "Status".into(),
            kind: ProjectFieldKind::Status,
            options,
        }
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
                is_open: true,
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
                    is_open: true,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    is_open: false,
                    option_id: "o2".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(p.option_id_for(true), Some("o1"));
        assert_eq!(p.option_id_for(false), Some("o2"));
    }

    #[test]
    fn resolved_option_id_for_open_and_closed() {
        // Both lifecycle buckets are mapped; the resolver returns each
        // bucket's own option. With a bucket unmapped, it returns None
        // (the old Blocked→Open fallback is gone — RFC 0004 D1).
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o_open", "Backlog", 0), opt("o_done", "Done", 1)],
            vec![
                StatusMapping {
                    is_open: true,
                    option_id: "o_open".into(),
                },
                StatusMapping {
                    is_open: false,
                    option_id: "o_done".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(p.resolved_option_id_for(true), Some("o_open"));
        assert_eq!(p.resolved_option_id_for(false), Some("o_done"));
    }

    #[test]
    fn resolved_option_id_for_unmapped_bucket_is_none() {
        // Only the open bucket is mapped → resolving the closed bucket is None
        // (no fallback). An option-less board likewise yields None for open.
        let p = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o_open", "Backlog", 0)],
            vec![StatusMapping {
                is_open: true,
                option_id: "o_open".into(),
            }],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(p.resolved_option_id_for(true), Some("o_open"));
        assert_eq!(p.resolved_option_id_for(false), None);
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
        // Same open/closed bit mapped twice — option_id_for would return the
        // first match and silently mask the user error. Reject at construction.
        let err = Project::new(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            "PVTSSF_field".into(),
            vec![opt("o1", "Backlog", 0), opt("o2", "In Progress", 1)],
            vec![
                StatusMapping {
                    is_open: true,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    is_open: true,
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
                    is_open: true,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    is_open: false,
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

    // ---------- generalized-field constructor + accessors ------------------

    fn priority_field() -> ProjectField {
        ProjectField {
            field_id: "PVTSSF_prio".into(),
            name: "Priority".into(),
            kind: ProjectFieldKind::Priority,
            options: vec![opt("p0", "P0", 0), opt("p1", "P1", 1)],
        }
    }

    #[test]
    fn from_fields_rejects_more_than_one_status_field() {
        let err = Project::from_fields(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            vec![
                status_field("PVTSSF_a", vec![opt("o1", "Backlog", 0)]),
                status_field("PVTSSF_b", vec![opt("o2", "Done", 0)]),
            ],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn from_fields_rejects_mapping_to_option_not_owned_by_status_field() {
        // The mapped option lives on the Priority field, not the Status field —
        // mappings resolve off Status only, so this must be rejected.
        let err = Project::from_fields(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            vec![
                status_field("PVTSSF_s", vec![opt("o1", "Backlog", 0)]),
                priority_field(),
            ],
            vec![StatusMapping {
                is_open: true,
                option_id: "p0".into(),
            }],
            false,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn from_fields_accepts_status_field_with_empty_options() {
        let p = Project::from_fields(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            vec![status_field("PVTSSF_s", vec![])],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert!(p.status_options().is_empty());
        assert_eq!(p.status_field_id(), Some("PVTSSF_s"));
    }

    #[test]
    fn from_fields_rejects_fields_without_a_status_field() {
        // A non-empty field set with no Status classification is invalid — the
        // link flow must always identify a Status field.
        let err = Project::from_fields(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            vec![priority_field()],
            vec![],
            false,
            Timestamp::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn accessors_read_the_status_field_only_in_a_multi_field_project() {
        // Status + Priority. Every resolver reads the Status field's data and
        // ignores the Priority field entirely.
        let p = Project::from_fields(
            pid(),
            "acme".into(),
            7,
            "Repo Link".into(),
            vec![
                priority_field(),
                status_field(
                    "PVTSSF_s",
                    vec![opt("o1", "Backlog", 0), opt("o2", "Done", 1)],
                ),
            ],
            vec![
                StatusMapping {
                    is_open: true,
                    option_id: "o1".into(),
                },
                StatusMapping {
                    is_open: false,
                    option_id: "o2".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap();
        assert_eq!(p.status_field_id(), Some("PVTSSF_s"));
        assert_eq!(p.status_field().map(|f| f.name.as_str()), Some("Status"));
        assert_eq!(p.status_options().len(), 2);
        assert_eq!(p.option_id_for(true), Some("o1"));
        assert_eq!(p.resolved_option_id_for(false), Some("o2"));
        assert_eq!(p.option_name_for("o2"), Some("Done"));
        // The Priority option is not visible through the Status accessors.
        assert_eq!(p.option_name_for("p0"), None);
        // The Priority field itself is retained on the aggregate.
        assert!(
            p.fields
                .iter()
                .any(|f| f.kind == ProjectFieldKind::Priority && f.name == "Priority")
        );
    }
}
