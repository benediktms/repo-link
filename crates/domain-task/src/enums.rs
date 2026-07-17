//! Standalone serde enums with no behaviour.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The task lifecycle axis (RFC 0004 D1): the open/closed bit fused with its
/// GitHub `state_reason` into a single closed set of legal states, so an
/// illegal combination (e.g. "open but completed") is unrepresentable. The
/// old 5-state `TaskStatus` is gone; "Blocked" is no longer a state — it is
/// derived from `blocked_by` relations ([`crate::task::Task::is_blocked`]).
///
/// Decomposes to GitHub's two REST fields at the outbound boundary
/// (`application-sync`): `is_open()` is the `state` bit, and the closed
/// variants carry the `state_reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Open, no reason — the "open since creation" state of a fresh task.
    Open,
    /// Open again after having been closed (the closed→open transition
    /// marker, distinct from `Open`). Maps to GitHub `state_reason = reopened`.
    Reopened,
    /// Closed, work finished as planned. GitHub `state_reason = completed`.
    Completed,
    /// Closed without completing — dropped, deferred, won't-do. GitHub
    /// `state_reason = not_planned`. (The old "archived" notion folds here.)
    NotPlanned,
}

impl Lifecycle {
    /// The open/closed bit (GitHub REST `state`): `Open`/`Reopened` are open,
    /// `Completed`/`NotPlanned` are closed.
    pub fn is_open(self) -> bool {
        matches!(self, Lifecycle::Open | Lifecycle::Reopened)
    }

    /// The GitHub REST `state_reason` string for this lifecycle, or `None` for
    /// a fresh `Open` (open-since-creation carries no reason). The single
    /// canonical source for the reason projection — DTOs and the outbound
    /// mapping derive from this rather than re-listing the strings.
    pub fn state_reason(self) -> Option<&'static str> {
        match self {
            Lifecycle::Open => None,
            Lifecycle::Reopened => Some("reopened"),
            Lifecycle::Completed => Some("completed"),
            Lifecycle::NotPlanned => Some("not_planned"),
        }
    }
}

/// How the local copy of the task relates to its remote counterpart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Never pushed; lives only in the local SQLite store.
    LocalOnly,
    /// Marked for sync, not yet pushed.
    Staged,
    /// Local matches the last known remote snapshot.
    Synced,
    /// Local has uncommitted edits since the last successful sync.
    DirtyLocal,
    /// Remote has changed since the last successful sync.
    DirtyRemote,
    /// Both sides diverged — needs human resolution.
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

/// A task's issue type (RFC 0006 D7): an **extensible, open** classification
/// — the three well-known kinds plus a `Custom(String)` escape hatch for any
/// org-specific type (e.g. `"Epic"`, `"Chore"`). Like `priority` was before
/// its own sync work, this is **local-only metadata** at this stage: GitHub's
/// issue-type rail and org-registry mapping/validation are a follow-up
/// (#228 / RFC §6 Q3), so a change to it must NOT flip sync state.
///
/// ## Why not `#[derive(Serialize, Deserialize)]`
///
/// Unlike `Priority` (all unit variants, so a plain derive with
/// `rename_all = "lowercase"` yields a single JSON string), the data-carrying
/// `Custom(String)` variant would serialize under the default externally-tagged
/// representation as an object (`{"custom":"Epic"}`), not a string. The DB and
/// query helpers (`enum_to_str` / `enum_from_str` / `enum_str`) all require the
/// serialized value to be a single JSON string, so this type instead has a
/// canonical single-string form (`Display`) and manual serde over it.
///
/// ## Round-trip semantics (RFC D7: "unknown → Custom")
///
/// Parsing is infallible and case-insensitive: `"task"` / `"bug"` / `"feature"`
/// (any case) map to the well-known variants, and everything else becomes
/// `Custom(verbatim)`. A degenerate `Custom(builtin)` (e.g. `Custom("Task")`)
/// therefore collapses to the well-known variant on round-trip — acceptable per
/// D7. Strict reject-vs-warn validation against an org registry is deferred to
/// RFC §6 Q3 / #228.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IssueType {
    Task,
    Bug,
    Feature,
    /// Any type outside the well-known set — org-specific (`"Epic"`, `"Chore"`,
    /// …). Held verbatim; the wire/registry projection is #228.
    Custom(String),
}

impl fmt::Display for IssueType {
    /// The canonical single-string form: `task` / `bug` / `feature` for the
    /// well-known variants, and the verbatim payload for `Custom`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueType::Task => f.write_str("task"),
            IssueType::Bug => f.write_str("bug"),
            IssueType::Feature => f.write_str("feature"),
            IssueType::Custom(s) => f.write_str(s),
        }
    }
}

impl IssueType {
    /// Match `s` (any case) to a well-known variant, or `None` for a custom
    /// type. The single source of truth for the string → variant direction,
    /// shared by both `From` impls; must stay the inverse of [`fmt::Display`]
    /// (the `issue_type_well_known_round_trips` test guards that).
    fn well_known(s: &str) -> Option<IssueType> {
        if s.eq_ignore_ascii_case("task") {
            Some(IssueType::Task)
        } else if s.eq_ignore_ascii_case("bug") {
            Some(IssueType::Bug)
        } else if s.eq_ignore_ascii_case("feature") {
            Some(IssueType::Feature)
        } else {
            None
        }
    }
}

impl From<&str> for IssueType {
    /// Infallible, case-insensitive parse (RFC D7: unknown → `Custom`). The
    /// well-known names match regardless of case **without allocating**;
    /// anything else is preserved verbatim as `Custom`.
    fn from(s: &str) -> Self {
        IssueType::well_known(s).unwrap_or_else(|| IssueType::Custom(s.to_string()))
    }
}

impl From<String> for IssueType {
    /// Owned counterpart of [`From<&str>`]: moves the buffer straight into
    /// `Custom` instead of re-allocating it (the deserialize hot path).
    fn from(s: String) -> Self {
        IssueType::well_known(&s).unwrap_or(IssueType::Custom(s))
    }
}

impl Serialize for IssueType {
    /// Serialize as the single canonical string (via [`fmt::Display`]) so the
    /// DB/query helpers that expect a JSON string keep working.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IssueType {
    /// Deserialize from a single string, applying the same infallible
    /// unknown → `Custom` rule as [`IssueType::from`].
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(IssueType::from(s))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    BlockedBy,
    Blocks,
    Duplicates,
    ParentOf,
    ChildOf,
    RelatedTo,
}

impl RelationKind {
    /// The reciprocal edge that should exist on the *other* task so the
    /// relation graph reads coherently from both ends.
    ///
    /// Directional pairs invert (`A blocks B` ⇒ `B blocked_by A`;
    /// `A parent_of B` ⇒ `B child_of A`). Symmetric kinds return
    /// themselves (`A related_to B` ⇒ `B related_to A`; likewise
    /// `duplicates`, treated as a mutual "these are the same work" link).
    ///
    /// Every kind has a reciprocal — there is deliberately no one-directional
    /// kind. (`depends_on` was dropped as a redundant synonym of `blocked_by`;
    /// see migration `…_drop_depends_on_relation`.)
    pub fn inverse(self) -> RelationKind {
        match self {
            RelationKind::BlockedBy => RelationKind::Blocks,
            RelationKind::Blocks => RelationKind::BlockedBy,
            RelationKind::ParentOf => RelationKind::ChildOf,
            RelationKind::ChildOf => RelationKind::ParentOf,
            RelationKind::RelatedTo => RelationKind::RelatedTo,
            RelationKind::Duplicates => RelationKind::Duplicates,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `Lifecycle` variant, for the D1-invariant tests below.
    const ALL_LIFECYCLES: [Lifecycle; 4] = [
        Lifecycle::Open,
        Lifecycle::Reopened,
        Lifecycle::Completed,
        Lifecycle::NotPlanned,
    ];

    /// RFC 0004 D1 invariant — *closed-with-reason*: a closed lifecycle always
    /// projects a `state_reason`. The fused enum makes the inverse
    /// ("closed but no reason") unrepresentable; this locks the projection so a
    /// future variant or `state_reason()` edit can't reintroduce it.
    #[test]
    fn closed_lifecycle_always_has_a_state_reason() {
        for lc in ALL_LIFECYCLES {
            if !lc.is_open() {
                assert!(
                    lc.state_reason().is_some(),
                    "{lc:?} is closed but projects no state_reason"
                );
            }
        }
    }

    /// RFC 0004 D1 invariant — *not-planned-cannot-be-open*: no open lifecycle
    /// projects the `not_planned` reason, and `NotPlanned` itself is closed.
    /// (The old "open but not_planned" 5-state combination is unrepresentable.)
    #[test]
    fn open_lifecycle_is_never_not_planned() {
        assert!(
            !Lifecycle::NotPlanned.is_open(),
            "NotPlanned must be closed"
        );
        for lc in ALL_LIFECYCLES {
            if lc.is_open() {
                assert_ne!(
                    lc.state_reason(),
                    Some("not_planned"),
                    "{lc:?} is open but projects the not_planned reason"
                );
            }
        }
    }

    #[test]
    fn relation_inverse_is_an_involution() {
        // Applying inverse twice returns the original kind for every variant,
        // so a reciprocal edge never drifts from the edge that spawned it.
        for kind in [
            RelationKind::BlockedBy,
            RelationKind::Blocks,
            RelationKind::Duplicates,
            RelationKind::ParentOf,
            RelationKind::ChildOf,
            RelationKind::RelatedTo,
        ] {
            assert_eq!(kind.inverse().inverse(), kind);
        }
    }

    #[test]
    fn directional_pairs_invert_symmetric_kinds_are_self() {
        assert_eq!(RelationKind::BlockedBy.inverse(), RelationKind::Blocks);
        assert_eq!(RelationKind::ParentOf.inverse(), RelationKind::ChildOf);
        assert_eq!(RelationKind::RelatedTo.inverse(), RelationKind::RelatedTo);
        assert_eq!(RelationKind::Duplicates.inverse(), RelationKind::Duplicates);
    }

    /// RFC 0006 D7 — the well-known variants project their canonical lowercase
    /// name through both `Display` and serde, and a `Custom` payload is emitted
    /// verbatim as a single JSON string (never an object).
    #[test]
    fn issue_type_serializes_to_a_single_canonical_string() {
        for (it, want) in [
            (IssueType::Task, "task"),
            (IssueType::Bug, "bug"),
            (IssueType::Feature, "feature"),
            (IssueType::Custom("Epic".into()), "Epic"),
        ] {
            // Display and the serialized JSON string agree.
            assert_eq!(it.to_string(), want, "Display for {it:?}");
            assert_eq!(
                serde_json::to_value(&it).unwrap(),
                serde_json::Value::String(want.to_string()),
                "serde for {it:?} must be the same single string as Display"
            );
        }
    }

    /// RFC 0006 D7 — parsing is infallible and case-insensitive for the
    /// well-known set, and unknown strings become `Custom(verbatim)`.
    #[test]
    fn issue_type_parses_case_insensitively_unknown_to_custom() {
        assert_eq!(IssueType::from("bug"), IssueType::Bug);
        assert_eq!(IssueType::from("Task"), IssueType::Task);
        assert_eq!(IssueType::from("TASK"), IssueType::Task);
        assert_eq!(IssueType::from("Feature"), IssueType::Feature);
        // Unknown → Custom, payload preserved verbatim (original case kept).
        assert_eq!(IssueType::from("Epic"), IssueType::Custom("Epic".into()));
    }

    /// The well-known variants must survive `Display` → `From` unchanged, so
    /// the two independently-maintained maps stay exact inverses — a new
    /// variant added to one but not the other trips this.
    #[test]
    fn issue_type_well_known_round_trips() {
        for v in [IssueType::Task, IssueType::Bug, IssueType::Feature] {
            assert_eq!(IssueType::from(v.to_string().as_str()), v);
        }
    }

    /// Deserialization applies the same unknown → `Custom` rule.
    #[test]
    fn issue_type_deserializes_from_a_single_string() {
        assert_eq!(
            serde_json::from_value::<IssueType>(serde_json::Value::String("bug".into())).unwrap(),
            IssueType::Bug
        );
        assert_eq!(
            serde_json::from_value::<IssueType>(serde_json::Value::String("Epic".into())).unwrap(),
            IssueType::Custom("Epic".into())
        );
    }

    /// Round-trip is lossless for a `Custom` payload outside the well-known
    /// set, and documented-lossy for a `Custom` that spells a built-in
    /// (`Custom("task")` collapses to `Task`) — acceptable per D7.
    #[test]
    fn issue_type_round_trip_is_lossless_except_for_degenerate_custom() {
        let lossless = IssueType::Custom("Epic".into());
        let s = lossless.to_string();
        assert_eq!(IssueType::from(s.as_str()), lossless);

        // Degenerate: a Custom that spells a built-in collapses on round-trip.
        let degenerate = IssueType::Custom("task".into());
        assert_eq!(
            IssueType::from(degenerate.to_string().as_str()),
            IssueType::Task
        );
    }
}
