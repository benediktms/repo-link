//! The local-status → project-option mapping row.
//!
//! The option catalog itself moved to [`crate::field`] as the generic
//! [`crate::FieldOption`] once every retained single-select field grew its own
//! catalog (RFC 0006 D2). The mapping below stays here: it is lifecycle-specific
//! and does not generalize.

use serde::{Deserialize, Serialize};

/// One row of the local lifecycle → project-option mapping. Keyed on the
/// open/closed bit (RFC 0004 D1): an open task maps to one board option, a
/// closed task to another. Built once at `rl project link` (auto-seeded by
/// name) and editable via `rl project map`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMapping {
    pub is_open: bool,
    pub option_id: String,
}
