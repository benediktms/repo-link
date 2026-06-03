//! domain-task — Task aggregate, lifecycle + sync state machines, relations.
//!
//! Lifecycle and sync are **orthogonal**: a task can be `Open + DirtyLocal`
//! or `Blocked + Synced`. The two enums live side-by-side on `Task` so the
//! sync engine can reconcile without first asking whether a task is alive,
//! and the planning UI can filter blockers without caring about remote
//! drift.

mod enums;
mod hash;
mod relation;
mod snapshot;
mod task;

pub use enums::{Priority, RelationKind, SyncState, TaskStatus};
pub use hash::{MAX_HASH_LEN, MIN_HASH_LEN, is_valid_hash, random_lowercase_base32};
pub use relation::{RemoteRef, TaskComment, TaskRelation};
pub use snapshot::{SnapshotSource, TaskSnapshot};
pub use task::{MIRRORED_FIELDS, MirrorField, MirrorPatch, Task, assignees_equal};
