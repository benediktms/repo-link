//! domain-task — Task aggregate, lifecycle + sync state machines, relations.
//!
//! Lifecycle and sync are **orthogonal**: a task can be `open + DirtyLocal`
//! or `closed + Synced`. The lifecycle axis is a single [`Lifecycle`] value
//! (`Open | Reopened | Completed | NotPlanned`, RFC 0004 D1), whose
//! [`Lifecycle::is_open`] and [`Lifecycle::state_reason`] derive the REST
//! `(closed, state_reason)` pair; the sync axis is [`SyncState`]. They live
//! side-by-side on `Task` so the sync engine can
//! reconcile without first asking whether a task is alive, and "blocked" is
//! derived from `blocked_by` relations ([`Task::is_blocked`]) rather than
//! being a lifecycle state.

mod enums;
mod hash;
mod relation;
mod snapshot;
mod task;

pub use enums::{Lifecycle, Priority, RelationKind, SyncState};
pub use hash::{MAX_HASH_LEN, MIN_HASH_LEN, is_valid_hash, random_lowercase_base32};
pub use relation::{RemoteRef, TaskComment, TaskRelation};
pub use snapshot::{SnapshotSource, TaskSnapshot};
pub use task::{MIRRORED_FIELDS, MirrorField, MirrorPatch, Task, assignees_equal};
