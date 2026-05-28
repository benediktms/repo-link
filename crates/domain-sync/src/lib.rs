//! domain-sync — pure reconciliation rules over [`domain_task::SyncState`]
//! plus the outbox value types. No I/O.

mod outbox;
mod policy;

pub use outbox::{OutboxEntry, OutboxMutation, OutboxStatus};
pub use policy::{ConflictKind, SyncDecision, SyncPolicy, decide};
