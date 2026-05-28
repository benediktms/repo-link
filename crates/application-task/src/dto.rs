//! DTO mapping for tasks: enum (de)serialization helpers, composite
//! display-ID assembly, and the pure `Task` → `TaskDto` conversion.

use domain_task::Task;
use dto_shared::{RemoteRefDto, TaskCommentDto, TaskDto, TaskRelationDto};
use serde::de::DeserializeOwned;

use crate::error::{Result, ServiceError};

fn enum_str<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

pub(crate) fn parse_enum<T: DeserializeOwned>(field: &'static str, value: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).map_err(|_| {
        ServiceError::BadEnum {
            field,
            value: value.to_string(),
        }
    })
}

/// Assemble the user-visible composite `id` for a task DTO.
///
/// Rules (in priority order):
/// 1. Non-empty hash + non-empty prefix → `"{prefix}-{hash}"`.
/// 2. Non-empty hash + empty/None prefix → bare `"{hash}"`. (Task
///    has no repo binding, e.g. workspace-scoped or pre-attach.)
/// 3. Empty hash → UUID (transition fallback for legacy rows the
///    backfill hasn't reached yet; rare and short-lived in practice).
pub(crate) fn assemble_task_display_id(t: &Task, prefix: Option<&str>) -> String {
    if !t.hash.is_empty() {
        match prefix.filter(|p| !p.is_empty()) {
            Some(p) => format!("{}-{}", p, t.hash),
            None => t.hash.clone(),
        }
    } else {
        t.id.to_string()
    }
}

pub fn task_to_dto(t: &Task, prefix: Option<&str>) -> TaskDto {
    TaskDto {
        id: assemble_task_display_id(t, prefix),
        workspace_id: t.workspace_id.to_string(),
        repo_id: t.repo_id.map(|r| r.to_string()),
        title: t.title.clone(),
        body: t.body.clone(),
        status: enum_str(&t.status),
        sync_state: enum_str(&t.sync),
        priority: enum_str(&t.priority),
        assignees: t.assignees.clone(),
        remote: t.remote.as_ref().map(|r| RemoteRefDto {
            provider: r.provider.clone(),
            remote_id: r.remote_id.clone(),
        }),
        relations: t
            .relations
            .iter()
            .map(|r| TaskRelationDto {
                kind: enum_str(&r.kind),
                other: r.other.to_string(),
            })
            .collect(),
        comments: t
            .comments
            .iter()
            .map(|c| TaskCommentDto {
                remote_id: c.remote_id.clone(),
                author: c.author.clone(),
                body: c.body.clone(),
                created_at: c.created_at.into(),
            })
            .collect(),
        created_at: t.created_at.into(),
        updated_at: t.updated_at.into(),
    }
}
