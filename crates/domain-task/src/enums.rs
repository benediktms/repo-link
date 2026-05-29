//! Standalone serde enums with no behaviour.

use serde::{Deserialize, Serialize};

/// Where the task is in the human workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created but no one has started it.
    Open,
    /// Actively being worked on.
    InProgress,
    /// Stuck on an external dependency.
    Blocked,
    /// Work is complete. Distinct from `Archived` — done tasks stay
    /// visible in dashboards; archived ones are out of sight.
    Done,
    /// Terminal — dropped, deferred indefinitely, or post-done cleanup.
    Archived,
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
