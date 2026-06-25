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
        is_open: t.is_open(),
        // Canonical reason projection lives on the enum (RFC 0004 D1).
        state_reason: t.lifecycle.state_reason().map(str::to_string),
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
        // The cached board status is resolved to a display name in
        // `TaskService::task_dto` (it needs a project handle this pure fn
        // doesn't have). Default to None here so the pure conversion stays
        // network- and repo-free. CACHED only — never a network call.
        project_status: None,
        // The `blocked_by` UUIDs are upgraded to composite display IDs in
        // `TaskService::task_dto` (same as `relations`), which has the binding
        // handle this pure fn lacks. Seed with UUIDs here.
        blocked_by: t.blocked_by().map(|id| id.to_string()).collect(),
        // Freshness is a mirror-only annotation (RFC 0004 D2): a purely-local
        // task has no remote to refresh, so the field stays absent. The
        // wall-clock `synced_at` is the display value (deltas use the monotonic
        // companion, not surfaced here).
        last_refreshed_at: t.is_mirror().then(|| t.synced_at.map(Into::into)).flatten(),
        created_at: t.created_at.into(),
        updated_at: t.updated_at.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
    use domain_task::{RelationKind, RemoteRef};
    use dto_shared::{CreateTaskCmd, ImportMirrorCmd, UpdateTaskCmd};

    /// `last_refreshed_at` is a mirror-only annotation (RFC 0004 D2): a
    /// purely-local task never carries it, even with a `synced_at` set; a
    /// mirrored task surfaces its `synced_at` as the wall-clock value.
    #[test]
    fn last_refreshed_at_is_mirror_only() {
        let mut t = Task::new_draft(WorkspaceId::new(), None, "t".into()).unwrap();
        // LocalOnly with a synced_at set: the freshness gate must drop it.
        t.synced_at = Some(Timestamp::now());
        assert!(
            task_to_dto(&t, None).last_refreshed_at.is_none(),
            "a local-only task must not surface a last_refreshed_at"
        );

        // Promote to a mirror (Synced); now the synced_at is surfaced.
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "1")).unwrap();
        t.synced_at = Some(Timestamp::now());
        assert!(
            task_to_dto(&t, None).last_refreshed_at.is_some(),
            "a mirrored task must surface its synced_at as last_refreshed_at"
        );

        // A mirror that has never been observed: still no value.
        t.synced_at = None;
        assert!(task_to_dto(&t, None).last_refreshed_at.is_none());

        // Serialization contract: when there's no timestamp the key is OMITTED
        // (not `null`) — a local task and a never-observed mirror both drop it.
        let local = Task::new_draft(WorkspaceId::new(), None, "t".into()).unwrap();
        let json = serde_json::to_value(task_to_dto(&local, None)).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("last_refreshed_at"),
            "last_refreshed_at must be omitted, not null, when there is no observed timestamp"
        );
        // …and present once observed.
        t.synced_at = Some(Timestamp::now());
        let json = serde_json::to_value(task_to_dto(&t, None)).unwrap();
        assert!(json.as_object().unwrap().contains_key("last_refreshed_at"));
    }

    /// The flat `blocked_by` list is derived from the `blocked_by` relation
    /// edges. The pure conversion seeds UUIDs (composite IDs are layered on in
    /// the service); here we assert the derivation, not the composite upgrade.
    #[test]
    fn blocked_by_is_derived_from_relations() {
        let mut t = Task::new_draft(WorkspaceId::new(), None, "t".into()).unwrap();
        assert!(task_to_dto(&t, None).blocked_by.is_empty());

        let blocker = TaskId::new();
        t.add_relation(RelationKind::BlockedBy, blocker);
        assert_eq!(
            task_to_dto(&t, None).blocked_by,
            vec![blocker.to_string()],
            "blocked_by reflects the BlockedBy relation edges"
        );
    }

    /// RFC 0002 D5 / #119: the filing repo is an INTERNAL axis. `task_to_dto`
    /// is the single funnel for [`TaskDto`], so it must never leak
    /// `filing_repo_id` onto the serialized DTO — only `repo_id` (the logical
    /// axis) crosses the boundary. The test populates the domain task's
    /// filing repo so it genuinely proves the mapping DROPS a *set* value,
    /// not merely that it is `None`. A future contributor adding the field
    /// "for symmetry" trips this guard.
    #[test]
    fn task_dto_json_omits_filing_repo_id_and_keeps_repo_id() {
        let logical = RepoId::new();
        let mut t = Task::new_draft(
            WorkspaceId::new(),
            Some(logical),
            "guard the boundary".into(),
        )
        .unwrap();
        // Set the INTERNAL axis to a *different* repo so the assertion proves
        // task_to_dto drops a populated filing repo, not just a None.
        t.set_filing_repo_id(Some(RepoId::new())).unwrap();

        let dto = task_to_dto(&t, Some("rpl"));
        let v = serde_json::to_value(&dto).unwrap();
        let obj = v.as_object().expect("TaskDto serializes to a JSON object");

        assert!(
            !obj.contains_key("filing_repo_id"),
            "TaskDto JSON must NOT carry the internal filing_repo_id axis (RFC 0002 D5, #119)"
        );
        assert_eq!(
            obj.get("repo_id").and_then(|r| r.as_str()),
            Some(logical.to_string().as_str()),
            "TaskDto must still carry the logical repo_id"
        );
    }

    /// RFC 0002 D5 / #119: the create/update/import command DTOs are part of
    /// the consumer contract and must not carry the internal `filing_repo_id`
    /// axis either. NOTE for the later CLI ticket: the per-task `--filing-repo`
    /// override lands on [`CreateTaskCmd`] as its OWN distinct input field — it
    /// is NEVER named `filing_repo_id`, so this guard stays valid; when that
    /// ticket lands it should only revisit the CreateTaskCmd line below if it
    /// chooses that key name (it must not).
    #[test]
    fn cmd_dtos_json_omit_filing_repo_id() {
        let create = CreateTaskCmd {
            workspace_id: WorkspaceId::new().to_string(),
            repo_id: Some(RepoId::new().to_string()),
            title: "t".into(),
            body: None,
            priority: None,
            // RFC 0002 D5 / #122: the per-task filing-repo override uses a
            // key distinct from `filing_repo_id` — this field being named
            // `filing_repo_override` (not `filing_repo_id`) keeps the guard
            // below valid. Testing with Some proves the serialization path
            // never emits `filing_repo_id` even when the field is populated.
            filing_repo_override: Some(RepoId::new().to_string()),
        };
        let update = UpdateTaskCmd {
            task_id: "rpl-abc".into(),
            title: Some("t".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: Some(RepoId::new().to_string()),
        };
        let import = ImportMirrorCmd {
            workspace_id: WorkspaceId::new().to_string(),
            repo_id: Some(RepoId::new().to_string()),
            provider: "github".into(),
            remote_id: "org/repo#1".into(),
            title: "t".into(),
            body: String::new(),
            assignees: vec![],
            closed: false,
        };

        for (name, v) in [
            ("CreateTaskCmd", serde_json::to_value(&create).unwrap()),
            ("UpdateTaskCmd", serde_json::to_value(&update).unwrap()),
            ("ImportMirrorCmd", serde_json::to_value(&import).unwrap()),
        ] {
            let obj = v.as_object().expect("command DTO is a JSON object");
            assert!(
                !obj.contains_key("filing_repo_id"),
                "{name} JSON must NOT carry the internal filing_repo_id axis (RFC 0002 D5, #119)"
            );
            assert!(
                obj.contains_key("repo_id"),
                "{name} must still carry the logical repo_id"
            );
        }
    }
}
