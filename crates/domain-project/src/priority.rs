//! The local-priority → project-option mapping row (RFC 0006 D3).
//!
//! Sibling of [`crate::StatusMapping`]: where the status mapping keys on the
//! open/closed bit, this keys on the local [`Priority`] enum (`P0..P3`). The
//! rows are DERIVED by ordinal at `rl project link`
//! ([`crate::derive_priority_mappings`]) and overridable by hand — the
//! derivation is a cache, not the source of truth. The outbound projection that
//! reads these to set a board's Priority single-select is a follow-up (#225);
//! they are persisted + loaded here so the derivation is a genuine round trip.

use domain_task::Priority;
use serde::{Deserialize, Serialize};

/// One row of the local `Priority` → project-option mapping, keyed on the
/// local `P0..P3` enum (RFC 0006 D3). `option_id` references an option owned by
/// the project's `Priority`-kind field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityMapping {
    pub priority: Priority,
    pub option_id: String,
}
