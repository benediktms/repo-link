//! The `Task` aggregate: struct, lifecycle + sync state machines.

use domain_core::{Aggregate, DomainError, RepoId, Result, TaskId, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::enums::{Priority, RelationKind, SyncState, TaskStatus};
use crate::relation::{RemoteRef, TaskComment, TaskRelation};
use crate::snapshot::{SnapshotSource, TaskSnapshot};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub workspace_id: WorkspaceId,
    /// The task's **logical repo**: where the code/worktrees live and the
    /// source of the friendly-ID prefix. Today the backing GitHub issue is
    /// also filed in this repo — logical and filing repo are the same until
    /// RFC 0002 introduces a separate filing-repo axis. `None` for an orphan
    /// task (a project-board draft with no repo).
    pub repo_id: Option<RepoId>,
    /// The task's **filing repo** (RFC 0002): where its backing GitHub issue
    /// is actually filed. Resolved and recorded at promote; `None` until then,
    /// in which case the D2 resolution chain falls through to logical `repo_id`
    /// so behaviour is unchanged. Distinct from `repo_id` (logical, where the
    /// code lives) so the two can diverge. Internal axis (RFC 0002 D1/D5):
    /// persisted and on the aggregate, but deliberately NOT on the task
    /// DTO/JSON (the DTO guard is its own ticket, #119) and NOT a
    /// dirty-detection input (excluded from `snapshot_view`, added in #118).
    pub filing_repo_id: Option<RepoId>,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub sync: SyncState,
    pub priority: Priority,
    pub assignees: Vec<String>,
    pub remote: Option<RemoteRef>,
    pub relations: Vec<TaskRelation>,
    /// Comments mirrored from the remote issue (and, in a follow-up, pending
    /// outbound ones). Append-only; populated by the repository on load and
    /// reconciled by `sync pull`. Deliberately excluded from `snapshot_view`
    /// so comment activity never marks the task dirty.
    pub comments: Vec<TaskComment>,
    /// GitHub Projects v2 item node ID (`PVTI_…`) when the task is mirrored
    /// to a project — i.e. when its workspace has `project_id` set and the
    /// task has been promoted at least once. `None` for projectless
    /// workspaces and for project-bound tasks that haven't been promoted
    /// yet. Like `node_id` on [`RemoteRef`], purely additive: no consumer
    /// reads it before Stage 5 wires the GraphQL surface in.
    pub project_item_id: Option<String>,
    /// Cache of the task's *remote* GitHub Projects v2 status, as the
    /// option's `option_id` (`47fc9ee4`-style). Written by the Stage-7
    /// poller from the polled item's `status_option_id` (RFC 0001 Stage 8,
    /// closes #39); `None` means "not yet polled" (NOT a mismatch).
    ///
    /// This is a one-way mirror of remote state on a **separate drift axis**
    /// — it is deliberately excluded from [`Task::snapshot_view`] and
    /// [`Task::reconcile_dirty_against_baseline`] so a board move never flips
    /// `sync_state` (which tracks the REST open/closed + title/body axis).
    /// Drift surfacing compares it to the option the task's local lifecycle
    /// status maps to, independently of `sync_state`.
    pub project_status_option_id: Option<String>,
    /// Short globally-unique hash used to assemble the friendly composite
    /// ID (`{repo.prefix}-{hash}`). Lowercase RFC 4648 base32, length 3+
    /// (grown by the persistence layer on collision). Empty string is the
    /// "not yet minted" sentinel pre-backfill; minted tasks always carry
    /// a non-empty value.
    pub hash: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The latest baseline-eligible [`TaskSnapshot`] (one of `Promote` /
    /// `Push` / `Pull` / `ConflictResolve`). Populated by the repository
    /// on load; mutated by `set_synced_baseline` after a successful
    /// sync. Drives [`Task::reconcile_dirty_against_baseline`] —
    /// idempotent edits and reverts don't gratuitously mark the task
    /// dirty.
    pub synced_baseline: Option<TaskSnapshot>,
}

impl Task {
    /// New local-only task in `Open` status. Sync state starts at `LocalOnly`.
    pub fn new_draft(
        workspace_id: WorkspaceId,
        repo_id: Option<RepoId>,
        title: String,
    ) -> Result<Self> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        let now = Timestamp::now();
        Ok(Self {
            id: TaskId::new(),
            workspace_id,
            repo_id,
            // Local draft: filing repo is unresolved until promote (D2).
            filing_repo_id: None,
            title,
            body: String::new(),
            status: TaskStatus::Open,
            sync: SyncState::LocalOnly,
            priority: Priority::P3,
            assignees: Vec::new(),
            remote: None,
            relations: Vec::new(),
            comments: Vec::new(),
            project_item_id: None,
            project_status_option_id: None,
            // Empty until the persistence layer mints a unique base32
            // hash on first save. Domain stays agnostic to randomness
            // and DB-backed uniqueness retries.
            hash: String::new(),
            created_at: now,
            updated_at: now,
            synced_baseline: None,
        })
    }

    /// Construct a task that is already a mirror of an existing remote issue
    /// — the one-step factory `sync import` needs. Unlike the
    /// `new_draft → stage → promote` path (which *creates* the remote), this
    /// records a remote that already exists: `sync = Synced`, `remote` set,
    /// and the diff baseline captured from a `Pull` view (the remote is the
    /// source of truth from the first version). `status` mirrors the remote's
    /// open/closed bit; lifecycle and sync stay orthogonal thereafter. `hash`
    /// is left empty for the persistence layer to mint, as with `new_draft`.
    pub fn import_mirror(
        workspace_id: WorkspaceId,
        repo_id: Option<RepoId>,
        remote: RemoteRef,
        title: String,
        body: String,
        assignees: Vec<String>,
        closed: bool,
    ) -> Result<Self> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        let now = Timestamp::now();
        let mut task = Self {
            id: TaskId::new(),
            workspace_id,
            repo_id,
            // An imported mirror already has a remote issue, and historically
            // filing == logical, so record the filing repo as the logical one.
            // This makes the written row agree with the D6 dedup lookup (#120).
            filing_repo_id: repo_id,
            title,
            body,
            status: if closed {
                TaskStatus::Done
            } else {
                TaskStatus::Open
            },
            sync: SyncState::Synced,
            priority: Priority::P3,
            assignees,
            remote: Some(remote),
            relations: Vec::new(),
            comments: Vec::new(),
            project_item_id: None,
            project_status_option_id: None,
            hash: String::new(),
            created_at: now,
            updated_at: now,
            synced_baseline: None,
        };
        task.synced_baseline = Some(task.snapshot_view(SnapshotSource::Pull));
        Ok(task)
    }

    /// Refresh the diff baseline after a successful remote-aligning sync
    /// event (promote / push / pull / conflict resolve). The snapshot's
    /// `version` is assigned by the repository; the application layer
    /// constructs a candidate via [`Task::snapshot_view`] and the
    /// adapter persists it.
    pub fn set_synced_baseline(&mut self, baseline: TaskSnapshot) {
        self.synced_baseline = Some(baseline);
        self.reconcile_dirty_against_baseline();
    }

    /// Build a snapshot value of the task as it currently is.
    ///
    /// `version` is left as `0` — the repository is responsible for
    /// assigning the next monotonic version when persisting. The
    /// in-memory representation never needs the version to do diff
    /// comparisons; equality compares everything *except* version, so a
    /// fresh `snapshot_view` and the stored baseline still compare
    /// cleanly for dirty detection.
    pub fn snapshot_view(&self, source: SnapshotSource) -> TaskSnapshot {
        TaskSnapshot {
            task_id: self.id,
            version: 0,
            title: self.title.clone(),
            body: self.body.clone(),
            status: self.status,
            sync_state: self.sync,
            priority: self.priority,
            assignees: self.assignees.clone(),
            remote: self.remote.clone(),
            repo_id: self.repo_id,
            // Fresh snapshots always record the binding (even if it's None).
            repo_id_recorded: true,
            source,
            captured_at: Timestamp::now(),
        }
    }

    // --- Sync transitions ------------------------------------------------
    //
    // None of these change `status` — lifecycle and sync are orthogonal.
    // Archived tasks are still permitted to transition sync (e.g. a final
    // pull after archiving) so the daemon doesn't have to special-case
    // them; callers that want to skip Archived tasks can filter upstream.

    pub fn stage_for_sync(&mut self) -> Result<()> {
        match self.sync {
            SyncState::LocalOnly | SyncState::DirtyLocal => {
                self.sync = SyncState::Staged;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot stage from sync={other:?}"
            ))),
        }
    }

    pub fn promote_to_remote(&mut self, remote: RemoteRef) -> Result<()> {
        if self.sync != SyncState::Staged {
            return Err(DomainError::transition(format!(
                "cannot promote from sync={:?}",
                self.sync
            )));
        }
        self.remote = Some(remote);
        self.sync = SyncState::Synced;
        self.touch();
        // Promote IS a remote-alignment event — capture the current state
        // as the initial baseline so subsequent edits diff correctly.
        self.synced_baseline = Some(self.snapshot_view(SnapshotSource::Promote));
        Ok(())
    }

    /// Confirm that a sync event has aligned local with remote: flips
    /// `sync` to `Synced` AND refreshes the diff baseline. The `source`
    /// records *why* this alignment happened (push / pull / conflict
    /// resolve). Must be a baseline-eligible source — `LocalEdit`,
    /// `PrePull`, and `Rollback` aren't alignment events.
    pub fn confirm_synced(&mut self, source: SnapshotSource) -> Result<()> {
        if !source.is_baseline() {
            return Err(DomainError::validation(format!(
                "confirm_synced source must be Promote/Push/Pull/ConflictResolve, got {source:?}"
            )));
        }
        match self.sync {
            SyncState::Staged | SyncState::DirtyLocal | SyncState::DirtyRemote => {
                self.sync = SyncState::Synced;
                self.touch();
                self.synced_baseline = Some(self.snapshot_view(source));
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot confirm_synced from sync={other:?}"
            ))),
        }
    }

    pub fn mark_dirty_local(&mut self) -> Result<()> {
        match self.sync {
            SyncState::Synced => {
                self.sync = SyncState::DirtyLocal;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark dirty_local from sync={other:?}"
            ))),
        }
    }

    pub fn mark_dirty_remote(&mut self) -> Result<()> {
        match self.sync {
            SyncState::Synced => {
                self.sync = SyncState::DirtyRemote;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark dirty_remote from sync={other:?}"
            ))),
        }
    }

    pub fn mark_conflicted(&mut self) -> Result<()> {
        match self.sync {
            SyncState::DirtyLocal | SyncState::DirtyRemote | SyncState::Synced => {
                self.sync = SyncState::Conflict;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark conflict from sync={other:?}"
            ))),
        }
    }

    /// Rewire the task to a different `(repo_id, remote)` pair, used by
    /// `rl task link` to attach an arbitrary remote (`force_conflict = true` →
    /// flip to `Conflict`, the user must resolve via `sync pull` / explicit
    /// accept) or to record a verified post-transfer relink
    /// (`force_conflict = false` → keep the current sync state; the
    /// application layer is responsible for refreshing the snapshot baseline
    /// so the new remote becomes the dirty-detection ground truth).
    ///
    /// Rewrites both `repo_id` and `remote` atomically — `set_repo_id`'s "no
    /// reassign while remote-backed" guard does not apply here because link is
    /// the *intended* path for changing the remote. The snapshot history
    /// (tagged [`SnapshotSource::Link`]) is the audit trail.
    pub fn link_to_remote(
        &mut self,
        repo_id: RepoId,
        remote: RemoteRef,
        force_conflict: bool,
    ) -> Result<()> {
        self.repo_id = Some(repo_id);
        self.remote = Some(remote);
        if force_conflict {
            self.sync = SyncState::Conflict;
        }
        self.touch();
        Ok(())
    }

    // --- Lifecycle transitions ------------------------------------------
    //
    // All mutations that change remote-observable state (lifecycle status,
    // title, body, assignees) call `reconcile_dirty_against_baseline`
    // after touching. The helper diffs against [`synced_baseline`] (the
    // last known remote state) — idempotent edits stay `Synced`, real
    // edits flip `Synced → DirtyLocal`, edits while remote is dirty
    // escalate to `Conflict`, and reverts unwind `DirtyLocal → Synced`.
    // The CLI command returns as soon as the local store is updated;
    // the daemon picks up `DirtyLocal` tasks on its next tick.

    /// Move `Open` or `Blocked` into `InProgress` (signal start of work).
    pub fn start(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Open | TaskStatus::Blocked => {
                self.status = TaskStatus::InProgress;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot start from status={other:?}"
            ))),
        }
    }

    /// Move into `Blocked` from `Open` or `InProgress`.
    pub fn mark_blocked(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Open | TaskStatus::InProgress => {
                self.status = TaskStatus::Blocked;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot block from status={other:?}"
            ))),
        }
    }

    /// Move `Blocked` back to `Open`. Caller can `start()` again explicitly.
    pub fn unblock(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Blocked => {
                self.status = TaskStatus::Open;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot unblock from status={other:?}"
            ))),
        }
    }

    /// Mark `InProgress` task as `Done`.
    pub fn complete(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::InProgress => {
                self.status = TaskStatus::Done;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot complete from status={other:?}"
            ))),
        }
    }

    /// Move a `Done` task back to `Open` — used when work was marked done
    /// prematurely and needs to be reopened.
    pub fn reopen(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Done => {
                self.status = TaskStatus::Open;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot reopen from status={other:?}"
            ))),
        }
    }

    pub fn archive(&mut self) -> Result<()> {
        if self.status == TaskStatus::Archived {
            return Err(DomainError::transition("already archived"));
        }
        self.status = TaskStatus::Archived;
        self.touch();
        self.reconcile_dirty_against_baseline();
        Ok(())
    }

    pub fn add_relation(&mut self, kind: RelationKind, other: TaskId) {
        if !self
            .relations
            .iter()
            .any(|r| r.kind == kind && r.other == other)
        {
            self.relations.push(TaskRelation { kind, other });
            self.touch();
        }
    }

    /// Remove the `(kind, other)` edge if present. Returns whether anything
    /// was removed (so callers can skip a redundant save / reciprocal walk).
    pub fn remove_relation(&mut self, kind: RelationKind, other: TaskId) -> bool {
        let before = self.relations.len();
        self.relations
            .retain(|r| !(r.kind == kind && r.other == other));
        let removed = self.relations.len() != before;
        if removed {
            self.touch();
        }
        removed
    }

    /// Drop every relation on this task, returning the removed edges so the
    /// caller can strip the matching reciprocals from the other tasks.
    pub fn clear_relations(&mut self) -> Vec<TaskRelation> {
        if self.relations.is_empty() {
            return Vec::new();
        }
        let taken = std::mem::take(&mut self.relations);
        self.touch();
        taken
    }

    /// Priority is **local-only metadata**: GitHub doesn't model it, so a
    /// priority change does NOT flip sync state.
    pub fn set_priority(&mut self, priority: Priority) {
        if self.priority != priority {
            self.priority = priority;
            self.touch();
        }
    }

    /// Cache the task's remote GitHub Projects v2 status option id (written
    /// by the Stage-7 poller from a polled item, RFC 0001 Stage 8). This is a
    /// **separate drift axis**, not a lifecycle/sync transition: it mirrors
    /// remote state for `rl query drift` + `rl task show` and deliberately
    /// does NOT call `reconcile_dirty_against_baseline` or `touch` — a board
    /// move must never flip `sync_state` nor mark the task updated. Returns
    /// whether the cached value actually changed so the poller can skip a
    /// redundant persist.
    pub fn set_project_status_option_id(&mut self, option_id: Option<String>) -> bool {
        if self.project_status_option_id == option_id {
            return false;
        }
        self.project_status_option_id = option_id;
        true
    }

    /// Reassign the task's **logical repo** binding (code/worktrees/prefix).
    /// Permitted only while the task is not yet remote-backed: once promoted,
    /// the backing issue lives in a specific GitHub repo (today the logical
    /// repo, until RFC 0002 splits out a separate filing repo), so moving the
    /// local task to a different binding would orphan that issue. Logical-repo
    /// ownership is local metadata (it selects the promote/filing target), so
    /// — like `priority` — changing it does NOT flip sync state.
    pub fn set_repo_id(&mut self, repo_id: Option<RepoId>) -> Result<()> {
        // Idempotent no-op: setting the same value is always fine, even on
        // a remote-backed task — only an actual *change* is rejected.
        if self.repo_id == repo_id {
            return Ok(());
        }
        if self.remote.is_some() {
            return Err(DomainError::validation(
                "cannot reassign the repo of a task already synced to a remote issue",
            ));
        }
        self.repo_id = repo_id;
        self.touch();
        Ok(())
    }

    /// Record the task's **filing repo** (RFC 0002) — where its backing GitHub
    /// issue is filed. Resolved at promote, i.e. set at the very moment the
    /// task becomes remote-backed, so unlike [`Task::set_repo_id`] this must
    /// permit the initial set even when `remote.is_some()`: a naive
    /// reject-once-remote guard would reject the recording write itself.
    ///
    /// Contract: the initial set is allowed whenever `filing_repo_id` is
    /// currently `None` (remote-backed or not); only a *change* of an
    /// already-recorded value is rejected, because the backing issue already
    /// lives in that repo and re-pointing it would orphan it. Setting the same
    /// value is an idempotent no-op. Like the logical repo, the filing repo is
    /// local sync/persistence metadata, NOT a mirrored field, so this does not
    /// call [`Task::reconcile_dirty_against_baseline`].
    pub fn set_filing_repo_id(&mut self, filing_repo_id: Option<RepoId>) -> Result<()> {
        // Idempotent no-op, including re-recording the same value at promote.
        if self.filing_repo_id == filing_repo_id {
            return Ok(());
        }
        // Reject only a change of an already-recorded filing repo; the initial
        // set (currently `None`) is always allowed, even once remote-backed.
        if self.filing_repo_id.is_some() {
            return Err(DomainError::validation(
                "cannot change or clear the filing repo of a task once it has been recorded",
            ));
        }
        self.filing_repo_id = filing_repo_id;
        self.touch();
        Ok(())
    }

    /// Body is mirrored to the remote issue, so editing it marks the task
    /// `DirtyLocal` (when remote-backed and currently `Synced`).
    pub fn set_body(&mut self, body: String) {
        self.body = body;
        self.touch();
        self.reconcile_dirty_against_baseline();
    }

    /// Title round-trips to the remote issue's title.
    pub fn set_title(&mut self, title: String) -> Result<()> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        self.title = title;
        self.touch();
        self.reconcile_dirty_against_baseline();
        Ok(())
    }

    /// Assignees round-trip to the remote issue's assignees.
    pub fn set_assignees(&mut self, assignees: Vec<String>) {
        self.assignees = assignees;
        self.touch();
        self.reconcile_dirty_against_baseline();
    }

    pub fn is_remote_backed(&self) -> bool {
        self.remote.is_some()
    }

    /// Diff the current task against [`Task::synced_baseline`] and
    /// reconcile the sync state. Called by every mutation that touches
    /// remote-observable state.
    ///
    /// Only fields GitHub mirrors (title, body, status, assignees) count
    /// toward the diff — `priority` is local metadata, `relations` live
    /// in their own table.
    ///
    /// Transition table (assuming a baseline exists):
    ///
    /// | Before  | Differs from baseline | After |
    /// |---|---|---|
    /// | `Synced` | yes | `DirtyLocal` (standard edit) |
    /// | `Synced` | no | `Synced` (idempotent edit; no-op) |
    /// | `DirtyLocal` | no | `Synced` (revert to baseline value) |
    /// | `DirtyLocal` | yes | `DirtyLocal` |
    /// | `DirtyRemote` | yes | `Conflict` (both sides diverged) |
    /// | `Conflict` | * | `Conflict` (already worst-case) |
    /// | `Staged` | * | `Staged` (the in-flight push will read latest fields) |
    /// | `LocalOnly` | * | `LocalOnly` (no remote ⇒ baseline is None ⇒ early return) |
    ///
    /// Without a baseline (local-only task, never promoted) this is a
    /// no-op.
    pub fn reconcile_dirty_against_baseline(&mut self) {
        let Some(baseline) = &self.synced_baseline else {
            return;
        };
        let differs = self.title != baseline.title
            || self.body != baseline.body
            || self.status != baseline.status
            || !assignees_equal(&self.assignees, &baseline.assignees);
        self.sync = match (self.sync, differs) {
            (SyncState::Synced, true) => SyncState::DirtyLocal,
            (SyncState::DirtyLocal, false) => SyncState::Synced,
            (SyncState::DirtyRemote, true) => SyncState::Conflict,
            (other, _) => other,
        };
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

/// Order-insensitive assignee comparison. GitHub does not preserve the order
/// of assignees in its REST responses, so `["alice","bob"]` and `["bob","alice"]`
/// must be treated as equal to avoid spurious `DirtyLocal` transitions.
/// Order-insensitive set equality for assignee lists. GitHub doesn't guarantee
/// a stable order across responses, so callers reconciling local + remote
/// assignees must compare as sets to avoid spurious drift on re-ordering.
pub fn assignees_equal(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let a_set: std::collections::HashSet<&String> = a.iter().collect();
    let b_set: std::collections::HashSet<&String> = b.iter().collect();
    a_set == b_set
}

impl Aggregate for Task {
    type Id = TaskId;

    fn id(&self) -> Self::Id {
        self.id
    }

    fn created_at(&self) -> Timestamp {
        self.created_at
    }

    fn updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Task {
        Task::new_draft(WorkspaceId::new(), None, "do the thing".into()).unwrap()
    }

    fn remote_ref() -> RemoteRef {
        RemoteRef::new("github", "org/repo#1")
    }

    #[test]
    fn rejects_empty_title() {
        assert!(Task::new_draft(WorkspaceId::new(), None, "  ".into()).is_err());
    }

    #[test]
    fn happy_path_local_only_to_synced() {
        let mut t = draft();
        assert_eq!(t.status, TaskStatus::Open);
        assert_eq!(t.sync, SyncState::LocalOnly);
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        // promote_to_remote lands directly on Synced — see its doc comment.
        assert_eq!(t.sync, SyncState::Synced);
        assert!(t.is_remote_backed());
    }

    #[test]
    fn promote_requires_staged() {
        let mut t = draft();
        assert!(t.promote_to_remote(remote_ref()).is_err());
    }

    #[test]
    fn import_mirror_lands_synced_with_pull_baseline() {
        let t = Task::import_mirror(
            WorkspaceId::new(),
            None,
            remote_ref(),
            "imported".into(),
            "from remote".into(),
            vec!["alice".into()],
            true,
        )
        .unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        assert_eq!(t.status, TaskStatus::Done); // closed → Done
        assert!(t.is_remote_backed());
        assert_eq!(t.assignees, vec!["alice".to_string()]);
        // Baseline captured from the remote, so a fresh import is NOT dirty.
        let baseline = t.synced_baseline.as_ref().expect("baseline set");
        assert_eq!(baseline.source, SnapshotSource::Pull);
    }

    #[test]
    fn comments_do_not_dirty_a_synced_task() {
        let mut t = draft();
        assert!(t.comments.is_empty());
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        // Mirroring a comment must not perturb sync state: comments are
        // excluded from the snapshot baseline, so a subsequent reconcile
        // (here via a no-op body set) leaves the task Synced, not DirtyLocal.
        t.comments.push(TaskComment {
            local_id: None,
            remote_id: Some("7".into()),
            author: "octocat".into(),
            body: "looks good".into(),
            created_at: Timestamp::now(),
        });
        let body = t.body.clone();
        t.set_body(body);
        assert_eq!(
            t.sync,
            SyncState::Synced,
            "comment activity must not dirty a synced task"
        );
        assert_eq!(t.comments.len(), 1);
    }

    #[test]
    fn link_to_remote_with_force_conflict_flips_synced_to_conflict() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        let new_remote = RemoteRef::new("github", "999");
        t.link_to_remote(RepoId::new(), new_remote.clone(), true)
            .unwrap();
        assert_eq!(t.sync, SyncState::Conflict);
        assert_eq!(t.remote.as_ref(), Some(&new_remote));
    }

    #[test]
    fn link_to_remote_verified_preserves_sync_state() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        let new_remote = RemoteRef::new("github", "1506");
        // Verified relink: caller asserts the new remote is identity-preserving.
        t.link_to_remote(RepoId::new(), new_remote.clone(), false)
            .unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        assert_eq!(t.remote.as_ref(), Some(&new_remote));
    }

    #[test]
    fn link_to_remote_from_local_only_with_force_conflict_attaches_and_conflicts() {
        let mut t = draft();
        assert_eq!(t.sync, SyncState::LocalOnly);
        assert!(t.remote.is_none());

        let repo = RepoId::new();
        t.link_to_remote(repo, remote_ref(), true).unwrap();
        // Arbitrary attach: local task had no remote; link wires it up and the
        // user must resolve the divergence between the two histories.
        assert_eq!(t.sync, SyncState::Conflict);
        assert!(t.remote.is_some());
        assert_eq!(t.repo_id, Some(repo));
    }

    #[test]
    fn import_mirror_open_issue_maps_to_open() {
        let t = Task::import_mirror(
            WorkspaceId::new(),
            None,
            remote_ref(),
            "open one".into(),
            String::new(),
            vec![],
            false,
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Open);
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn import_mirror_rejects_empty_title() {
        assert!(
            Task::import_mirror(
                WorkspaceId::new(),
                None,
                remote_ref(),
                "   ".into(),
                String::new(),
                vec![],
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn dirty_local_then_resync() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_local().unwrap();
        t.stage_for_sync().unwrap();
        assert_eq!(t.sync, SyncState::Staged);
    }

    #[test]
    fn lifecycle_and_sync_are_independent() {
        // A task can be Blocked + DirtyLocal at the same time. Blocking
        // doesn't roll back sync; staging doesn't unblock.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_local().unwrap();
        t.mark_blocked().unwrap();
        assert_eq!(t.status, TaskStatus::Blocked);
        assert_eq!(t.sync, SyncState::DirtyLocal);
    }

    #[test]
    fn status_transitions_open_inprogress_blocked() {
        let mut t = draft();
        t.start().unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        t.mark_blocked().unwrap();
        assert_eq!(t.status, TaskStatus::Blocked);
        t.start().unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        t.mark_blocked().unwrap();
        t.unblock().unwrap();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn cannot_start_archived_task() {
        let mut t = draft();
        t.archive().unwrap();
        assert!(t.start().is_err());
    }

    #[test]
    fn complete_requires_in_progress_and_reopen_returns_to_open() {
        let mut t = draft();
        // Can't complete an Open task — must start it first.
        assert!(t.complete().is_err());
        t.start().unwrap();
        t.complete().unwrap();
        assert_eq!(t.status, TaskStatus::Done);
        // Done is not Archived: still visible, still mutable.
        t.reopen().unwrap();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn done_tasks_can_still_be_archived() {
        let mut t = draft();
        t.start().unwrap();
        t.complete().unwrap();
        t.archive().unwrap();
        assert_eq!(t.status, TaskStatus::Archived);
    }

    /// The whole point of the auto-dirty behavior: a lifecycle transition
    /// on a remote-backed Synced task should enqueue a remote update via
    /// the DirtyLocal flag, so the daemon picks it up on its next tick.
    /// The CLI command itself stays synchronous (no network in the hot
    /// path).
    #[test]
    fn lifecycle_mutations_on_synced_remote_task_flip_to_dirty_local() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        // Each lifecycle hop should leave the task DirtyLocal so the
        // daemon picks it up on its next push tick.
        t.start().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        // mark_synced re-syncs so we can exercise the next transition.
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.complete().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.reopen().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.mark_blocked().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.unblock().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
    }

    #[test]
    fn local_only_task_lifecycle_does_not_flip_sync() {
        // Without a remote, lifecycle changes leave sync at LocalOnly —
        // there's nothing to push.
        let mut t = draft();
        assert_eq!(t.sync, SyncState::LocalOnly);
        t.start().unwrap();
        t.complete().unwrap();
        assert_eq!(t.sync, SyncState::LocalOnly);
    }

    #[test]
    fn body_and_title_edits_also_flip_to_dirty_local() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();

        t.set_body("revised".into());
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.set_title("new title".into()).unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.set_assignees(vec!["alice".into()]);
        assert_eq!(t.sync, SyncState::DirtyLocal);
    }

    #[test]
    fn priority_is_local_only_and_does_not_dirty() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        t.set_priority(Priority::P0);
        // Priority isn't a remote field — no spurious DirtyLocal flip.
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn set_repo_id_assigns_when_not_remote_backed() {
        let mut t = draft();
        assert_eq!(t.repo_id, None);
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        assert_eq!(t.repo_id, Some(repo));
        // Repo ownership selects the promote target; it's local metadata,
        // so assigning it on a local-only task doesn't flip sync state.
        assert_eq!(t.sync, SyncState::LocalOnly);
    }

    #[test]
    fn set_repo_id_rejected_once_remote_backed() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        // The remote issue lives in a specific repo; reassigning would
        // orphan it.
        let err = t.set_repo_id(Some(RepoId::new())).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn set_repo_id_idempotent_no_op() {
        let mut t = draft();
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        let before = t.updated_at;
        t.set_repo_id(Some(repo)).unwrap();
        assert_eq!(t.updated_at, before);
    }

    #[test]
    fn set_repo_id_noop_allowed_even_when_remote_backed() {
        // A no-op (same value) must succeed even after promotion — only an
        // actual *change* of repo on a remote-backed task is rejected.
        let mut t = draft();
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        let before = t.updated_at;
        t.set_repo_id(Some(repo)).unwrap(); // same value → Ok, no touch
        assert_eq!(t.updated_at, before);
    }

    #[test]
    fn set_filing_repo_id_records_on_local_task_without_dirtying() {
        let mut t = draft();
        assert_eq!(t.filing_repo_id, None);
        let repo = RepoId::new();
        t.set_filing_repo_id(Some(repo)).unwrap();
        assert_eq!(t.filing_repo_id, Some(repo));
        // Filing repo is local sync metadata, not a mirrored field.
        assert_eq!(t.sync, SyncState::LocalOnly);
    }

    #[test]
    fn set_filing_repo_id_allows_initial_set_when_remote_backed() {
        // The load-bearing case: the filing repo is resolved and recorded AT
        // promote, i.e. once the task is already remote-backed. The initial
        // set (currently None) must therefore be allowed, and must not flip
        // sync state — it isn't a remote-observable edit.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        assert_eq!(t.filing_repo_id, None);
        let repo = RepoId::new();
        t.set_filing_repo_id(Some(repo)).unwrap();
        assert_eq!(t.filing_repo_id, Some(repo));
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn set_filing_repo_id_rejects_change_of_recorded_value() {
        // Once recorded, the backing issue lives in that filing repo; changing
        // it would orphan the issue, so a *change* is rejected.
        let mut t = draft();
        t.set_filing_repo_id(Some(RepoId::new())).unwrap();
        let err = t.set_filing_repo_id(Some(RepoId::new())).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn set_filing_repo_id_idempotent_no_op() {
        let mut t = draft();
        let repo = RepoId::new();
        t.set_filing_repo_id(Some(repo)).unwrap();
        let before = t.updated_at;
        t.set_filing_repo_id(Some(repo)).unwrap(); // same value → Ok, no touch
        assert_eq!(t.updated_at, before);
    }

    #[test]
    fn import_mirror_records_filing_repo_as_logical() {
        // An imported mirror already has a remote issue and historically
        // filing == logical, so the constructor records the filing repo as
        // the logical one (keeps the written row in step with the D6 dedup).
        let repo = RepoId::new();
        let t = Task::import_mirror(
            WorkspaceId::new(),
            Some(repo),
            remote_ref(),
            "imported".into(),
            String::new(),
            vec![],
            false,
        )
        .unwrap();
        assert_eq!(t.repo_id, Some(repo));
        assert_eq!(t.filing_repo_id, Some(repo));
    }

    /// Mutating a `DirtyRemote` task is the textbook conflict case: remote
    /// had changes we hadn't pulled, and now we're piling local changes on
    /// top. Sync state must escalate to `Conflict` so the daemon (and the
    /// `query drift` view) surfaces it correctly — silently letting it
    /// look like a clean DirtyLocal would erase the "remote was dirty"
    /// signal.
    #[test]
    fn mutation_on_dirty_remote_escalates_to_conflict() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_remote().unwrap();
        assert_eq!(t.sync, SyncState::DirtyRemote);
        t.set_body("local edit".into());
        assert_eq!(t.sync, SyncState::Conflict);
    }

    #[test]
    fn lifecycle_mutation_on_dirty_remote_also_escalates() {
        // Same rule for non-body mutations — lifecycle changes are
        // remote-observable, so they count.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_remote().unwrap();
        t.start().unwrap();
        assert_eq!(t.sync, SyncState::Conflict);
    }

    #[test]
    fn mutation_on_conflict_stays_conflict() {
        // Once conflicted, edits don't make things worse — but they also
        // don't accidentally "resolve" the conflict by flipping to DirtyLocal.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_conflicted().unwrap();
        assert_eq!(t.sync, SyncState::Conflict);
        t.set_body("trying anyway".into());
        t.start().unwrap();
        t.set_assignees(vec!["alice".into()]);
        assert_eq!(t.sync, SyncState::Conflict);
    }

    /// Reordering assignees (without any other change) must NOT produce a
    /// false-positive `DirtyLocal`. GitHub's REST API returns assignees in
    /// arbitrary order, so order has no semantic meaning.
    #[test]
    fn assignee_reorder_does_not_dirty() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.set_assignees(vec!["alice".into(), "bob".into()]);
        t.confirm_synced(SnapshotSource::Push).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        // Reorder only — no other field changes.
        t.set_assignees(vec!["bob".into(), "alice".into()]);
        assert_eq!(
            t.sync,
            SyncState::Synced,
            "reordering assignees must not produce DirtyLocal"
        );
    }

    #[test]
    fn relations_are_deduplicated() {
        let mut t = draft();
        let other = TaskId::new();
        t.add_relation(RelationKind::BlockedBy, other);
        t.add_relation(RelationKind::BlockedBy, other);
        assert_eq!(t.relations.len(), 1);
    }

    #[test]
    fn archive_is_terminal() {
        let mut t = draft();
        t.archive().unwrap();
        assert!(t.archive().is_err());
    }
}
