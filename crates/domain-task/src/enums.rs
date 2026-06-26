//! Standalone serde enums with no behaviour.

use serde::{Deserialize, Serialize};

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
}
