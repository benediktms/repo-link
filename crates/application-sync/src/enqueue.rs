//! Shared outbox-enqueue helpers (RFC 0001 Stage 6, #54).
//!
//! Lives here — not in `application-task` — because both `TaskService`
//! (lifecycle / edit) and `WorkspaceService` (eager set-project backfill)
//! need to translate a mirror task into the right [`OutboxMutation`], and
//! `application-sync` already owns the outbox vocabulary and the
//! status-option fallback ([`crate::option_id_for_status_with_fallback`]).
//!
//! These helpers are intentionally *decision* functions plus a thin enqueue
//! wrapper: they decide which mutation (if any) a mirror task owes and append
//! it to the outbox. They never touch the remote — the [`crate::OutboxDrainer`]
//! does that. Nothing is enqueued for `LocalOnly` tasks, priority-only edits,
//! relation ops, rollbacks, or no-op edits (the caller is responsible for not
//! invoking these on those paths; the mirror guards below add a second line of
//! defence).

use std::sync::Arc;

use domain_project::Project;
use domain_sync::{OutboxEntry, OutboxMutation};
use domain_task::{SyncState, Task};
use domain_workspace::Workspace;
use ports::{OutboxRepository, PortResult, ProjectRepository, WorkspaceRepository};

/// Is this task a mirror (i.e. not purely local)? Only mirror tasks owe
/// outbound mutations. Mirrors RFC 0001 §3 D2: `sync_state != LocalOnly`.
pub fn is_mirror(task: &Task) -> bool {
    task.sync != SyncState::LocalOnly
}

/// Is the mirror issue-backed (has a real REST issue)?
pub fn is_issue_backed(task: &Task) -> bool {
    task.remote.is_some()
}

/// Is the mirror draft-backed (a project draft with no REST issue)?
pub fn is_draft_backed(task: &Task) -> bool {
    task.remote.is_none() && task.project_item_id.is_some()
}

/// Resolve a task's owning [`Project`], if its workspace has one. `Ok(None)`
/// when the workspace is projectless — the common projectless path.
pub async fn resolve_project(
    workspaces: &Arc<dyn WorkspaceRepository>,
    projects: &Arc<dyn ProjectRepository>,
    task: &Task,
) -> PortResult<Option<Project>> {
    let workspace = workspaces.get(task.workspace_id).await?;
    project_for_workspace(projects, &workspace).await
}

/// Resolve the parent project from an ALREADY-FETCHED workspace, so a caller
/// that also needs the workspace (e.g. the RFC 0002 first-board-filing default)
/// doesn't pay a second `workspaces.get` round-trip.
pub async fn project_for_workspace(
    projects: &Arc<dyn ProjectRepository>,
    workspace: &Workspace,
) -> PortResult<Option<Project>> {
    let Some(project_id) = workspace.project_id.clone() else {
        return Ok(None);
    };
    Ok(Some(projects.get(project_id).await?))
}

/// Build the `SetProjectStatus` mutation for a task already attached to a
/// project item, applying the Blocked→Open fallback. `None` when no option
/// resolves (option-less board) or the task isn't attached yet.
pub fn set_project_status_mutation(project: &Project, task: &Task) -> Option<OutboxMutation> {
    let item_node_id = task.project_item_id.clone()?;
    let option_id = crate::option_id_for_status_with_fallback(project, task.status)?;
    Some(OutboxMutation::SetProjectStatus {
        project_node_id: project.id.as_str().to_string(),
        item_node_id,
        status_field_id: project.status_field_id.clone(),
        option_id,
    })
}

/// Enqueue a single mutation for `task_id`.
pub async fn enqueue(
    outbox: &Arc<dyn OutboxRepository>,
    task_id: domain_core::TaskId,
    mutation: OutboxMutation,
) -> PortResult<()> {
    let entry = OutboxEntry::new(task_id, mutation);
    outbox.enqueue(&entry).await
}

/// Plan the outbox mutations a mirror task owes after a **lifecycle** change
/// (start / complete / reopen / block / unblock / archive) or a **content**
/// edit (title / body). Returns them in apply order.
///
/// Routing (RFC 0001 §3 D2 / Stage 6):
/// - Project mirror with an item id → `SetProjectStatus` for the lifecycle
///   move (NOT an UpdateRemote-close for block/unblock — a project board moves
///   the card rather than closing the issue).
/// - Project mirror whose workspace has a project but `project_item_id` is
///   `None` → lazy net: `AddItem` (issue-backed) or `CreateDraftIssue`
///   (draft-backed); the drainer's write-back enqueues the follow-up
///   `SetProjectStatus` once the item id is known.
/// - Issue-backed mirror (any workspace) → `UpdateRemote` so the issue's
///   open/closed bit + title/body track the local task.
/// - Draft-backed mirror content edit → `UpdateDraftIssue`.
///
/// `filing_canonical` is the canonical URL of the repo the backing issue is
/// *filed* in (RFC 0002). For a cross-filed task this is the FILING repo, which
/// can differ from the logical repo — so the issue-addressing `UpdateRemote`
/// targets where the issue actually lives. Callers pass `filing_canonical_for`
/// (the recorded `filing_repo_id`, falling back to the logical `repo_id`), NOT
/// the logical canonical (which stays for D4 prefix / worktree / relink ops).
///
/// `content_changed` indicates a title/body edit happened (so issue/draft
/// content is pushed); lifecycle-only changes still push the open/closed bit
/// via `UpdateRemote` for issue-backed tasks because GitHub's issue state is
/// the lifecycle mirror. A draft-backed mirror has **no** lifecycle mirror on
/// the issue axis (drafts have no REST issue — the project card moves
/// instead), so its `UpdateDraftIssue` is gated on `content_changed`: a
/// lifecycle-only transition (`start`/`complete`/`block`/`archive`) would
/// otherwise enqueue a no-op content write.
pub fn plan_mutations(
    task: &Task,
    project: Option<&Project>,
    filing_canonical: Option<&str>,
    content_changed: bool,
) -> Vec<OutboxMutation> {
    if !is_mirror(task) {
        return Vec::new();
    }
    let mut out = Vec::new();

    // Issue-backed: keep the REST issue's title/body/state in sync. This is
    // the open/closed mirror; it runs regardless of project membership and on
    // lifecycle-only changes too (GitHub's issue state IS the lifecycle
    // mirror, so the open/closed bit must always push).
    if is_issue_backed(task) {
        if let (Some(remote), Some(canonical)) = (task.remote.as_ref(), filing_canonical) {
            out.push(OutboxMutation::UpdateRemote {
                canonical_repo: canonical.to_string(),
                remote_id: remote.remote_id.clone(),
                title: Some(task.title.clone()),
                body: Some(task.body.clone()),
                // The drainer re-derives (closed, state_reason) from the task's
                // live status; this hint is informational only.
                closed: None,
            });
        }
    } else if is_draft_backed(task) && content_changed {
        // Draft content edit — drafts have no REST counterpart. Only emit on
        // an actual title/body change; a lifecycle-only transition moves the
        // project card (via SetProjectStatus below), not the draft content.
        out.push(OutboxMutation::UpdateDraftIssue {
            item_node_id: task.project_item_id.clone().unwrap_or_default(),
            title: Some(task.title.clone()),
            body: Some(task.body.clone()),
        });
    }

    // Project axis: move the card, or lazily attach if not yet a member.
    if let Some(project) = project {
        match task.project_item_id.as_ref() {
            Some(_) => {
                if let Some(m) = set_project_status_mutation(project, task) {
                    out.push(m);
                }
            }
            None => {
                // Lazy net — attach now; SetProjectStatus follows via the
                // drainer's AddItem / CreateDraftIssue write-back.
                if let Some(remote) = task.remote.as_ref().and_then(|r| r.node_id.clone()) {
                    // Issue-backed: attach the existing issue to the board.
                    out.push(OutboxMutation::AddItem {
                        project_node_id: project.id.as_str().to_string(),
                        issue_node_id: remote,
                    });
                } else if task.remote.is_none() {
                    // Draft-backed (no REST issue): create a new board draft.
                    // The drainer's CreateDraftIssue write-back records the
                    // returned PVTI_ item id and enqueues the follow-up
                    // SetProjectStatus.
                    out.push(OutboxMutation::CreateDraftIssue {
                        project_node_id: project.id.as_str().to_string(),
                        title: task.title.clone(),
                        body: task.body.clone(),
                    });
                }
                // (An issue-backed task with no node id yet can't be attached;
                //  it will attach once its node id is known via a pull.)
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_core::{ProjectId, WorkspaceId};
    use domain_project::{Project, StatusMapping, StatusOption};
    use domain_task::{RemoteRef, SyncState, Task};

    fn make_project(id: &str) -> Project {
        Project::new(
            ProjectId::parse(id).unwrap(),
            "acme".into(),
            1,
            "Board".into(),
            "PVTSSF_f".into(),
            vec![StatusOption {
                option_id: "o1".into(),
                name: "Backlog".into(),
                ordinal: 0,
            }],
            vec![StatusMapping {
                status: domain_task::TaskStatus::Open,
                option_id: "o1".into(),
            }],
            false,
            domain_core::Timestamp::now(),
        )
        .unwrap()
    }

    /// Build a minimal synced draft-backed mirror (remote == None,
    /// project_item_id == None) ready for first-board-filing.
    fn make_draft_backed_mirror() -> Task {
        let ws = WorkspaceId::new();
        let mut t = Task::import_mirror(
            ws,
            None,
            RemoteRef::new("github", "0"),
            "draft title".into(),
            "draft body".into(),
            vec![],
            false,
        )
        .unwrap();
        // Strip the REST remote — makes it a pure draft with SyncState retained.
        t.remote = None;
        t
    }

    /// Build an issue-backed mirror with a GraphQL node id and no project_item_id.
    fn make_issue_backed_mirror(node_id: &str) -> Task {
        let ws = WorkspaceId::new();
        let mut t = Task::new_draft(ws, None, "issue title".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef {
            provider: "github".into(),
            remote_id: "42".into(),
            node_id: Some(node_id.to_string()),
        })
        .unwrap();
        t
    }

    // --- lazy-net branch: draft-backed emits CreateDraftIssue ---------------

    #[test]
    fn draft_backed_first_filing_emits_create_draft_issue() {
        let t = make_draft_backed_mirror();
        let project = make_project("PVT_kwHO_enq_draft");

        let mutations = plan_mutations(&t, Some(&project), None, false);
        let kinds: Vec<&str> = mutations.iter().map(|m| m.kind()).collect();
        assert!(
            kinds.contains(&"create_draft_issue"),
            "draft-backed first filing must emit CreateDraftIssue: {kinds:?}"
        );
    }

    #[test]
    fn draft_backed_create_draft_issue_carries_title_and_body() {
        let t = make_draft_backed_mirror();
        let project = make_project("PVT_kwHO_enq_content");

        let mutations = plan_mutations(&t, Some(&project), None, false);
        let m = mutations
            .iter()
            .find(|m| m.kind() == "create_draft_issue")
            .unwrap();
        match m {
            OutboxMutation::CreateDraftIssue {
                project_node_id,
                title,
                body,
            } => {
                assert_eq!(project_node_id, project.id.as_str());
                assert_eq!(title, "draft title");
                assert_eq!(body, "draft body");
            }
            other => panic!("expected CreateDraftIssue, got {other:?}"),
        }
    }

    // --- lazy-net branch: issue-backed still emits AddItem (regression guard) --

    #[test]
    fn issue_backed_first_filing_still_emits_add_item() {
        let t = make_issue_backed_mirror("I_nid");
        let project = make_project("PVT_kwHO_enq_additem");

        let mutations = plan_mutations(&t, Some(&project), Some("github.com/o/r"), false);
        let kinds: Vec<&str> = mutations.iter().map(|m| m.kind()).collect();
        assert!(
            kinds.contains(&"add_item"),
            "issue-backed first filing must still emit AddItem (regression): {kinds:?}"
        );
        assert!(
            !kinds.contains(&"create_draft_issue"),
            "issue-backed must NOT emit CreateDraftIssue: {kinds:?}"
        );
    }

    // --- no project → no lazy-net mutation (existing behaviour) -------------

    #[test]
    fn draft_backed_without_project_emits_nothing_for_project_axis() {
        let t = make_draft_backed_mirror();
        // No project passed → project axis is skipped entirely.
        let mutations = plan_mutations(&t, None, None, false);
        let kinds: Vec<&str> = mutations.iter().map(|m| m.kind()).collect();
        assert!(
            !kinds.contains(&"create_draft_issue"),
            "without a project there is no board to file on: {kinds:?}"
        );
        assert!(
            !kinds.contains(&"add_item"),
            "without a project AddItem must not be emitted: {kinds:?}"
        );
    }

    // --- local-only tasks are always silent ---------------------------------

    #[test]
    fn local_only_task_emits_nothing() {
        let t = Task::new_draft(WorkspaceId::new(), None, "local".into()).unwrap();
        assert_eq!(t.sync, SyncState::LocalOnly);
        let project = make_project("PVT_kwHO_enq_local");
        let mutations = plan_mutations(&t, Some(&project), None, false);
        assert!(
            mutations.is_empty(),
            "LocalOnly task must produce no mutations"
        );
    }
}
