//! Status field value objects: the option catalog and the
//! local-status → option mapping rows.

use serde::{Deserialize, Serialize};

/// One option on a Project's single-select Status field.
///
/// - `option_id` is GitHub's stable identifier for the option (an 8-char
///   hex prefix like `47fc9ee4`). The mapping below references this value.
/// - `ordinal` is the option's index in the field definition — kept so the
///   CLI can echo the user-facing order from GitHub without re-sorting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOption {
    pub option_id: String,
    pub name: String,
    pub ordinal: u32,
}

/// One row of the local lifecycle → project-option mapping. Keyed on the
/// open/closed bit (RFC 0004 D1): an open task maps to one board option, a
/// closed task to another. Built once at `rl project link` (auto-seeded by
/// name) and editable via `rl project map`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMapping {
    pub is_open: bool,
    pub option_id: String,
}
