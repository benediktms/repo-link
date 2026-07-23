//! Generalized project field value objects (RFC 0006 D2 + D9).
//!
//! A GitHub Projects v2 board can expose many single-select fields (Status,
//! Priority, Type, …). Historically repo-link modelled only the one it drives
//! for lifecycle — "Status" — as a bare `status_field_id` + option catalog. The
//! generalized model keeps the *whole* set of retained single-select fields as
//! a `Vec<ProjectField>`, each tagged with a [`ProjectFieldKind`] classified by
//! name at link time. Status resolution keys on `kind == Status`, never on the
//! field's literal name, so a board whose lifecycle field is named "Stage"
//! still works.

use domain_core::{DomainError, Result};
use serde::{Deserialize, Serialize};

/// One option on a Project single-select field.
///
/// - `option_id` is GitHub's stable identifier for the option (an 8-char hex
///   prefix like `47fc9ee4`). The status mapping references this value.
/// - `ordinal` is the option's index in the field definition — kept so the CLI
///   can echo the user-facing order from GitHub without re-sorting.
///
/// Renamed from the old `StatusOption`: options are no longer status-specific
/// now that every retained single-select field carries its own catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldOption {
    pub option_id: String,
    pub name: String,
    pub ordinal: u32,
}

/// How repo-link treats a retained single-select field, decided by name at
/// link time (RFC 0006 D9). `Status` drives lifecycle; `Priority` drives the
/// ordinal priority projection (#225); `Type` drives the custom issue-type
/// projection on user-owned boards (#238), resolved by case-insensitive option
/// name. `Other` is retained (persisted + loaded) so the data round-trips but
/// no mapping logic consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectFieldKind {
    Status,
    Priority,
    Type,
    Other,
}

impl ProjectFieldKind {
    /// The persisted discriminator string (matches the migration's CHECK).
    pub fn as_db_str(&self) -> &'static str {
        match self {
            ProjectFieldKind::Status => "status",
            ProjectFieldKind::Priority => "priority",
            ProjectFieldKind::Type => "type",
            ProjectFieldKind::Other => "other",
        }
    }

    /// Parse the persisted discriminator; an unknown value is a corrupted row
    /// and surfaces as a typed error rather than a silent default.
    pub fn from_db_str(s: &str) -> Result<Self> {
        match s {
            "status" => Ok(ProjectFieldKind::Status),
            "priority" => Ok(ProjectFieldKind::Priority),
            "type" => Ok(ProjectFieldKind::Type),
            "other" => Ok(ProjectFieldKind::Other),
            other => Err(DomainError::validation(format!(
                "unknown project field kind '{other}'"
            ))),
        }
    }
}

/// One retained single-select field on a project: its GitHub field id, name,
/// classified [`ProjectFieldKind`], and option catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectField {
    pub field_id: String,
    pub name: String,
    pub kind: ProjectFieldKind,
    pub options: Vec<FieldOption>,
}

impl ProjectField {
    /// Construct a field with `kind` defaulting to [`ProjectFieldKind::Other`].
    /// Callers reach for [`assign_field_kinds`] to classify a whole retained
    /// set rather than setting `kind` per field by hand.
    pub fn new(field_id: String, name: String, options: Vec<FieldOption>) -> Self {
        Self {
            field_id,
            name,
            kind: ProjectFieldKind::Other,
            options,
        }
    }
}

/// Classify a retained set of single-select fields by name (RFC 0006 D9),
/// preserving the RFC 0001 D1 Status-selection rule:
///
/// - **Status** — the field literally named `"Status"`; if none is, the FIRST
///   field (the historical fallback for a board whose lifecycle field is named
///   something else). At most one field is ever tagged `Status`.
/// - **Priority** — a (non-Status) field named `"Priority"`.
/// - **Type** — a (non-Status) field named `"Type"` or `"Types"`,
///   case-insensitively (RFC 0006 #238). Unlike Status/Priority this may tag
///   MORE than one field (a board with both a "Type" and a "Types" single-select
///   is legal); ambiguity is resolved lazily at projection time — `Project`
///   surfaces the custom Type field only when exactly one is tagged (see
///   [`crate::Project::type_field`]) rather than rejecting the board at
///   construction, so lifecycle/priority still work.
/// - **Other** — everything else.
///
/// An empty input yields an empty set with no Status field (the caller — e.g.
/// `link_from_snapshot` — surfaces the "no single-select to use as Status"
/// error).
pub fn assign_field_kinds(mut fields: Vec<ProjectField>) -> Vec<ProjectField> {
    // The Status field: prefer the one literally named "Status", else fall
    // back to the first field (RFC 0001 D1). `position` on an empty slice is
    // `None`, so an empty board classifies nothing.
    let status_idx = fields
        .iter()
        .position(|f| f.name == "Status")
        .or(if fields.is_empty() { None } else { Some(0) });

    for (i, field) in fields.iter_mut().enumerate() {
        field.kind = if Some(i) == status_idx {
            ProjectFieldKind::Status
        } else if field.name == "Priority" {
            ProjectFieldKind::Priority
        } else if field.name.eq_ignore_ascii_case("type")
            || field.name.eq_ignore_ascii_case("types")
        {
            ProjectFieldKind::Type
        } else {
            ProjectFieldKind::Other
        };
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(id: &str, name: &str) -> ProjectField {
        ProjectField::new(
            id.into(),
            name.into(),
            vec![FieldOption {
                option_id: format!("{id}-o0"),
                name: "opt".into(),
                ordinal: 0,
            }],
        )
    }

    #[test]
    fn picks_field_named_status_over_earlier_priority() {
        let fields =
            assign_field_kinds(vec![field("f_prio", "Priority"), field("f_stat", "Status")]);
        assert_eq!(fields[0].kind, ProjectFieldKind::Priority);
        assert_eq!(fields[1].kind, ProjectFieldKind::Status);
    }

    #[test]
    fn falls_back_to_first_field_when_none_named_status() {
        // RFC 0001 D1 fallback: no "Status" field → the first single-select is
        // treated as Status (here a "Stage" field).
        let fields = assign_field_kinds(vec![field("f_stage", "Stage"), field("f_other", "Notes")]);
        assert_eq!(fields[0].kind, ProjectFieldKind::Status);
        assert_eq!(fields[1].kind, ProjectFieldKind::Other);
    }

    #[test]
    fn tags_priority_type_and_leaves_the_rest_other() {
        let fields = assign_field_kinds(vec![
            field("f_stat", "Status"),
            field("f_prio", "Priority"),
            field("f_type", "Type"),
            field("f_notes", "Notes"),
        ]);
        assert_eq!(fields[0].kind, ProjectFieldKind::Status);
        assert_eq!(fields[1].kind, ProjectFieldKind::Priority);
        assert_eq!(fields[2].kind, ProjectFieldKind::Type);
        assert_eq!(fields[3].kind, ProjectFieldKind::Other);
    }

    #[test]
    fn tags_type_and_types_case_insensitively() {
        // RFC 0006 #238 decision 1: "Type" or "Types", case-insensitive. A
        // board legitimately carrying both variants tags BOTH — the ambiguity
        // is resolved at projection time, not here.
        let fields = assign_field_kinds(vec![
            field("f_stat", "Status"),
            field("f_type", "type"),
            field("f_types", "TYPES"),
        ]);
        assert_eq!(fields[1].kind, ProjectFieldKind::Type);
        assert_eq!(fields[2].kind, ProjectFieldKind::Type);
        assert_eq!(
            fields
                .iter()
                .filter(|f| f.kind == ProjectFieldKind::Type)
                .count(),
            2
        );
    }

    #[test]
    fn empty_input_yields_no_status_field() {
        let fields = assign_field_kinds(vec![]);
        assert!(fields.is_empty());
        assert!(!fields.iter().any(|f| f.kind == ProjectFieldKind::Status));
    }

    #[test]
    fn kind_db_str_round_trips_and_rejects_unknown() {
        for kind in [
            ProjectFieldKind::Status,
            ProjectFieldKind::Priority,
            ProjectFieldKind::Type,
            ProjectFieldKind::Other,
        ] {
            assert_eq!(
                ProjectFieldKind::from_db_str(kind.as_db_str()).unwrap(),
                kind
            );
        }
        assert!(ProjectFieldKind::from_db_str("nope").is_err());
    }
}
