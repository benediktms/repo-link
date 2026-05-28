//! Conversion helpers between local [`TaskStatus`] values and their
//! on-the-wire string form.

use domain_task::TaskStatus;

use crate::error::{Result, ServiceError};

pub(crate) fn parse_status(raw: &str) -> Result<TaskStatus> {
    // `Archived` is intentionally not accepted: the schema CHECK on
    // `project_status_mappings.status` only allows the four workflow-visible
    // statuses, and an archived task is hidden from sync anyway. Mapping it
    // would never have an effect.
    match raw {
        "open" => Ok(TaskStatus::Open),
        "in_progress" => Ok(TaskStatus::InProgress),
        "blocked" => Ok(TaskStatus::Blocked),
        "done" => Ok(TaskStatus::Done),
        other => Err(ServiceError::UnknownStatus(other.to_string())),
    }
}

pub(crate) fn status_to_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Open => "open",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        // Domain permits the variant but the schema's CHECK rejects it on
        // save. We never construct a mapping carrying it (see `parse_status`)
        // so reaching this arm signals a corrupt local mapping — surface
        // it as the literal so the load-time `Project::new` validator can
        // bubble it as a domain error rather than a panic.
        TaskStatus::Archived => "archived",
    }
}
