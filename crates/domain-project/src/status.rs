//! Status field value objects: the option catalog and the
//! local-status → option mapping rows.

use domain_task::TaskStatus;
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

/// One row of the local-status → project-option mapping. Built once at
/// `rl project link` time (auto-seeded by name match) and editable via
/// `rl project map`. Multiple `TaskStatus` values may legitimately map to
/// the same `option_id` (e.g. `Open` and `Blocked` both → "Backlog").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusMapping {
    pub status: TaskStatus,
    pub option_id: String,
}
