//! Conversion helpers between the project-status mapping's open/closed bit
//! and its on-the-wire string form.
//!
//! RFC 0004 collapsed the four-variant `TaskStatus` into a single open/closed
//! bit on [`domain_project::StatusMapping`]. A mapping row now says either
//! "open tasks → this option" or "closed tasks → this option", so the only
//! status key the CLI (`rl project map --status <X>`) and the seed payloads
//! accept is `open` or `closed`.

use crate::error::{Result, ServiceError};

/// Parse the CLI/seed `--status` mapping key into the open/closed bit.
///
/// Accepts exactly `"open"` (→ `true`) and `"closed"` (→ `false`). Anything
/// else — including the old `in_progress` / `blocked` / `done` values — is
/// rejected as an unknown status.
pub(crate) fn parse_status(raw: &str) -> Result<bool> {
    match raw {
        "open" => Ok(true),
        "closed" => Ok(false),
        other => Err(ServiceError::UnknownStatus(other.to_string())),
    }
}

/// Serialize the open/closed bit back to its string form for DTO output.
pub(crate) fn status_to_str(is_open: bool) -> &'static str {
    if is_open { "open" } else { "closed" }
}
