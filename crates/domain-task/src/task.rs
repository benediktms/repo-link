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
            // Capture the resolved filing repo for history/audit (RFC 0002
            // #118). It is NOT a dirty-detection input —
            // `reconcile_dirty_against_baseline` never reads it — so recording
            // it here can't flip sync state.
            filing_repo_id: self.filing_repo_id,
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

    /// Confirm a sync event that aligned local with remote for **only** the
    /// fields actually transmitted in the PATCH (RFC 0003 D5). The baseline
    /// advances per-field — every field whose `Some` flag is set in `patch`
    /// is overwritten on the prior baseline; every other field stays
    /// byte-identical to its pre-call value. This closes the silent-rebaseline
    /// class: an untransmitted field (today: any field whose channel is
    /// incomplete) must remain dirty so the next push re-sends it, instead
    /// of being hidden by a premature whole-baseline refresh.
    ///
    /// Guards:
    /// - `source` must be one of `Promote` / `Push` / `Pull` / `ConflictResolve`
    ///   — the four sources that correspond to an actual transmitted PATCH
    ///   payload. **`Link` is rejected** even though [`SnapshotSource::is_baseline`]
    ///   admits it: a `Link` event rewires the remote identity but does not
    ///   send a PATCH, so there are no per-field transmitted values to merge —
    ///   use the full-snapshot [`Task::confirm_synced`] for a verified relink.
    /// - `self.sync` must be in `{Staged, DirtyLocal, DirtyRemote}`.
    /// - A baseline must be present — the merge is over the existing baseline,
    ///   not a fresh one. The merged baseline's `source` is stamped with the
    ///   call's `source` argument; `version` / `captured_at` are preserved
    ///   from the prior baseline (the repository re-stamps `captured_at` on
    ///   save with its own clock, so the in-memory value is informational
    ///   only).
    ///
    /// After the merge, `reconcile_dirty_against_baseline` runs so that any
    /// un-rebaselined field that still differs flips the task back to
    /// `DirtyLocal` — a partial re-baseline must NOT silently turn a
    /// `DirtyLocal` task into a `Synced` one when other fields are still
    /// diverged.
    ///
    /// The PATCH response MUST NOT be used to source the new baseline —
    /// `update_issue` returns the response value, which reflects the
    /// `assignees=[]` it sent (a `None` assignees field becomes an empty
    /// list in the response), and re-baselining from it would clobber local
    /// intent. The application layer passes the transmitted local
    /// `MirrorPatch` instead.
    pub fn confirm_synced_fields(
        &mut self,
        source: SnapshotSource,
        patch: &MirrorPatch,
    ) -> Result<()> {
        if !matches!(
            source,
            SnapshotSource::Promote
                | SnapshotSource::Push
                | SnapshotSource::Pull
                | SnapshotSource::ConflictResolve
        ) {
            return Err(DomainError::validation(format!(
                "confirm_synced_fields source must be Promote/Push/Pull/ConflictResolve, got {source:?}"
            )));
        }
        match self.sync {
            SyncState::Staged | SyncState::DirtyLocal | SyncState::DirtyRemote => {}
            other => {
                return Err(DomainError::transition(format!(
                    "cannot confirm_synced_fields from sync={other:?}"
                )));
            }
        }
        let prior = self
            .synced_baseline
            .as_ref()
            .ok_or_else(|| {
                DomainError::transition("cannot confirm_synced_fields without a synced_baseline")
            })?
            .clone();
        let mut merged = prior;
        merged.source = source;
        if let Some(title) = &patch.title {
            merged.title = title.clone();
        }
        if let Some(body) = &patch.body {
            merged.body = body.clone();
        }
        if let Some(status) = patch.status {
            merged.status = status;
        }
        if let Some(assignees) = &patch.assignees {
            merged.assignees = assignees.clone();
        }
        self.synced_baseline = Some(merged);
        self.sync = SyncState::Synced;
        self.touch();
        // Re-run dirty detection so any un-rebaselined field that still
        // differs keeps the task DirtyLocal — the partial-baseline fix
        // would be defeated by leaving the task Synced.
        self.reconcile_dirty_against_baseline();
        Ok(())
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
    ///
    /// Pre-RFC-0002 `repo_id` WAS the backing issue's home, so reassigning it
    /// on a remote-backed task would orphan that issue — hence the original
    /// blanket lock. Post-RFC-0002 (docs/rfcs/0002-task-repo-axes) `repo_id` is
    /// purely the LOGICAL repo; the issue lives in [`Task::filing_repo_id`].
    ///
    /// So once remote-backed, a change is permitted IFF a filing repo is
    /// **recorded** — `filing_repo_id` is `Some` — because the issue's home is
    /// then pinned by `filing_repo_id` independently of the logical repo, which
    /// this leaves untouched. Whether the recorded filing repo *equals* the
    /// logical repo is irrelevant: a logical-repo change cannot move an issue
    /// whose home is already pinned (e.g. `sync import --cascade` rows where
    /// `filing_repo_id == repo_id` are valid, fully-recorded states and must be
    /// reassignable). It is still rejected only when `filing_repo_id` is `None`:
    /// there nothing pins the issue's home, so moving the logical repo would
    /// orphan the issue (pre-RFC behaviour, preserved). Re-collapsing logical
    /// onto filing (new `repo_id` == `filing_repo_id`) is likewise allowed — the
    /// recorded filing repo stays authoritative and is never re-resolved.
    ///
    /// Logical-repo ownership is local metadata (it selects the promote/filing
    /// target), so — like `priority`, and per RFC 0003's dirty-detection
    /// exclusion of both repo axes — changing it does NOT flip sync state.
    pub fn set_repo_id(&mut self, repo_id: Option<RepoId>) -> Result<()> {
        // Idempotent no-op: setting the same value is always fine, even on
        // a remote-backed task — only an actual *change* is rejected.
        if self.repo_id == repo_id {
            return Ok(());
        }
        if self.remote.is_some() && self.filing_repo_id.is_none() {
            // The backing issue's home is pinned by `filing_repo_id`; only when
            // it is recorded is the logical repo free to move without orphaning
            // the issue. Equality with the logical repo does not matter — a
            // recorded filing repo (even one collapsed onto the logical repo)
            // pins the issue independently. Reject only when nothing is
            // recorded.
            return Err(DomainError::validation(
                "cannot reassign the repo of a synced task unless a filing repo is recorded",
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
        let differs = MIRRORED_FIELDS.iter().any(|f| f.differs(self, baseline));
        self.sync = match (self.sync, differs) {
            (SyncState::Synced, true) => SyncState::DirtyLocal,
            (SyncState::DirtyLocal, false) => SyncState::Synced,
            (SyncState::DirtyRemote, true) => SyncState::Conflict,
            (other, _) => other,
        };
    }

    /// Field-level diff of the live task against [`Task::synced_baseline`]: each
    /// [`MirrorPatch`] field is `Some(current value)` iff that mirrored field
    /// differs from the baseline, reusing the **same** [`MirrorField::differs`]
    /// comparators as dirty-detection so the diff and the sync-state verdict can
    /// never disagree. Without a baseline (never promoted) the patch is empty —
    /// mirroring [`Task::reconcile_dirty_against_baseline`]'s no-baseline no-op.
    ///
    /// Compared field-by-field on purpose — never whole-snapshot `PartialEq`, whose
    /// `version`/`captured_at`/`sync_state` always differ (RFC 0003 D2).
    pub fn diff_against_baseline(&self) -> MirrorPatch {
        let Some(baseline) = &self.synced_baseline else {
            return MirrorPatch::default();
        };
        MirrorPatch {
            title: MirrorField::Title
                .differs(self, baseline)
                .then(|| self.title.clone()),
            body: MirrorField::Body
                .differs(self, baseline)
                .then(|| self.body.clone()),
            status: MirrorField::Status
                .differs(self, baseline)
                .then_some(self.status),
            assignees: MirrorField::Assignees
                .differs(self, baseline)
                .then(|| self.assignees.clone()),
        }
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

/// The fields GitHub mirrors on a task's backing issue — the single source of
/// truth (RFC 0003 D1) for what counts as remote-observable content. Dirty
/// detection here, the field-level diff ([`Task::diff_against_baseline`], rpl-day),
/// and the outbound/inbound field sets (`application-sync`, rpl-47f) all reference
/// this set so the definitions cannot drift apart. `priority`, `relations`, and
/// the project-status axis are deliberately NOT mirrored.
///
/// Keep in lockstep with the mirrored fields [`Task::snapshot_view`] captures into
/// the baseline — [`MirrorField::differs`] compares the live task against exactly
/// those captured fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorField {
    Title,
    Body,
    Status,
    Assignees,
}

/// Canonical iteration order over [`MirrorField`]. Detection folds this with
/// [`MirrorField::differs`]; the field-level diff (rpl-day) walks the same set.
pub const MIRRORED_FIELDS: [MirrorField; 4] = [
    MirrorField::Title,
    MirrorField::Body,
    MirrorField::Status,
    MirrorField::Assignees,
];

impl MirrorField {
    /// True iff this field differs between the live `task` and its `baseline`
    /// snapshot, using the canonical per-field comparator: string inequality for
    /// title/body, enum inequality for status, and [`assignees_equal`] (unordered
    /// set equality) for assignees. Compared field-by-field on purpose — never via
    /// whole-snapshot `PartialEq`, whose `version`/`captured_at`/`sync_state` always
    /// differ.
    pub fn differs(self, task: &Task, baseline: &TaskSnapshot) -> bool {
        match self {
            MirrorField::Title => task.title != baseline.title,
            MirrorField::Body => task.body != baseline.body,
            MirrorField::Status => task.status != baseline.status,
            MirrorField::Assignees => !assignees_equal(&task.assignees, &baseline.assignees),
        }
    }
}

/// A field-level diff of a task against its [`Task::synced_baseline`]: each field is
/// `Some(current value)` iff it differs from the baseline (per
/// [`MirrorField::differs`]), else `None`. Built by [`Task::diff_against_baseline`]
/// so the outbound push (rpl-x2v / rpl-47f) can send only the fields that changed.
/// Carries domain values — `status` stays a [`TaskStatus`]; the open/closed mapping
/// happens at the outbound boundary, not here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MirrorPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<TaskStatus>,
    pub assignees: Option<Vec<String>>,
}

impl MirrorPatch {
    /// No mirrored field differs from the baseline (or there is no baseline).
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.status.is_none()
            && self.assignees.is_none()
    }
}

/// Order-insensitive set equality for assignee lists. GitHub doesn't guarantee a
/// stable order across REST responses, so `["alice","bob"]` and `["bob","alice"]`
/// must compare equal — callers reconciling local + remote assignees compare as
/// sets to avoid spurious `DirtyLocal` transitions on a pure re-ordering.
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

    /// A remote-backed, `Synced` task with a baseline captured at promote.
    fn synced() -> Task {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        t
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
    fn mirrored_fields_set_is_canonical_and_complete() {
        // Tripwire: adding a mirrored field to `Task` without extending the set
        // (or vice versa) fails here. Supports the rpl-3j3 labels decision.
        assert_eq!(MIRRORED_FIELDS.len(), 4);
        for f in [
            MirrorField::Title,
            MirrorField::Body,
            MirrorField::Status,
            MirrorField::Assignees,
        ] {
            assert!(
                MIRRORED_FIELDS.contains(&f),
                "{f:?} missing from MIRRORED_FIELDS"
            );
        }
    }

    #[test]
    fn inbound_mirror_set_excludes_status_per_d7() {
        // Tripwire for the D7 inbound carve-out (RFC 0003 §2 D7, rpl-47f):
        // the inbound (pull) path excludes `Status` because pull cannot
        // map GitHub's two-state open/closed onto the local 5-state
        // lifecycle. The 3-field shape is re-encoded inline below on
        // purpose: the `application-sync` side re-encodes the same
        // literal in its own tripwire test, and the duplication IS the
        // assertion — a divergence in either crate fails both build
        // graphs. If a future PR adds a new `MirrorField` to the
        // canonical set and wants it on the inbound path, this test is
        // the place that decision gets encoded: extend the `INBOUND`
        // slice here AND extend the matching literal in
        // `application-sync`'s `inbound_mirror_field_set_excludes_status`.
        //
        // `MIRRORED_FIELDS.len() == 4` pins canonical-set growth: a
        // 5th `MirrorField` (e.g. `Labels` from RFC 0003 D8) would
        // force a deliberate decision about whether the new field is
        // inbound. The `MIRRORED_FIELDS.contains(&Status)` half pins
        // "Status is canonical but excluded from inbound" — a
        // hand-rolled INBOUND without Status is necessary but not
        // sufficient; the assertion makes the carve-out explicit.
        const INBOUND: [MirrorField; 3] = [
            MirrorField::Title,
            MirrorField::Body,
            MirrorField::Assignees,
        ];
        assert_eq!(
            MIRRORED_FIELDS.len(),
            4,
            "canonical MIRRORED_FIELDS must be exactly 4 fields (Title, Body, Status, Assignees)"
        );
        for f in INBOUND {
            assert!(
                MIRRORED_FIELDS.contains(&f),
                "inbound field {f:?} missing from canonical MIRRORED_FIELDS"
            );
        }
        assert!(
            MIRRORED_FIELDS.contains(&MirrorField::Status),
            "Status is canonical but excluded from inbound — D7 carve-out, not a missing field"
        );
        assert!(
            !INBOUND.contains(&MirrorField::Status),
            "Status must remain outbound-only (D7) — pulling the REST closed bit into the local 5-state lifecycle is out of scope"
        );
    }

    #[test]
    fn mirror_field_differs_isolates_each_field() {
        let mut t = synced();
        t.set_assignees(vec!["alice".into()]);
        // Re-baseline so the seeded assignee is part of the baseline, not a diff.
        t.confirm_synced(SnapshotSource::Push).unwrap();
        let baseline = t.synced_baseline.clone().expect("baseline");

        // Unmodified task: no field differs.
        for f in MIRRORED_FIELDS {
            assert!(
                !f.differs(&t, &baseline),
                "{f:?} must not differ when unchanged"
            );
        }

        // Each mutation (applied to a fresh clone) trips exactly one field.
        let assert_only = |changed: MirrorField, c: &Task| {
            for f in MIRRORED_FIELDS {
                assert_eq!(
                    f.differs(c, &baseline),
                    f == changed,
                    "only {changed:?} should differ, but {f:?} disagreed"
                );
            }
        };

        let mut title = t.clone();
        title.title = "changed".into();
        assert_only(MirrorField::Title, &title);

        let mut body = t.clone();
        body.body = "changed".into();
        assert_only(MirrorField::Body, &body);

        let mut status = t.clone();
        status.status = TaskStatus::Done;
        assert_only(MirrorField::Status, &status);

        let mut assignees = t.clone();
        assignees.assignees = vec!["bob".into()];
        assert_only(MirrorField::Assignees, &assignees);
    }

    #[test]
    fn mirror_field_assignees_ignores_order() {
        let mut t = synced();
        t.set_assignees(vec!["alice".into(), "bob".into()]);
        t.confirm_synced(SnapshotSource::Push).unwrap();
        let baseline = t.synced_baseline.clone().expect("baseline");

        // A pure re-order is not a difference…
        let mut reordered = t.clone();
        reordered.assignees = vec!["bob".into(), "alice".into()];
        assert!(!MirrorField::Assignees.differs(&reordered, &baseline));

        // …and reconciling a re-order keeps the task Synced (no spurious flip).
        t.set_assignees(vec!["bob".into(), "alice".into()]);
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn each_mirrored_field_edit_flips_dirty_local() {
        // snapshot_view captures every mirrored field into the baseline, so an
        // edit to any of them is detectable through reconcile — proving the
        // captured-baseline set and the comparator set agree.
        {
            let mut t = synced();
            t.set_title("changed".into()).unwrap();
            assert_eq!(t.sync, SyncState::DirtyLocal, "title");
        }
        {
            let mut t = synced();
            t.set_body("changed".into());
            assert_eq!(t.sync, SyncState::DirtyLocal, "body");
        }
        {
            let mut t = synced();
            t.start().unwrap(); // Open → InProgress
            assert_eq!(t.sync, SyncState::DirtyLocal, "status");
        }
        {
            let mut t = synced();
            t.set_assignees(vec!["zoe".into()]);
            assert_eq!(t.sync, SyncState::DirtyLocal, "assignees");
        }
    }

    #[test]
    fn diff_against_baseline_empty_without_baseline() {
        // Never promoted ⇒ no baseline ⇒ empty patch (mirrors reconcile's no-op).
        let t = draft();
        assert!(t.synced_baseline.is_none());
        assert!(t.diff_against_baseline().is_empty());
    }

    #[test]
    fn diff_against_baseline_empty_when_clean() {
        let t = synced();
        let patch = t.diff_against_baseline();
        assert!(patch.is_empty());
        assert_eq!(patch, MirrorPatch::default());
    }

    #[test]
    fn diff_against_baseline_isolates_single_field() {
        // Title.
        let mut t = synced();
        t.set_title("new title".into()).unwrap();
        let p = t.diff_against_baseline();
        assert_eq!(p.title.as_deref(), Some("new title"));
        assert!(p.body.is_none() && p.status.is_none() && p.assignees.is_none());
        assert!(!p.is_empty());

        // Body.
        let mut t = synced();
        t.set_body("new body".into());
        let p = t.diff_against_baseline();
        assert_eq!(p.body.as_deref(), Some("new body"));
        assert!(p.title.is_none() && p.status.is_none() && p.assignees.is_none());

        // Status (Open → InProgress).
        let mut t = synced();
        t.start().unwrap();
        let p = t.diff_against_baseline();
        assert_eq!(p.status, Some(TaskStatus::InProgress));
        assert!(p.title.is_none() && p.body.is_none() && p.assignees.is_none());

        // Assignees.
        let mut t = synced();
        t.set_assignees(vec!["alice".into()]);
        let p = t.diff_against_baseline();
        assert_eq!(p.assignees, Some(vec!["alice".to_string()]));
        assert!(p.title.is_none() && p.body.is_none() && p.status.is_none());
    }

    #[test]
    fn diff_against_baseline_multi_field() {
        let mut t = synced();
        t.set_title("t2".into()).unwrap();
        t.set_assignees(vec!["bob".into()]);
        let p = t.diff_against_baseline();
        assert_eq!(p.title.as_deref(), Some("t2"));
        assert_eq!(p.assignees, Some(vec!["bob".to_string()]));
        assert!(p.body.is_none() && p.status.is_none());
    }

    #[test]
    fn diff_against_baseline_ignores_assignee_reorder() {
        let mut t = synced();
        t.set_assignees(vec!["alice".into(), "bob".into()]);
        t.confirm_synced(SnapshotSource::Push).unwrap();
        // Pure re-order ⇒ not a diff (reuses assignees_equal).
        t.set_assignees(vec!["bob".into(), "alice".into()]);
        assert!(t.diff_against_baseline().assignees.is_none());
        assert!(t.diff_against_baseline().is_empty());
    }

    #[test]
    fn diff_emptiness_agrees_with_dirty_detection() {
        // The patch is non-empty exactly when reconcile flips DirtyLocal.
        let mut t = synced();
        assert!(t.diff_against_baseline().is_empty());
        assert_eq!(t.sync, SyncState::Synced);

        t.set_body("changed".into());
        assert!(!t.diff_against_baseline().is_empty());
        assert_eq!(t.sync, SyncState::DirtyLocal);

        // Revert to baseline value ⇒ clean again, patch empty again.
        t.set_body(String::new());
        assert!(t.diff_against_baseline().is_empty());
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn confirm_synced_fields_title_only_rebaselines_only_title() {
        // Title-only patch: only the title baseline entry moves; body,
        // status, and assignees stay byte-identical to the pre-call baseline.
        let mut t = synced();
        t.set_title("new title".into()).unwrap();
        let pre = t.synced_baseline.clone().expect("baseline");
        let patch = t.diff_against_baseline();
        assert_eq!(patch.title.as_deref(), Some("new title"));
        assert!(patch.body.is_none() && patch.status.is_none() && patch.assignees.is_none());

        t.confirm_synced_fields(SnapshotSource::Push, &patch)
            .unwrap();

        assert_eq!(t.sync, SyncState::Synced);
        let post = t.synced_baseline.clone().expect("baseline");
        assert_eq!(post.title, "new title");
        assert_eq!(post.body, pre.body, "body baseline entry must be unchanged");
        assert_eq!(
            post.status, pre.status,
            "status baseline entry must be unchanged"
        );
        assert_eq!(
            post.assignees, pre.assignees,
            "assignees baseline entry must be unchanged"
        );
        assert_eq!(
            post.source,
            SnapshotSource::Push,
            "source stamped on merged baseline"
        );
    }

    #[test]
    fn confirm_synced_fields_each_field_isolated() {
        // Body-only, status-only, assignees-only: each patches a single
        // field; the other three baseline entries stay byte-identical.
        for edit in [
            (
                "body",
                Box::new(|t: &mut Task| t.set_body("revised".into())) as Box<dyn Fn(&mut Task)>,
            ),
            (
                "status",
                Box::new(|t: &mut Task| {
                    t.start().unwrap();
                }),
            ),
            (
                "assignees",
                Box::new(|t: &mut Task| {
                    t.set_assignees(vec!["zoe".into()]);
                }),
            ),
        ] {
            let (label, mutate) = edit;
            let mut t = synced();
            mutate(&mut t);
            let pre = t.synced_baseline.clone().expect("baseline");
            let patch = t.diff_against_baseline();
            assert_eq!(
                patch,
                {
                    let mut p = MirrorPatch::default();
                    match label {
                        "body" => p.body = Some("revised".into()),
                        "status" => p.status = Some(TaskStatus::InProgress),
                        "assignees" => p.assignees = Some(vec!["zoe".into()]),
                        _ => unreachable!(),
                    }
                    p
                },
                "only {label} in the patch"
            );

            t.confirm_synced_fields(SnapshotSource::Push, &patch)
                .unwrap();

            assert_eq!(t.sync, SyncState::Synced);
            let post = t.synced_baseline.clone().expect("baseline");
            // The edited field carries the new value; the other three are
            // byte-identical to pre.
            match label {
                "body" => {
                    assert_eq!(post.body, "revised");
                    assert_eq!(post.title, pre.title);
                    assert_eq!(post.status, pre.status);
                    assert_eq!(post.assignees, pre.assignees);
                }
                "status" => {
                    assert_eq!(post.status, TaskStatus::InProgress);
                    assert_eq!(post.title, pre.title);
                    assert_eq!(post.body, pre.body);
                    assert_eq!(post.assignees, pre.assignees);
                }
                "assignees" => {
                    assert_eq!(post.assignees, vec!["zoe".to_string()]);
                    assert_eq!(post.title, pre.title);
                    assert_eq!(post.body, pre.body);
                    assert_eq!(post.status, pre.status);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn confirm_synced_fields_full_patch_matches_full_rebaseline() {
        // All four `Some` in the patch: the merged baseline must equal
        // the live task and behave identically to a full
        // `confirm_synced(SnapshotSource::Push)` rebaseline.
        let mut t = synced();
        t.set_title("full t".into()).unwrap();
        t.set_body("full b".into());
        t.start().unwrap();
        t.set_assignees(vec!["alice".into(), "bob".into()]);
        let patch = t.diff_against_baseline();
        assert!(!patch.is_empty());

        t.confirm_synced_fields(SnapshotSource::Push, &patch)
            .unwrap();

        assert_eq!(t.sync, SyncState::Synced);
        let post = t.synced_baseline.clone().expect("baseline");
        assert_eq!(post.title, "full t");
        assert_eq!(post.body, "full b");
        assert_eq!(post.status, TaskStatus::InProgress);
        assert_eq!(post.assignees, vec!["alice".to_string(), "bob".to_string()]);
        // The patch was the WHOLE diff, so reconcile sees no remaining
        // delta and the task stays Synced.
        assert!(t.diff_against_baseline().is_empty());
    }

    #[test]
    fn confirm_synced_fields_empty_patch_flips_state_baseline_unchanged() {
        // Empty patch on a Staged task: the state flips to Synced (a
        // no-op drain still owes the state transition) but no baseline
        // entry is touched. The captured `source` is stamped on the
        // unchanged-baseline entry so the audit trail records the drain.
        //
        // Path: synced() (Synced + baseline) → set_title("edit") flips
        // to DirtyLocal → stage_for_sync() flips to Staged (Staged
        // survives any further reconcile, so we can revert the title
        // to its baseline value WITHOUT the state machine bouncing us
        // back to Synced) → diff is now empty + state is Staged, the
        // exact precondition a no-op drain delivers.
        let mut t = synced();
        t.set_title("stale edit".into()).unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.stage_for_sync().unwrap();
        assert_eq!(t.sync, SyncState::Staged);
        t.set_title(t.synced_baseline.as_ref().unwrap().title.clone())
            .unwrap();
        assert_eq!(t.sync, SyncState::Staged, "Staged survives reconcile");
        let pre = t.synced_baseline.clone().expect("baseline");
        let patch = t.diff_against_baseline();
        assert!(patch.is_empty(), "no field differs from baseline");

        t.confirm_synced_fields(SnapshotSource::Push, &patch)
            .unwrap();

        assert_eq!(t.sync, SyncState::Synced);
        let post = t.synced_baseline.clone().expect("baseline");
        assert_eq!(post.title, pre.title);
        assert_eq!(post.body, pre.body);
        assert_eq!(post.status, pre.status);
        assert_eq!(post.assignees, pre.assignees);
        assert_eq!(post.source, SnapshotSource::Push);
    }

    #[test]
    fn confirm_synced_fields_keeps_unrebaselined_fields_dirty() {
        // The load-bearing silent-loss-fix assertion: a title-only patch on
        // a task that ALSO has un-pushed body and assignee edits must keep
        // the task DirtyLocal after confirm (the un-rebaselined fields
        // still differ, so reconcile_dirty_against_baseline must flip it
        // back). The next push will see the un-pushed fields and re-send
        // them. Without this guarantee, a partial push would silently
        // "succeed" and the un-pushed fields would never reach GitHub.
        let mut t = synced();
        t.set_title("pushed title".into()).unwrap();
        t.set_body("pushed body".into());
        t.set_assignees(vec!["carol".into()]);
        let title_patch = MirrorPatch {
            title: Some("pushed title".into()),
            ..Default::default()
        };
        assert!(title_patch.body.is_none());
        assert!(title_patch.assignees.is_none());

        t.confirm_synced_fields(SnapshotSource::Push, &title_patch)
            .unwrap();

        // Title is rebaselined; body and assignees are NOT.
        assert_eq!(
            t.sync,
            SyncState::DirtyLocal,
            "un-rebaselined body/assignees must keep the task dirty"
        );
        let post = t.synced_baseline.clone().expect("baseline");
        assert_eq!(post.title, "pushed title");
        assert_ne!(
            post.body, "pushed body",
            "body baseline must NOT have been rebaselined by a title-only patch"
        );
        assert_ne!(
            post.assignees,
            vec!["carol".to_string()],
            "assignees baseline must NOT have been rebaselined by a title-only patch"
        );

        // The un-pushed fields are still detectable as a diff.
        let next = t.diff_against_baseline();
        assert!(next.body.is_some() && next.assignees.is_some());
        assert!(next.title.is_none(), "title is already rebaselined");
    }

    #[test]
    fn confirm_synced_fields_invalid_source_errors() {
        let mut t = synced();
        t.set_body("changed".into());
        let patch = t.diff_against_baseline();
        // Non-transmitting sources must be rejected: LocalEdit /
        // PrePull / Rollback are not baseline-eligible, and Link
        // (baseline-eligible per `is_baseline`) is also rejected
        // because there is no PATCH payload to merge per-field —
        // callers wanting a relink rebaseline must use the
        // full-snapshot `confirm_synced` instead.
        for bad in [
            SnapshotSource::LocalEdit,
            SnapshotSource::PrePull,
            SnapshotSource::Rollback,
            SnapshotSource::Created,
            SnapshotSource::Link,
        ] {
            let err = t
                .confirm_synced_fields(bad, &patch)
                .expect_err("non-transmitting source must be rejected");
            assert!(
                format!("{err}").contains("confirm_synced_fields source"),
                "unexpected error for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn confirm_synced_fields_invalid_state_errors() {
        // Synced is NOT a confirmable state (confirm_synced / confirm_
        // synced_fields only accept Staged | DirtyLocal | DirtyRemote).
        let mut t = synced();
        let patch = MirrorPatch::default();
        let err = t
            .confirm_synced_fields(SnapshotSource::Push, &patch)
            .expect_err("Synced must be rejected");
        assert!(
            format!("{err}").contains("from sync=Synced"),
            "unexpected error: {err}"
        );

        // LocalOnly is not confirmable either.
        let mut t = draft();
        assert_eq!(t.sync, SyncState::LocalOnly);
        assert!(t.synced_baseline.is_none());
        let err = t
            .confirm_synced_fields(SnapshotSource::Push, &MirrorPatch::default())
            .expect_err("LocalOnly must be rejected");
        assert!(
            format!("{err}").contains("from sync=LocalOnly"),
            "unexpected error: {err}"
        );
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
        // No filing repo was ever recorded (`filing_repo_id == None`), so
        // nothing pins the issue's home — reassigning the logical repo would
        // orphan it. This is the canonical "filing not recorded" reject (#164).
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
    fn set_repo_id_allowed_when_remote_backed_with_distinct_filing() {
        // Post-RFC-0002: the backing issue lives in the recorded DISTINCT
        // filing repo, so the logical repo is free to move without orphaning
        // it.
        let mut t = draft();
        let logical = RepoId::new();
        let filing = RepoId::new();
        t.set_repo_id(Some(logical)).unwrap();
        t.set_filing_repo_id(Some(filing)).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        let new_logical = RepoId::new();
        t.set_repo_id(Some(new_logical)).unwrap();
        assert_eq!(t.repo_id, Some(new_logical));
        // Filing repo (the issue's home) is untouched, and a logical-repo move
        // is local metadata — no spurious DirtyLocal flip.
        assert_eq!(t.filing_repo_id, Some(filing));
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn set_repo_id_allowed_when_remote_backed_and_filing_equals_logical() {
        // A recorded filing repo pins the issue's home even when it EQUALS the
        // logical repo (e.g. `sync import --cascade` rows). The logical repo is
        // then free to move — `filing_repo_id != repo_id` is irrelevant; only
        // `filing_repo_id IS NOT NULL` matters (#164).
        let mut t = draft();
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        t.set_filing_repo_id(Some(repo)).unwrap(); // filing collapsed onto logical
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        let new_logical = RepoId::new();
        t.set_repo_id(Some(new_logical)).unwrap();
        assert_eq!(t.repo_id, Some(new_logical));
        // Filing repo (the issue's home) is untouched, and a logical-repo move
        // is local metadata — no spurious DirtyLocal flip.
        assert_eq!(t.filing_repo_id, Some(repo));
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn set_repo_id_recollapse_onto_filing_allowed() {
        // With a distinct filing recorded, moving the logical repo to EQUAL the
        // filing repo is allowed (re-collapse): the recorded filing stays
        // authoritative and is never touched.
        let mut t = draft();
        let logical = RepoId::new();
        let filing = RepoId::new();
        t.set_repo_id(Some(logical)).unwrap();
        t.set_filing_repo_id(Some(filing)).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        t.set_repo_id(Some(filing)).unwrap();
        assert_eq!(t.repo_id, Some(filing));
        assert_eq!(t.filing_repo_id, Some(filing));
        // Re-collapse is still a logical-repo move — local metadata, no flip.
        assert_eq!(t.sync, SyncState::Synced);
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
    fn snapshot_view_captures_filing_repo_id() {
        // RFC 0002 #118: a baseline-eligible snapshot carries the resolved
        // filing repo so promote/push/pull/conflict/link history records it.
        let mut t = draft();
        // Fresh draft: filing repo unresolved → snapshot records None.
        assert_eq!(
            t.snapshot_view(SnapshotSource::Created).filing_repo_id,
            None
        );

        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        let repo = RepoId::new();
        t.set_filing_repo_id(Some(repo)).unwrap();
        assert_eq!(
            t.snapshot_view(SnapshotSource::Promote).filing_repo_id,
            Some(repo),
            "snapshot_view must capture the resolved filing repo"
        );
    }

    #[test]
    fn filing_repo_id_not_in_dirty_diff() {
        // RFC 0002 #118: filing repo is history/audit only — recording it must
        // NOT mark a Synced task DirtyLocal (it is excluded from
        // reconcile_dirty_against_baseline).
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        t.set_filing_repo_id(Some(RepoId::new())).unwrap();
        // A reconcile (driven here by a no-op body set) must leave it Synced.
        let body = t.body.clone();
        t.set_body(body);
        assert_eq!(
            t.sync,
            SyncState::Synced,
            "recording the filing repo must not flip the task DirtyLocal"
        );
    }

    #[test]
    fn import_mirror_records_filing_repo_as_logical() {
        // An imported mirror already has a remote issue and historically
        // filing == logical, so the constructor records the filing repo as
        // the logical one (keeps the written row in step with the D6 dedup).
        // The value is stored as `Some(repo)`, never collapsed to `None` on
        // equality — `task show`'s additive `filing_repo` overlay relies on
        // this to surface the recorded filing repo even when it equals the
        // logical repo (#164 secondary display guarantee).
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
