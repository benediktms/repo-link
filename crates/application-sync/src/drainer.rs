//! [`OutboxDrainer`] — the asynchronous outbound path for mirror tasks
//! (RFC 0001 Stage 6, #54).
//!
//! # Model
//!
//! Lifecycle / edit commands on a mirror task enqueue an [`OutboxEntry`]
//! (see `application-task` / `application-workspace`); the drainer applies
//! those entries against GitHub. The daemon drives [`OutboxDrainer::drain_once`]
//! on every tick — it is the **sole** outbound path the daemon uses (the
//! synchronous `SyncService::push` is retained only for `rl task claim`'s
//! interactive feedback).
//!
//! # Ordering contract
//!
//! - **Per-task FIFO.** A single task's `start → edit → complete` sequence
//!   applies in enqueue order. Enforced at the repository: a claim never
//!   returns an entry for a task that has an earlier-enqueued non-terminal
//!   sibling — neither an `inflight` entry NOR an older `pending` one. The
//!   older-pending half is load-bearing: when a head fails recoverably it goes
//!   back to `pending` with a *future* `next_attempt_at` (not eligible), and
//!   its tail must wait behind it rather than overtake it. Combined with
//!   `drain_once` processing one entry fully (success / retry / dead-letter)
//!   before the next claim, a task's head is always resolved (or backing off)
//!   before its tail is eligible.
//! - **Parallel across tasks.** A stuck or failing head on task A (now
//!   `pending` with a future `next_attempt_at`, or dead-lettered) never
//!   blocks task B — the claim simply skips A and returns B's head.
//! - **Capped retry with exponential backoff + dead-letter.** A recoverable
//!   provider error reschedules the entry (`attempts += 1`, `next_attempt_at =
//!   now + backoff(attempts)`, back to `pending`) while `attempts + 1 <
//!   max_attempts`; at the cap it dead-letters (`status = failed`, terminal,
//!   surfaced by `rl sync outbox`).
//!
//! # Write-backs
//!
//! - `AddItem` writes the returned `PVTI_…` onto `task.project_item_id` and
//!   enqueues a follow-up `SetProjectStatus` (two-phase: attach now, set the
//!   column on the next drain pass once we know the item id). The write-back is
//!   idempotent to a detach (#54): if the task's workspace no longer targets
//!   the project the `AddItem` was for (detached / moved while the entry was
//!   inflight), the local write-back is DISCARDED — persisting it would
//!   re-anchor the task to the board it just left. The entry still succeeds;
//!   the remote board item is intentionally left as-is (remote board cleanup is
//!   a separate concern).
//! - `CreateDraftIssue` writes the returned item id onto `project_item_id` and
//!   likewise enqueues a `SetProjectStatus` follow-up.
//! - `ConvertDraftToIssue` writes the returned issue node id AND its REST
//!   `number` onto `task.remote` (a fully-populated `RemoteRef`) so subsequent
//!   GraphQL mutations have a node-id address and REST `UpdateRemote` has the
//!   number — never a half-populated issue-backed ref with an empty
//!   `remote_id`.
//!
//! The resolving write-back (`project_item_id` / `remote`) is always
//! persisted by [`Self::apply`] *before* [`OutboxRepository::mark_succeeded`]
//! flips the entry terminal — `drain_once` only marks succeeded once `apply`
//! returns `Ok`. So a crash between the remote call and the write-back leaves
//! the entry un-marked and replayable, while a crash after the write-back but
//! before `mark_succeeded` replays an entry whose task already reflects the
//! remote result (handled by the guards below).
//!
//! # Delivery semantics: at-least-once + idempotency guards
//!
//! The GitHub ↔ SQLite boundary cannot be exactly-once: `claim_next_eligible`
//! flips an entry to `inflight` in a committed transaction *before*
//! [`Self::apply`] runs, and `requeue_orphaned_inflight` resets any stranded
//! `inflight` row back to `pending` on the next startup. So an entry whose
//! remote call succeeded but whose `write_back_*` / `mark_succeeded` never
//! committed (crash, kill) is **replayed**. The honest model is at-least-once
//! delivery + per-mutation idempotency + dead-letter, not exactly-once.
//!
//! Most mutations are naturally idempotent: `addProjectV2ItemById` (`AddItem`)
//! returns the same item, `SetProjectStatus` is a write-the-same-value, and
//! `UpdateRemote` / `UpdateDraftIssue` re-push identical content. Two are NOT,
//! and carry explicit guards in [`Self::apply`]:
//! - **`CreateDraftIssue`** — a blind replay would mint a *second* draft. The
//!   guard skips the remote create when the task already carries a
//!   `project_item_id` (the create already landed) and proceeds to the
//!   follow-up `SetProjectStatus` using that id.
//! - **`ConvertDraftToIssue`** — re-running the convert on an already-converted
//!   item misbehaves. The guard skips the remote call when the task already
//!   carries a real issue node id (`remote.node_id` set).

use std::sync::Arc;
use std::time::Duration;

use domain_core::Timestamp;
use domain_sync::{OutboxEntry, OutboxMutation};
use domain_task::{RemoteRef, SnapshotSource, SyncState};
use ports::{
    OutboxRepository, ProjectRepository, RemoteProjectProvider, RemoteTaskProvider, TaskRepository,
    WorkspaceRepository,
};
use tracing::{debug, info, warn};

use crate::error::Result;

/// Exponential backoff schedule + attempt cap for the drainer's retry policy
/// (RFC 0001 §10.2). `delays[i]` is the wait before attempt `i + 2` (the
/// first retry uses `delays[0]`); past the end of the slice the last delay is
/// reused. `max_attempts` is the total number of tries before dead-lettering.
#[derive(Clone, Debug)]
pub struct BackoffSchedule {
    delays: Vec<Duration>,
    max_attempts: u32,
}

impl BackoffSchedule {
    /// The Stage-6 default: 5s, 30s, 2m, 10m, capped at 5 attempts.
    pub fn standard() -> Self {
        Self {
            delays: vec![
                Duration::from_secs(5),
                Duration::from_secs(30),
                Duration::from_secs(2 * 60),
                Duration::from_secs(10 * 60),
            ],
            max_attempts: 5,
        }
    }

    /// Custom schedule (used by tests to make backoff observable without
    /// real waits). `max_attempts` is clamped to at least 1.
    pub fn new(delays: Vec<Duration>, max_attempts: u32) -> Self {
        Self {
            delays,
            max_attempts: max_attempts.max(1),
        }
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Backoff before the next attempt given the count of attempts already
    /// made (the entry's `attempts` field after this failure is recorded).
    /// `attempts_made == 1` ⇒ `delays[0]`.
    fn delay_for(&self, attempts_made: u32) -> Duration {
        if self.delays.is_empty() {
            return Duration::from_secs(0);
        }
        let idx = (attempts_made.saturating_sub(1)) as usize;
        let idx = idx.min(self.delays.len() - 1);
        self.delays[idx]
    }
}

impl Default for BackoffSchedule {
    fn default() -> Self {
        Self::standard()
    }
}

pub struct OutboxDrainer {
    outbox: Arc<dyn OutboxRepository>,
    tasks: Arc<dyn TaskRepository>,
    workspaces: Arc<dyn WorkspaceRepository>,
    projects: Arc<dyn ProjectRepository>,
    remote_tasks: Arc<dyn RemoteTaskProvider>,
    remote_projects: Arc<dyn RemoteProjectProvider>,
    backoff: BackoffSchedule,
}

impl OutboxDrainer {
    pub fn new(
        outbox: Arc<dyn OutboxRepository>,
        tasks: Arc<dyn TaskRepository>,
        workspaces: Arc<dyn WorkspaceRepository>,
        projects: Arc<dyn ProjectRepository>,
        remote_tasks: Arc<dyn RemoteTaskProvider>,
        remote_projects: Arc<dyn RemoteProjectProvider>,
    ) -> Self {
        Self {
            outbox,
            tasks,
            workspaces,
            projects,
            remote_tasks,
            remote_projects,
            backoff: BackoffSchedule::standard(),
        }
    }

    pub fn with_backoff(mut self, backoff: BackoffSchedule) -> Self {
        self.backoff = backoff;
        self
    }

    /// Claim and apply eligible entries until none remain eligible *this
    /// pass*. Returns the count of entries marked succeeded. Entries that
    /// fail recoverably are rescheduled (and not re-attempted this pass,
    /// since their `next_attempt_at` is now in the future); entries at the
    /// cap are dead-lettered. A claim that returns the same task again can't
    /// happen mid-pass because the head stays `inflight` until resolved here.
    pub async fn drain_once(&self) -> Result<usize> {
        let mut succeeded = 0usize;
        loop {
            let now = Timestamp::now();
            let Some(entry) = self.outbox.claim_next_eligible(now).await? else {
                break;
            };
            match self.apply(&entry).await {
                Ok(()) => {
                    self.outbox.mark_succeeded(entry.id).await?;
                    succeeded += 1;
                    info!(entry_id = %entry.id, task_id = %entry.task_id, kind = entry.mutation.kind(), "outbox entry drained");
                }
                Err(e) => {
                    let msg = e.to_string();
                    // `attempts` is the count *before* this failure; the repo
                    // bumps it. After bumping it becomes `attempts + 1`, so the
                    // cap test is "would the bumped count reach max_attempts?".
                    let attempts_after = entry.attempts + 1;
                    if attempts_after < self.backoff.max_attempts {
                        let delay = self.backoff.delay_for(attempts_after);
                        // Compute the backoff window from a FRESH timestamp at
                        // FAILURE time, not the claim-time `now` (#54). The
                        // remote call / error path can take longer than the
                        // backoff delay; reusing claim-time `now` would schedule
                        // `next_attempt_at` in the past, making the entry
                        // immediately re-claimable and defeating the backoff.
                        let failed_at = Timestamp::now();
                        let next_attempt_at = Timestamp::from_utc(
                            failed_at.into_inner()
                                + chrono::Duration::from_std(delay).unwrap_or_default(),
                        );
                        self.outbox
                            .record_retry(entry.id, &msg, next_attempt_at)
                            .await?;
                        warn!(entry_id = %entry.id, task_id = %entry.task_id, attempts = attempts_after, error = %msg, "outbox entry rescheduled");
                    } else {
                        self.outbox.mark_failed(entry.id, &msg).await?;
                        warn!(entry_id = %entry.id, task_id = %entry.task_id, attempts = attempts_after, error = %msg, "outbox entry dead-lettered");
                    }
                }
            }
        }
        Ok(succeeded)
    }

    /// Route one entry to the right provider and apply its write-backs.
    async fn apply(&self, entry: &OutboxEntry) -> Result<()> {
        match &entry.mutation {
            OutboxMutation::UpdateRemote {
                canonical_repo,
                remote_id,
                title: _,
                body: _,
                closed: _,
            } => {
                // Re-derive from the live task + its baseline. The
                // shared helper decides which fields ride the PATCH
                // and when to skip (empty patch ⇒ no PATCH, entry
                // still succeeds). Per-task FIFO means reading the
                // live task + its baseline cannot race a coalesced
                // sibling; the payload's captured fields are ignored.
                //
                // Re-baseline on success (RFC 0003 D5, rpl-xq6):
                // advance the baseline ONLY for the fields actually
                // transmitted in the patch, via
                // `confirm_synced_fields` — a field whose channel is
                // incomplete (e.g. an older push that didn't carry
                // assignees) must stay dirty and get re-pushed later,
                // instead of being silently rebaselined to the
                // un-pushed local value. We confirm + save
                // unconditionally when the task is in a confirmable
                // state (Staged | DirtyLocal | DirtyRemote), including
                // the empty-patch path: a Staged task drained with no
                // live diff still owes the Staged → Synced transition.
                //
                // Gating on confirmable state: a `Synced` task drained
                // for an `UpdateRemote` (e.g. a coalesced empty head,
                // a now-reverted local edit) is a no-op — the baseline
                // is already aligned, the state is already Synced, and
                // the entry just needs to mark succeeded. Calling
                // `confirm_synced_fields` on a `Synced` task would
                // error on the state guard and the entry would back
                // off behind a non-issue, head-of-line blocking the
                // next same-task tail. Non-confirmable states
                // (`LocalOnly` / `Conflict`) skip silently for the
                // same reason.
                let mut task = self.tasks.get(entry.task_id).await?;
                let confirmable = matches!(
                    task.sync,
                    SyncState::Staged | SyncState::DirtyLocal | SyncState::DirtyRemote
                );
                if confirmable {
                    let patch = task.diff_against_baseline();
                    if let Some(update) =
                        crate::build_update_from_patch(&task, &patch, canonical_repo, remote_id)
                    {
                        self.remote_tasks.update_remote(update).await?;
                    }
                    // Never re-baseline from the PATCH response:
                    // octocrab's `update_issue` returns assignees=[]
                    // for a PATCH that didn't set them, and
                    // re-baselining from it would clobber local
                    // assignee intent. The `patch` carries the
                    // transmitted set, which is what we merge over
                    // the existing baseline.
                    task.confirm_synced_fields(SnapshotSource::Push, &patch)?;
                    self.tasks.save(&task, SnapshotSource::Push).await?;
                }
            }
            OutboxMutation::AddItem {
                project_node_id,
                issue_node_id,
            } => {
                let item_id = self
                    .remote_projects
                    .add_item(project_node_id, issue_node_id)
                    .await?;
                // Detach-idempotent write-back (#54): an `AddItem` already
                // `inflight` can finish AFTER its workspace detached from (or
                // moved off) `project_node_id`. Persisting `project_item_id`
                // here would re-anchor the task to the board it just left,
                // silently undoing the detach. Re-validate that the task's
                // workspace STILL targets this project before writing back; if
                // it no longer matches, DISCARD the local write-back (and the
                // SetProjectStatus follow-up) and let the entry succeed — the
                // remote board item is intentionally left as-is (remote board
                // cleanup is a separate concern, per §10.5).
                if self
                    .workspace_still_targets_project(entry.task_id, project_node_id)
                    .await?
                {
                    self.write_back_project_item(entry.task_id, &item_id)
                        .await?;
                    self.enqueue_status_follow_up(entry.task_id, project_node_id, &item_id)
                        .await?;
                } else {
                    debug!(
                        task_id = %entry.task_id,
                        project_node_id = %project_node_id,
                        "add-item write-back skipped: workspace no longer targets this project (detached/moved)"
                    );
                }
            }
            OutboxMutation::CreateDraftIssue {
                project_node_id,
                title,
                body,
            } => {
                // Idempotency guard (at-least-once delivery): a crash after the
                // remote create but before `mark_succeeded` re-runs this entry
                // on startup (`requeue_orphaned_inflight`). Unlike GitHub's
                // idempotent `addProjectV2ItemById`, `createDraftIssue` would
                // mint a SECOND draft. If the task already carries a
                // `project_item_id`, the create already landed — skip the
                // remote call and proceed straight to the follow-up using the
                // known item id.
                let task = self.tasks.get(entry.task_id).await?;
                let item_id = match task.project_item_id.clone() {
                    Some(existing) => {
                        debug!(
                            task_id = %entry.task_id,
                            item_id = %existing,
                            "create-draft replay: task already has a project_item_id; skipping create"
                        );
                        existing
                    }
                    None => {
                        let minted = self
                            .remote_projects
                            .create_draft_issue(project_node_id, title, body)
                            .await?;
                        self.write_back_project_item(entry.task_id, &minted).await?;
                        minted
                    }
                };
                self.enqueue_status_follow_up(entry.task_id, project_node_id, &item_id)
                    .await?;
            }
            OutboxMutation::UpdateDraftIssue {
                item_node_id,
                title: _,
                body: _,
            } => {
                // Re-derive title/body from the *current* task (mirroring the
                // `UpdateRemote` arm), NOT the payload snapshotted at enqueue
                // time. A `DirtyLocal` draft-backed mirror coalesces later
                // title/body edits without a new outbox row, so the pending
                // entry's captured payload can be stale — pushing it would let
                // the newest edit never reach GitHub. Per-task FIFO guarantees
                // at most one in-flight UpdateDraftIssue, so reading live
                // content can't race a coalesced sibling. The payload's
                // captured title/body are intentionally ignored.
                let task = self.tasks.get(entry.task_id).await?;
                self.remote_projects
                    .update_draft_issue(item_node_id, Some(&task.title), Some(&task.body))
                    .await?;
            }
            OutboxMutation::ConvertDraftToIssue {
                item_node_id,
                repo_node_id,
            } => {
                // Idempotency guard (at-least-once delivery): a crash after the
                // remote convert but before `mark_succeeded` replays this entry
                // on startup. Re-running `convertProjectV2DraftIssueItemToIssue`
                // on an already-converted item misbehaves, so if the task
                // already carries a real issue node id the convert already
                // landed — skip the remote call.
                let task = self.tasks.get(entry.task_id).await?;
                if task
                    .remote
                    .as_ref()
                    .and_then(|r| r.node_id.as_deref())
                    .is_some()
                {
                    debug!(
                        task_id = %entry.task_id,
                        "convert-draft replay: task already has an issue node id; skipping convert"
                    );
                } else {
                    let (issue_node_id, issue_number) = self
                        .remote_projects
                        .convert_draft_to_issue(item_node_id, repo_node_id)
                        .await?;
                    self.write_back_converted_issue(entry.task_id, &issue_node_id, issue_number)
                        .await?;
                }
            }
            OutboxMutation::SetProjectStatus {
                project_node_id,
                item_node_id,
                status_field_id,
                option_id,
            } => {
                self.remote_projects
                    .set_status(project_node_id, item_node_id, status_field_id, option_id)
                    .await?;
            }
            // Relation-sync arms (#95/#96). These address the GitHub-native
            // primitives directly via REST; the adapter resolves the *related*
            // issue's integer db id at apply time (the entry carries only the
            // offline-known `#number`). The provider calls are idempotent
            // (already-linked / already-gone collapse to success), so an
            // at-least-once redelivery after a crash is safe with no extra
            // guard.
            OutboxMutation::AddSubIssue {
                parent_canonical,
                parent_remote_id,
                child_canonical,
                child_remote_id,
            } => {
                self.remote_tasks
                    .add_sub_issue(
                        parent_canonical,
                        parent_remote_id,
                        child_canonical,
                        child_remote_id,
                    )
                    .await?;
            }
            OutboxMutation::RemoveSubIssue {
                parent_canonical,
                parent_remote_id,
                child_canonical,
                child_remote_id,
            } => {
                self.remote_tasks
                    .remove_sub_issue(
                        parent_canonical,
                        parent_remote_id,
                        child_canonical,
                        child_remote_id,
                    )
                    .await?;
            }
            OutboxMutation::AddBlockedBy {
                blocked_canonical,
                blocked_remote_id,
                blocker_canonical,
                blocker_remote_id,
            } => {
                self.remote_tasks
                    .add_blocked_by(
                        blocked_canonical,
                        blocked_remote_id,
                        blocker_canonical,
                        blocker_remote_id,
                    )
                    .await?;
            }
            OutboxMutation::RemoveBlockedBy {
                blocked_canonical,
                blocked_remote_id,
                blocker_canonical,
                blocker_remote_id,
            } => {
                self.remote_tasks
                    .remove_blocked_by(
                        blocked_canonical,
                        blocked_remote_id,
                        blocker_canonical,
                        blocker_remote_id,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    /// Persist `item_id` onto `task.project_item_id`. The save uses a
    /// `LocalEdit` snapshot source so it never flips sync state — this is a
    /// cache write-back, not a remote-alignment event.
    async fn write_back_project_item(
        &self,
        task_id: domain_core::TaskId,
        item_id: &str,
    ) -> Result<()> {
        let mut task = self.tasks.get(task_id).await?;
        if task.project_item_id.as_deref() != Some(item_id) {
            task.project_item_id = Some(item_id.to_string());
            self.tasks.save(&task, SnapshotSource::LocalEdit).await?;
        }
        Ok(())
    }

    /// Persist the FULLY-populated `RemoteRef` returned by a draft→issue
    /// conversion: `node_id` (the new issue's `I_…`, for GraphQL mutations)
    /// and `remote_id` (the issue's REST `number`, for `UpdateRemote`). The
    /// number is what keeps the invariant — no issue-backed `RemoteRef` with
    /// an empty `remote_id` is ever persisted, so a later lifecycle/content
    /// edit can't enqueue an `UpdateRemote` against an unaddressable issue
    /// (#54).
    async fn write_back_converted_issue(
        &self,
        task_id: domain_core::TaskId,
        issue_node_id: &str,
        issue_number: u64,
    ) -> Result<()> {
        let mut task = self.tasks.get(task_id).await?;
        // Preserve the existing provider if the task already had one; default
        // to "github" otherwise (the only provider that converts drafts).
        let provider = task
            .remote
            .as_ref()
            .map(|r| r.provider.clone())
            .unwrap_or_else(|| "github".to_string());
        task.remote = Some(RemoteRef {
            provider,
            remote_id: issue_number.to_string(),
            node_id: Some(issue_node_id.to_string()),
        });
        self.tasks.save(&task, SnapshotSource::LocalEdit).await?;
        Ok(())
    }

    /// Does the task's workspace STILL target `project_node_id`? Used to make
    /// the `AddItem` write-back idempotent to a detach / move (#54): an
    /// `AddItem` that was already `inflight` when the workspace detached must
    /// not re-anchor the task to the old board. Returns `false` when the
    /// workspace is now projectless OR points at a DIFFERENT project than the
    /// one the `AddItem` targeted. Resolves the workspace → its `project_id` →
    /// the project's node id and compares against `project_node_id`.
    async fn workspace_still_targets_project(
        &self,
        task_id: domain_core::TaskId,
        project_node_id: &str,
    ) -> Result<bool> {
        let task = self.tasks.get(task_id).await?;
        let workspace = self.workspaces.get(task.workspace_id).await?;
        let Some(project_id) = workspace.project_id.clone() else {
            return Ok(false);
        };
        let project = self.projects.get(project_id).await?;
        Ok(project.id.as_str() == project_node_id)
    }

    /// Enqueue the `SetProjectStatus` that follows an `AddItem` /
    /// `CreateDraftIssue` once we know the item id. Resolves the project from
    /// the task's workspace and maps the current lifecycle status to an option
    /// (Blocked-with-no-matching-option falls back to the Open option, per RFC
    /// §3). No-op when the workspace has no project or no option resolves.
    async fn enqueue_status_follow_up(
        &self,
        task_id: domain_core::TaskId,
        project_node_id: &str,
        item_id: &str,
    ) -> Result<()> {
        let task = self.tasks.get(task_id).await?;
        let workspace = self.workspaces.get(task.workspace_id).await?;
        let Some(project_id) = workspace.project_id.clone() else {
            return Ok(());
        };
        let project = self.projects.get(project_id).await?;
        let Some(option_id) = crate::board_option_for_lifecycle(&project, task.is_open())
        else {
            return Ok(());
        };
        let entry = OutboxEntry::new(
            task_id,
            OutboxMutation::SetProjectStatus {
                project_node_id: project_node_id.to_string(),
                item_node_id: item_id.to_string(),
                status_field_id: project.status_field_id.clone(),
                option_id: option_id.to_string(),
            },
        );
        self.outbox.enqueue(&entry).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain_core::{ProjectId, Timestamp, WorkspaceId};
    use domain_project::{Project, StatusMapping, StatusOption};
    use domain_sync::{OutboxEntry, OutboxMutation, OutboxStatus};
    use domain_task::{RemoteRef, SyncState, Task};
    use domain_workspace::{Workspace, WorkspaceName};
    use ports::{
        OutboxRepository, ProjectRepository, RemoteStateReason, TaskRepository, WorkspaceRepository,
    };
    use testing_fixtures::{
        InMemoryOutboxRepository, InMemoryProjectRepository, InMemoryRemoteProjectProvider,
        InMemoryRemoteTaskProvider, InMemoryTaskRepository, InMemoryWorkspaceRepository,
        ProjectCall,
    };

    struct Harness {
        outbox: Arc<InMemoryOutboxRepository>,
        tasks: Arc<InMemoryTaskRepository>,
        workspaces: Arc<InMemoryWorkspaceRepository>,
        projects: Arc<InMemoryProjectRepository>,
        remote_tasks: Arc<InMemoryRemoteTaskProvider>,
        remote_projects: Arc<InMemoryRemoteProjectProvider>,
        drainer: OutboxDrainer,
    }

    /// Build a drainer over all-in-memory deps. `backoff` makes the retry
    /// policy observable without real waits.
    async fn harness(backoff: BackoffSchedule) -> Harness {
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let remote_tasks = Arc::new(InMemoryRemoteTaskProvider::new());
        let remote_projects = Arc::new(InMemoryRemoteProjectProvider::new());

        let outbox_dyn: Arc<dyn OutboxRepository> = outbox.clone();
        let tasks_dyn: Arc<dyn TaskRepository> = tasks.clone();
        let ws_dyn: Arc<dyn WorkspaceRepository> = workspaces.clone();
        let proj_dyn: Arc<dyn ProjectRepository> = projects.clone();
        let rt_dyn: Arc<dyn ports::RemoteTaskProvider> = remote_tasks.clone();
        let rp_dyn: Arc<dyn RemoteProjectProvider> = remote_projects.clone();

        let drainer = OutboxDrainer::new(outbox_dyn, tasks_dyn, ws_dyn, proj_dyn, rt_dyn, rp_dyn)
            .with_backoff(backoff);

        Harness {
            outbox,
            tasks,
            workspaces,
            projects,
            remote_tasks,
            remote_projects,
            drainer,
        }
    }

    /// Tiny backoff so reschedule pushes `next_attempt_at` into the future
    /// (so a rescheduled entry isn't re-claimed in the same `drain_once`).
    fn test_backoff(max_attempts: u32) -> BackoffSchedule {
        BackoffSchedule::new(vec![std::time::Duration::from_secs(60)], max_attempts)
    }

    /// A promoted task with a populated `synced_baseline` AND one
    /// local title edit already applied. The Promote snapshot
    /// (title `task {remote_id}`) becomes the baseline; the title
    /// is then bumped to `{remote_id}-edited` and saved as
    /// LocalEdit so the in-memory task is dirty but the baseline is
    /// unchanged. Tests wanting a clean baseline can revert the
    /// title before enqueue.
    async fn seed_issue_task(h: &Harness, ws: WorkspaceId, remote_id: &str) -> Task {
        let mut t = Task::new_draft(ws, None, format!("task {remote_id}")).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", remote_id))
            .unwrap();
        h.tasks.save(&t, SnapshotSource::Promote).await.unwrap();
        // One-shot title edit so `diff_against_baseline()` is non-empty.
        t.set_title(format!("{remote_id}-edited")).unwrap();
        h.tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
        t
    }

    fn project_with_options() -> Project {
        Project::new(
            ProjectId::parse("PVT_kwHO_test").unwrap(),
            "acme".into(),
            3,
            "Board".into(),
            "PVTSSF_field".into(),
            vec![
                StatusOption {
                    option_id: "o_backlog".into(),
                    name: "Backlog".into(),
                    ordinal: 0,
                },
                StatusOption {
                    option_id: "o_wip".into(),
                    name: "In progress".into(),
                    ordinal: 1,
                },
                StatusOption {
                    option_id: "o_done".into(),
                    name: "Done".into(),
                    ordinal: 2,
                },
            ],
            vec![
                StatusMapping {
                    is_open: true,
                    option_id: "o_backlog".into(),
                },
                StatusMapping {
                    is_open: false,
                    option_id: "o_done".into(),
                },
            ],
            false,
            Timestamp::now(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn drain_success_marks_succeeded_and_calls_provider_with_state() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let mut task = seed_issue_task(&h, ws, "1").await;
        // Drive to Done so the drainer should send (closed=true,
        // Completed). Save as LocalEdit so the baseline stays at the
        // Promote snapshot; saving as Push would re-baseline and
        // leave nothing to send.
        task.start().unwrap();
        task.complete().unwrap();
        h.tasks
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: Some("t".into()),
                body: Some("b".into()),
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 1);

        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].closed, Some(true));
        assert_eq!(updates[0].state_reason, Some(RemoteStateReason::Completed));

        let all = h.outbox.all();
        assert_eq!(all[0].status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn drain_update_remote_rebaselines_on_success_rfc0003_d5() {
        // RFC 0003 D5 (rpl-xq6): a successful drain of an `UpdateRemote`
        // must re-baseline the task so a later reconcile does not see
        // the same diff and re-push it. The rebaseline advances
        // per-field — the patched field moves to the transmitted
        // value, the un-patched fields stay at the pre-drain baseline
        // entry (rpl-vvf nails the byte-identical assertion; this
        // happy-path test confirms the basic rebaseline + the
        // Staged/DirtyLocal → Synced transition the old code skipped).
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "1").await;

        // Capture the pre-drain baseline (the Promote snapshot).
        let pre = h.tasks.get(task.id).await.unwrap();
        let pre_baseline = pre.synced_baseline.clone().expect("baseline");
        assert_eq!(pre_baseline.title, "task 1");
        assert_eq!(pre.sync, SyncState::DirtyLocal);
        assert_eq!(pre.title, "1-edited", "seed set a one-shot title edit");

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 1);

        // The drainer sent a PATCH carrying the live diff (the helper
        // re-derives title from the live task — the payload's
        // captured fields are ignored by design).
        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].title.as_deref(), Some("1-edited"));
        assert_eq!(updates[0].remote_id, "1");

        // The task is Synced and the baseline advanced per-field.
        let post = h.tasks.get(task.id).await.unwrap();
        assert_eq!(post.sync, SyncState::Synced);
        let post_baseline = post.synced_baseline.clone().expect("baseline");
        assert_eq!(post_baseline.title, "1-edited", "title rebaselined");
        assert_eq!(
            post_baseline.body, pre_baseline.body,
            "body baseline entry must be unchanged (rpl-xq6 happy path; \
             rpl-vvf nails the byte-identical assertion across the wire)"
        );
        assert_eq!(post_baseline.lifecycle, pre_baseline.lifecycle);
        assert_eq!(post_baseline.assignees, pre_baseline.assignees);
        assert_eq!(
            post_baseline.source,
            SnapshotSource::Push,
            "confirm source stamped on the rebaselined snapshot"
        );
        // No diff left: the post-drain patch is empty.
        assert!(post.diff_against_baseline().is_empty());

        assert_eq!(h.outbox.all()[0].status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn drain_update_remote_empty_patch_confirms_without_pushing() {
        // The empty-patch path: a task reaches the drainer's UpdateRemote
        // arm with no live diff against the baseline (e.g. a Staged
        // task whose local edit was reverted before the drain fired).
        // The drainer must:
        //  (a) skip the remote PATCH (no field to send),
        //  (b) still call `confirm_synced_fields` so the Staged →
        //      Synced transition lands — the old code (no confirm at
        //      all) would have left the task stuck in Staged even
        //      though the entry succeeded,
        //  (c) re-baseline with no field changes (the merged baseline
        //      is byte-identical to the pre-drain baseline; only the
        //      source stamp and captured_at move).
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        // Build a promoted task in Staged state with an empty diff.
        // Staged survives `reconcile_dirty_against_baseline`, so we
        // can revert a one-shot title edit to its baseline value
        // while the state stays Staged.
        let mut t = Task::new_draft(ws, None, "task 7".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", "7")).unwrap();
        // Persist the Promote snapshot so the repo's snapshot history
        // can re-project `synced_baseline` on `get`; without this the
        // history only carries the initial LocalEdit save and the
        // baseline would be missing on the reload below.
        h.tasks.save(&t, SnapshotSource::Promote).await.unwrap();
        t.set_title("stale".into()).unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.stage_for_sync().unwrap();
        assert_eq!(t.sync, SyncState::Staged);
        t.set_title(t.synced_baseline.as_ref().unwrap().title.clone())
            .unwrap();
        assert_eq!(t.sync, SyncState::Staged, "Staged survives reconcile");
        assert!(t.diff_against_baseline().is_empty());
        h.tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let pre = h.tasks.get(t.id).await.unwrap();
        let pre_baseline = pre.synced_baseline.clone().expect("baseline");
        assert_eq!(pre.sync, SyncState::Staged);

        let entry = OutboxEntry::new(
            t.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "7".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 1, "the entry still succeeds on an empty patch");

        // (a) No PATCH was sent.
        assert!(
            h.remote_tasks.updates().is_empty(),
            "build_update_from_patch returns None for an empty patch"
        );

        // (b) The Staged → Synced transition landed.
        let post = h.tasks.get(t.id).await.unwrap();
        assert_eq!(post.sync, SyncState::Synced);

        // (c) The merged baseline is byte-identical except for the
        // stamped source + captured_at.
        let post_baseline = post.synced_baseline.clone().expect("baseline");
        assert_eq!(post_baseline.title, pre_baseline.title);
        assert_eq!(post_baseline.body, pre_baseline.body);
        assert_eq!(post_baseline.lifecycle, pre_baseline.lifecycle);
        assert_eq!(post_baseline.assignees, pre_baseline.assignees);
        assert_eq!(post_baseline.source, SnapshotSource::Push);

        assert_eq!(h.outbox.all()[0].status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn drain_add_sub_issue_calls_provider() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "10").await;
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::AddSubIssue {
                parent_canonical: "github.com/o/r".into(),
                parent_remote_id: "10".into(),
                child_canonical: "github.com/o/r".into(),
                child_remote_id: "20".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        assert_eq!(h.drainer.drain_once().await.unwrap(), 1);

        let calls = h.remote_tasks.relation_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].op, "add_sub_issue");
        assert_eq!(
            calls[0].addressed_remote_id, "10",
            "endpoint addresses parent"
        );
        assert_eq!(
            calls[0].related_remote_id, "20",
            "child is the related issue"
        );
        assert_eq!(h.outbox.all()[0].status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn drain_add_blocked_by_calls_provider() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "10").await;
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::AddBlockedBy {
                blocked_canonical: "github.com/o/r".into(),
                blocked_remote_id: "10".into(),
                blocker_canonical: "github.com/o/r".into(),
                blocker_remote_id: "20".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        assert_eq!(h.drainer.drain_once().await.unwrap(), 1);

        let calls = h.remote_tasks.relation_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].op, "add_blocked_by");
        assert_eq!(
            calls[0].addressed_remote_id, "10",
            "endpoint addresses blocked"
        );
        assert_eq!(
            calls[0].related_remote_id, "20",
            "blocker is the related issue"
        );
        assert_eq!(h.outbox.all()[0].status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn provider_err_under_cap_reschedules_with_backoff() {
        // 60s backoff so the scheduled window is large enough to assert against
        // a pre-drain timestamp deterministically.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "1").await;
        h.remote_tasks.fail_next(1);

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        // Capture a lower bound just before the drain. The reschedule must
        // anchor `next_attempt_at` to a FRESH failure-time timestamp (#54), so
        // it lands at `failure_time + 60s` — at least `before + 60s` since
        // `failure_time >= before`. Reusing the claim-time `now` would still
        // satisfy this, but anchoring to `enqueued_at`, a stale/past instant, or
        // a default timestamp would schedule it in the past and fail here.
        let before = Timestamp::now();
        h.drainer.drain_once().await.unwrap();

        let all = h.outbox.all();
        assert_eq!(all[0].status, OutboxStatus::Pending, "back to pending");
        assert_eq!(all[0].attempts, 1, "attempts bumped");
        assert!(all[0].last_error.is_some());
        let next = all[0].next_attempt_at.expect("backoff window set");
        let lower = Timestamp::from_utc(before.into_inner() + chrono::Duration::seconds(60));
        assert!(
            next >= lower,
            "next_attempt_at ({next:?}) must be at least the failure time + the 60s backoff \
             (>= {lower:?}); a window scheduled in the past would re-drain immediately"
        );
    }

    #[tokio::test]
    async fn provider_err_at_cap_dead_letters() {
        // max_attempts = 1 ⇒ the first failure already reaches the cap.
        let h = harness(test_backoff(1)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "1").await;
        h.remote_tasks.fail_next(1);

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        let dead = h.outbox.list_dead_lettered().await.unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].status, OutboxStatus::Failed);
    }

    #[tokio::test]
    async fn per_task_fifo_drains_in_enqueue_order() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "1").await;

        // Three UpdateRemote on the same task, enqueued in order. The drainer
        // re-derives title/body from the *live* task (so they're identical
        // across the three), but `remote_id` is carried verbatim from the
        // payload — use it as the per-entry ordering signal. No `sleep()` to
        // stagger enqueued_at: the in-memory claim tie-breaks on insertion
        // order (mirroring the SQLite `rowid` contract).
        //
        // RFC 0003 D5 (rpl-xq6) change: with the per-field rebaseline on
        // success, the FIRST drain PATCHes + rebaselines the title so the
        // task is Synced. The second and third drains then see a Synced
        // task with an empty diff and the new drainer code (gated on
        // confirmable state) skips the PATCH — but the entries still
        // succeed. So only one remote PATCH lands.
        for remote_id in ["r-started", "r-edited", "r-completed"] {
            let entry = OutboxEntry::new(
                task.id,
                OutboxMutation::UpdateRemote {
                    canonical_repo: "github.com/o/r".into(),
                    remote_id: remote_id.into(),
                    title: None,
                    body: None,
                    closed: None,
                },
            );
            h.outbox.enqueue(&entry).await.unwrap();
        }

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 3, "all three entries drain in one pass");

        // Only the first entry produced a remote PATCH — the rebaseline
        // collapsed the diff for the next two. The first PATCH carries
        // the first entry's `remote_id` (carried verbatim from the
        // payload).
        let remote_ids: Vec<String> = h
            .remote_tasks
            .updates()
            .into_iter()
            .map(|u| u.remote_id)
            .collect();
        assert_eq!(
            remote_ids,
            vec!["r-started".to_string()],
            "only the first entry PATCHes (post-rpl-xq6 rebaseline collapses the rest)"
        );

        // FIFO claim order is preserved: every entry lands in
        // OutboxStatus::Succeeded, and the order in which the in-memory
        // fixture stamps `updated_at` (via `mark_succeeded`) is the
        // claim order, which is the enqueue order. Sorting the
        // succeeded entries by `updated_at` and reading the
        // payload's `remote_id` field gives the FIFO claim sequence.
        let all = h.outbox.all();
        let succeeded: Vec<&OutboxEntry> = all
            .iter()
            .filter(|e| e.status == OutboxStatus::Succeeded)
            .collect();
        assert_eq!(
            succeeded.len(),
            3,
            "all three entries succeed; the FIFO order is the claim order"
        );
        let mut ordered: Vec<&OutboxEntry> = succeeded.clone();
        ordered.sort_by_key(|e| e.updated_at);
        let claim_order: Vec<String> = ordered
            .iter()
            .map(|e| match &e.mutation {
                OutboxMutation::UpdateRemote { remote_id, .. } => remote_id.clone(),
                other => panic!("expected UpdateRemote, got {other:?}"),
            })
            .collect();
        assert_eq!(
            claim_order,
            vec![
                "r-started".to_string(),
                "r-edited".to_string(),
                "r-completed".to_string()
            ],
            "FIFO: same-task entries are mark_succeeded in enqueue order, \
             even though only the first PATCHes (the rebaseline collapses \
             the trailing two into no-ops)"
        );
    }

    #[tokio::test]
    async fn failed_head_on_task_a_does_not_block_task_b() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task_a = seed_issue_task(&h, ws, "1").await;
        let task_b = seed_issue_task(&h, ws, "2").await;

        // A's head fails (reschedules); B must still drain this pass.
        h.remote_tasks.fail_next(1);
        let a = OutboxEntry::new(
            task_a.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: Some("a".into()),
                closed: None,
            },
        );
        let b = OutboxEntry::new(
            task_b.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "2".into(),
                title: None,
                body: Some("b".into()),
                closed: None,
            },
        );
        h.outbox.enqueue(&a).await.unwrap();
        h.outbox.enqueue(&b).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 1, "B drained even though A's head failed");

        // The one successful update is B's — identify it by remote_id (carried
        // verbatim from the payload; body/title are re-derived from the live
        // task, so they're not a reliable per-entry signal).
        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].remote_id, "2");

        // A is rescheduled (pending, future next_attempt_at); B succeeded.
        let all = h.outbox.all();
        let a_row = all.iter().find(|e| e.id == a.id).unwrap();
        let b_row = all.iter().find(|e| e.id == b.id).unwrap();
        assert_eq!(a_row.status, OutboxStatus::Pending);
        assert_eq!(b_row.status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn add_item_writes_project_item_id_and_enqueues_set_status() {
        let h = harness(test_backoff(5)).await;

        // Workspace attached to a project; an issue-backed task with a node id.
        let project = project_with_options();
        h.projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        h.workspaces.save(&ws).await.unwrap();

        let mut task = Task::new_draft(ws.id, None, "attach me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(RemoteRef {
            provider: "github".into(),
            remote_id: "9".into(),
            node_id: Some("I_9".into()),
        })
        .unwrap();
        h.tasks.save(&task, SnapshotSource::Promote).await.unwrap();

        h.remote_projects.set_add_item_returns("PVTI_new");

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::AddItem {
                project_node_id: project.id.as_str().to_string(),
                issue_node_id: "I_9".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        // project_item_id written back onto the task.
        let reloaded = h.tasks.get(task.id).await.unwrap();
        assert_eq!(reloaded.project_item_id.as_deref(), Some("PVTI_new"));

        // A SetProjectStatus follow-up was enqueued and then drained (Open →
        // Backlog), so the provider saw both add_item and set_status.
        let calls = h.remote_projects.calls();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, ProjectCall::AddItem { .. }))
        );
        assert!(calls.iter().any(|c| matches!(
            c,
            ProjectCall::SetStatus { option_id, .. } if option_id == "o_backlog"
        )));
    }

    #[tokio::test]
    async fn add_item_after_detach_does_not_set_project_item_id() {
        // Detach-idempotent write-back (#54): an AddItem that was already
        // inflight finishes AFTER the workspace detached from the project. The
        // drainer must NOT persist project_item_id (that would re-anchor the
        // task to the board it just left). The entry still succeeds; the remote
        // board item is left as-is. Model the post-detach state directly: the
        // workspace's project_id is None at drain time.
        let h = harness(test_backoff(5)).await;

        // Project exists, but the workspace is NO LONGER attached to it.
        let project = project_with_options();
        h.projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = None; // detached
        h.workspaces.save(&ws).await.unwrap();

        let mut task = Task::new_draft(ws.id, None, "attach me".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(RemoteRef {
            provider: "github".into(),
            remote_id: "9".into(),
            node_id: Some("I_9".into()),
        })
        .unwrap();
        h.tasks.save(&task, SnapshotSource::Promote).await.unwrap();

        h.remote_projects.set_add_item_returns("PVTI_new");

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::AddItem {
                project_node_id: project.id.as_str().to_string(),
                issue_node_id: "I_9".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 1, "the entry still succeeds");

        // project_item_id was NOT written back (the detach stands).
        let reloaded = h.tasks.get(task.id).await.unwrap();
        assert_eq!(
            reloaded.project_item_id, None,
            "a post-detach AddItem must not re-anchor the task to the old board"
        );

        // The remote add_item DID fire (we left the board item as-is), but NO
        // SetProjectStatus follow-up was enqueued.
        let calls = h.remote_projects.calls();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, ProjectCall::AddItem { .. })),
            "the remote add_item still happened; board cleanup is separate"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, ProjectCall::SetStatus { .. })),
            "no SetProjectStatus follow-up after a detached AddItem"
        );
        assert_eq!(h.outbox.all()[0].status, OutboxStatus::Succeeded);
    }

    #[tokio::test]
    async fn convert_draft_to_issue_writes_issue_node_id_and_number() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        // A draft-backed mirror that just gained a repo (project_item_id set,
        // no remote yet). ConvertDraftToIssue mints the real issue; the
        // write-back persists a FULLY-populated RemoteRef from the returned
        // node id + REST number (#54) — never a half-populated ref with an
        // empty remote_id.
        let mut task = Task::import_mirror(
            ws,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.project_item_id = Some("PVTI_draft".into());
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        h.remote_projects
            .set_convert_returns_with_number("I_converted_42", 42);

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::ConvertDraftToIssue {
                item_node_id: "PVTI_draft".into(),
                repo_node_id: "github.com/o/r".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        let reloaded = h.tasks.get(task.id).await.unwrap();
        let remote = reloaded.remote.as_ref().expect("remote populated");
        assert_eq!(remote.node_id.as_deref(), Some("I_converted_42"));
        assert_eq!(
            remote.remote_id, "42",
            "remote_id is the REST number, never empty"
        );
    }

    #[tokio::test]
    async fn convert_write_back_never_yields_empty_remote_id_for_update_remote() {
        // Regression (#54): the invariant that must hold — after a draft→issue
        // conversion, a subsequent lifecycle/content edit must NEVER plan an
        // `UpdateRemote` with an empty `remote_id`. Before the capture-number
        // fix the write-back persisted an issue-backed RemoteRef with
        // remote_id = "" (only the node id was known), and `plan_mutations`
        // (which keys on `task.remote.is_some()`) would emit an UpdateRemote
        // that the drainer then tried to push to an unaddressable issue.
        use crate::enqueue::plan_mutations;

        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        let mut task = Task::import_mirror(
            ws,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.project_item_id = Some("PVTI_draft".into());
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        h.remote_projects
            .set_convert_returns_with_number("I_converted_7", 7);

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::ConvertDraftToIssue {
                item_node_id: "PVTI_draft".into(),
                repo_node_id: "github.com/o/r".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();
        h.drainer.drain_once().await.unwrap();

        // Plan the mutations a later content edit would owe. The converted
        // task is now issue-backed, so it owes an UpdateRemote — but with a
        // NON-empty remote_id (the captured number), never "".
        let reloaded = h.tasks.get(task.id).await.unwrap();
        let planned = plan_mutations(&reloaded, None, Some("github.com/o/r"), true);
        let update = planned
            .iter()
            .find_map(|m| match m {
                OutboxMutation::UpdateRemote { remote_id, .. } => Some(remote_id),
                _ => None,
            })
            .expect("issue-backed task owes an UpdateRemote");
        assert_eq!(update, "7", "UpdateRemote carries the captured number");
        assert!(
            !update.is_empty(),
            "no UpdateRemote may ever carry an empty remote_id"
        );
    }

    #[tokio::test]
    async fn backed_off_head_blocks_its_tail_same_task() {
        // Per-task FIFO under recoverable failure: one task, two UpdateRemote
        // entries (head, tail). The head fails recoverably and reschedules with
        // a FUTURE next_attempt_at (back to pending). The tail is eligible now,
        // but it must NOT overtake the backed-off head — draining this pass
        // must apply NOTHING for this task (the head isn't eligible yet, the
        // tail is blocked behind it). This guards the FIFO contract's
        // load-bearing failure mode.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let task = seed_issue_task(&h, ws, "1").await;

        // Head fails once → reschedules to a future window.
        h.remote_tasks.fail_next(1);

        let head = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: Some("head".into()),
                closed: None,
            },
        );
        let tail = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: Some("tail".into()),
                closed: None,
            },
        );
        h.outbox.enqueue(&head).await.unwrap();
        h.outbox.enqueue(&tail).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        // Only the head was attempted (and it failed → rescheduled). The tail
        // must NOT have been applied this pass.
        assert_eq!(n, 0, "nothing succeeded: head failed, tail must wait");
        // The provider saw no successful write at all this pass.
        assert!(
            h.remote_tasks.updates().is_empty(),
            "tail must not overtake a backed-off head"
        );

        let all = h.outbox.all();
        let head_row = all.iter().find(|e| e.id == head.id).unwrap();
        let tail_row = all.iter().find(|e| e.id == tail.id).unwrap();
        assert_eq!(head_row.status, OutboxStatus::Pending, "head rescheduled");
        assert_eq!(head_row.attempts, 1, "head attempt bumped");
        assert!(
            head_row.next_attempt_at.is_some(),
            "head backoff window set"
        );
        assert_eq!(
            tail_row.status,
            OutboxStatus::Pending,
            "tail still pending, never claimed"
        );
        assert_eq!(tail_row.attempts, 0, "tail never attempted");
    }

    #[tokio::test]
    async fn update_remote_pushes_live_task_title_body_not_payload_snapshot() {
        // The drainer re-derives title/body from the *live* task (mirroring
        // SyncService::push), NOT the snapshot captured on the enqueued entry.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let mut task = seed_issue_task(&h, ws, "1").await;
        task.set_title("LIVE TITLE".into()).unwrap();
        task.set_body("live body".into());
        h.tasks
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        // The entry carries STALE content captured at enqueue time.
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: Some("stale title".into()),
                body: Some("stale body".into()),
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].title.as_deref(), Some("LIVE TITLE"));
        assert_eq!(updates[0].body.as_deref(), Some("live body"));
    }

    #[tokio::test]
    async fn drain_update_remote_sends_only_changed_fields() {
        // Title-only edit must PATCH only `title`; the other
        // fields stay None so the adapter omits them.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        // Promote then edit only the title.
        let mut task = Task::new_draft(ws, None, "original".into()).unwrap();
        task.stage_for_sync().unwrap();
        task.promote_to_remote(RemoteRef::new("github", "1"))
            .unwrap();
        h.tasks.save(&task, SnapshotSource::Promote).await.unwrap();
        let mut t = h.tasks.get(task.id).await.unwrap();
        t.set_title("renamed".into()).unwrap();
        h.tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();
        h.drainer.drain_once().await.unwrap();

        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        assert_eq!(u.title.as_deref(), Some("renamed"));
        assert_eq!(u.body, None);
        assert_eq!(u.closed, None);
        assert_eq!(u.state_reason, None);
        assert_eq!(u.assignees, None);
    }

    /// Parallel to `update_remote_pushes_live_task_title_body_not_payload_snapshot`:
    /// the drainer must re-derive assignees from the live task, not from any
    /// payload snapshot. `OutboxMutation::UpdateRemote` carries NO `assignees`
    /// field (the drainer is the sole source — the rpl-x2v invariant), so the
    /// LIVE task's assignees must reach the wire even if the entry was enqueued
    /// earlier with no assignees. RFC 0003 §7 case 2 (rpl-oa6).
    #[tokio::test]
    async fn drainer_rerederives_assignees_from_live_task() {
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let mut task = seed_issue_task(&h, ws, "1").await;
        // Set assignees on the LIVE task post-enqueue-style: the entry below
        // carries no assignees at all, so the only source is the live task.
        task.set_assignees(vec!["carol".into()]);
        h.tasks
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].assignees.as_deref(),
            Some(&["carol".to_string()][..]),
            "drainer re-derives assignees from the live task (rpl-x2v invariant)"
        );
        // The other fields stay None because seed_issue_task's title edit
        // is the only diff in MirrorPatch; assignees join it.
        assert_eq!(updates[0].title.as_deref(), Some("1-edited"));
        assert_eq!(updates[0].body, None);
        assert_eq!(updates[0].closed, None);
        assert_eq!(updates[0].state_reason, None);
    }

    #[tokio::test]
    async fn drain_update_remote_skips_provider_when_patch_empty() {
        // Empty `MirrorPatch` ⇒ helper returns None ⇒ drainer does NOT
        // call `update_remote` at all. The entry still succeeds (no
        // remote call to retry). This is the new "no wasted PATCH"
        // behavior — a coalesced empty head, a comment-only push, etc.
        // all collapse to a no-op.
        //
        // RFC 0003 D5 (rpl-xq6): the drainer also gates the entire
        // PATCH+confirm+save block on `confirmable` state. After
        // reverting the title to its baseline value, the task is
        // `Synced` (not confirmable), so the block is skipped entirely
        // — no PATCH, no confirm, no rebaseline, no save. The task
        // must stay `Synced` with a byte-identical baseline; the
        // entry still succeeds.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        // `seed_issue_task` writes a Promote snapshot (baseline title =
        // "task 1") then a LocalEdit bumping the title to "1-edited".
        // Revert the title back to "task 1" with another LocalEdit so
        // the live title matches the baseline — the diff is now empty
        // and `reconcile_dirty_against_baseline` flips the task back
        // to `Synced`.
        let task = seed_issue_task(&h, ws, "1").await;
        let mut t = h.tasks.get(task.id).await.unwrap();
        t.set_title("task 1".into()).unwrap();
        h.tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        // Capture the pre-drain baseline + state for the no-op assertion.
        let pre = h.tasks.get(task.id).await.unwrap();
        assert_eq!(pre.sync, SyncState::Synced, "Synced post-revert");
        let pre_baseline = pre.synced_baseline.clone().expect("baseline");

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();
        h.drainer.drain_once().await.unwrap();

        assert!(
            h.remote_tasks.updates().is_empty(),
            "empty patch ⇒ no remote update call"
        );
        // The entry still succeeded.
        let all = h.outbox.all();
        assert_eq!(all[0].status, OutboxStatus::Succeeded);

        // The new no-op-path contract: a Synced task drained for
        // UpdateRemote is a true no-op — no PATCH, no rebaseline, no
        // save. State stays Synced and the baseline entry is
        // byte-identical to pre-drain.
        let post = h.tasks.get(task.id).await.unwrap();
        assert_eq!(
            post.sync,
            SyncState::Synced,
            "Synced stays Synced on a no-op drain"
        );
        assert_eq!(
            post.synced_baseline,
            Some(pre_baseline),
            "baseline must be unchanged on a no-op drain (confirmable gate skipped the save)"
        );
    }

    #[tokio::test]
    async fn update_draft_issue_pushes_live_task_title_body_not_payload_snapshot() {
        // Regression (#54): like UpdateRemote, UpdateDraftIssue must re-derive
        // title/body from the *live* task, not the payload snapshotted at
        // enqueue time. A DirtyLocal draft-backed mirror coalesces later edits
        // without a new outbox row, so the captured payload can be stale —
        // pushing it would drop the newest edit.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        // A draft-backed mirror with a project_item_id (it lives on a board).
        let mut task = Task::import_mirror(
            ws,
            None,
            RemoteRef::new("github", "0"),
            "stale title".into(),
            "stale body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.project_item_id = Some("PVTI_draft".into());
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        // A later edit coalesces onto the live task with no new outbox row.
        task.set_title("LIVE DRAFT TITLE".into()).unwrap();
        task.set_body("live draft body".into());
        h.tasks
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        // The enqueued entry carries STALE content captured at enqueue time.
        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateDraftIssue {
                item_node_id: "PVTI_draft".into(),
                title: Some("stale title".into()),
                body: Some("stale body".into()),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        let calls = h.remote_projects.calls();
        let update = calls
            .iter()
            .find_map(|c| match c {
                ProjectCall::UpdateDraftIssue { title, body, .. } => Some((title, body)),
                _ => None,
            })
            .expect("an UpdateDraftIssue call was applied");
        assert_eq!(update.0.as_deref(), Some("LIVE DRAFT TITLE"));
        assert_eq!(update.1.as_deref(), Some("live draft body"));
    }

    #[tokio::test]
    async fn reopened_update_remote_drains_with_closed_false() {
        // Drainer-level proof that a Reopened task's UpdateRemote re-opens the
        // issue rather than closing it: lifecycle_to_remote_state(Reopened) =
        // (false, Reopened). (Pre-RFC-0004 this exercised the Blocked status;
        // "blocked" is now a relation, so the open-but-non-fresh lifecycle that
        // drains to (false, Reopened) is Reopened.)
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();
        let mut task = seed_issue_task(&h, ws, "1").await;
        task.reopen().unwrap();
        h.tasks
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: "github.com/o/r".into(),
                remote_id: "1".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0].closed,
            Some(false),
            "Reopened must drain with closed=false, never closing the issue"
        );
        assert_eq!(
            updates[0].state_reason,
            Some(RemoteStateReason::Reopened),
            "Reopened must drain with state_reason=Reopened (lifecycle_to_remote_state)"
        );
    }

    #[tokio::test]
    async fn create_draft_issue_writes_item_id_and_enqueues_set_status() {
        // Structurally identical to add_item_writes_..., but exercises the
        // CreateDraftIssue apply arm + its shared write_back_project_item and
        // SetProjectStatus follow-up, which had no direct test.
        let h = harness(test_backoff(5)).await;

        let project = project_with_options();
        h.projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        h.workspaces.save(&ws).await.unwrap();

        // A draft-backed mirror: no remote, project_item_id not yet known
        // (CreateDraftIssue mints it).
        let mut task = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "new draft".into(),
            "draft body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.remote = None;
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        h.remote_projects.set_create_draft_returns("PVTI_draft_new");

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::CreateDraftIssue {
                project_node_id: project.id.as_str().to_string(),
                title: "new draft".into(),
                body: "draft body".into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        // project_item_id written back from the create.
        let reloaded = h.tasks.get(task.id).await.unwrap();
        assert_eq!(reloaded.project_item_id.as_deref(), Some("PVTI_draft_new"));

        // CreateDraftIssue + the follow-up SetProjectStatus both hit the
        // provider (Open → Backlog).
        let calls = h.remote_projects.calls();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, ProjectCall::CreateDraftIssue { .. }))
        );
        assert!(calls.iter().any(|c| matches!(
            c,
            ProjectCall::SetStatus { option_id, .. } if option_id == "o_backlog"
        )));
    }

    #[tokio::test]
    async fn create_draft_replay_does_not_mint_a_second_draft() {
        // At-least-once delivery: a crash after the remote create but before
        // mark_succeeded replays the entry. CreateDraftIssue is NOT idempotent
        // remotely (a blind replay would mint a second draft), so the drainer
        // guards on the task's project_item_id. Simulate the replay by draining
        // the same entry twice (the task carries project_item_id after pass 1)
        // and assert the provider saw exactly one create_draft_issue call.
        let h = harness(test_backoff(5)).await;

        let project = project_with_options();
        h.projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        h.workspaces.save(&ws).await.unwrap();

        let mut task = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "new draft".into(),
            "draft body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.remote = None;
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        h.remote_projects.set_create_draft_returns("PVTI_draft_new");

        let mk = || {
            OutboxEntry::new(
                task.id,
                OutboxMutation::CreateDraftIssue {
                    project_node_id: project.id.as_str().to_string(),
                    title: "new draft".into(),
                    body: "draft body".into(),
                },
            )
        };

        // Pass 1: the create lands and project_item_id is written back.
        h.outbox.enqueue(&mk()).await.unwrap();
        h.drainer.drain_once().await.unwrap();
        // Pass 2: replay the same mutation — the guard must skip the create.
        h.outbox.enqueue(&mk()).await.unwrap();
        h.drainer.drain_once().await.unwrap();

        let create_calls = h
            .remote_projects
            .calls()
            .into_iter()
            .filter(|c| matches!(c, ProjectCall::CreateDraftIssue { .. }))
            .count();
        assert_eq!(
            create_calls, 1,
            "replay must not mint a second draft (guarded by project_item_id)"
        );
    }

    #[tokio::test]
    async fn convert_draft_replay_does_not_reconvert() {
        // ConvertDraftToIssue is also not safe to replay: re-running the
        // convert on an already-converted item misbehaves. The drainer guards
        // on the task's remote.node_id. Drain the same convert entry twice and
        // assert the provider saw exactly one convert_draft_to_issue call.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        let mut task = Task::import_mirror(
            ws,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.project_item_id = Some("PVTI_draft".into());
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        h.remote_projects.set_convert_returns("I_converted_42");

        let mk = || {
            OutboxEntry::new(
                task.id,
                OutboxMutation::ConvertDraftToIssue {
                    item_node_id: "PVTI_draft".into(),
                    repo_node_id: "github.com/o/r".into(),
                },
            )
        };

        // Pass 1: convert lands, issue node id written back.
        h.outbox.enqueue(&mk()).await.unwrap();
        h.drainer.drain_once().await.unwrap();
        // Pass 2: replay — the guard must skip the convert.
        h.outbox.enqueue(&mk()).await.unwrap();
        h.drainer.drain_once().await.unwrap();

        let convert_calls = h
            .remote_projects
            .calls()
            .into_iter()
            .filter(|c| matches!(c, ProjectCall::ConvertDraftToIssue { .. }))
            .count();
        assert_eq!(
            convert_calls, 1,
            "replay must not re-convert (guarded by remote.node_id)"
        );
    }

    // --- RFC 0002 D6: cross-filed task async apply targets the filing repo ------
    //
    // These two tests are the load-bearing guard for #125: when a task's
    // filing_repo_id diverges from its logical repo_id (i.e. the issue was
    // filed in a different repo than the task's owner repo), the async drainer
    // must target the FILING repo — never the logical repo — for both
    // UpdateRemote and ConvertDraftToIssue mutations.
    //
    // The mechanism: both mutations carry the already-resolved filing canonical
    // (or filing repo node id) on the outbox entry itself, set at enqueue time
    // by the filing-aware planner. The drainer passes the entry's carried
    // literal straight through to the provider; it NEVER re-derives a canonical
    // from the task's logical repo_id during apply. These tests confirm that
    // contract holds.

    #[tokio::test]
    async fn update_remote_cross_filed_targets_filing_repo_not_logical_repo() {
        // A task whose issue was filed in "filing.repo" (filing canonical),
        // NOT in its logical "logical.repo". The outbox entry's canonical_repo
        // carries the filing canonical (resolved at enqueue time by the
        // filing-aware planner). The drainer must pass that literal through —
        // it must NOT re-derive the canonical from the task's logical repo.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        // Seed an issue-backed task. The task's repo_id would resolve to
        // "logical.repo" if the drainer ever re-derived the canonical from it;
        // we leave it unset (orphan) precisely so such re-derivation would
        // produce nothing or a wrong answer — making the test sensitive to
        // any attempt to re-resolve from the task rather than the entry.
        let mut task = seed_issue_task(&h, ws, "42").await;
        task.start().unwrap();
        h.tasks
            .save(&task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        // The filing-aware planner would have encoded the FILING canonical here.
        let filing_canonical = "github.com/org/filing-repo";

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::UpdateRemote {
                canonical_repo: filing_canonical.into(),
                remote_id: "42".into(),
                title: None,
                body: None,
                closed: None,
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        let n = h.drainer.drain_once().await.unwrap();
        assert_eq!(n, 1);

        let updates = h.remote_tasks.updates();
        assert_eq!(updates.len(), 1);
        // The task is an orphan (repo_id unset), so any attempt to re-derive a
        // canonical from the task — instead of using the entry's carried literal
        // — would yield None/empty, never `filing_canonical`. This assert_eq is
        // therefore load-bearing against a fall-back-to-logical regression.
        assert_eq!(
            updates[0].canonical_repo, filing_canonical,
            "UpdateRemote must target the FILING repo carried on the entry, \
             not a canonical re-derived from the task's logical repo"
        );
    }

    #[tokio::test]
    async fn convert_draft_to_issue_cross_filed_uses_entry_repo_node_id() {
        // A draft→issue conversion where the filing repo is different from
        // what the task's logical repo would resolve to. The outbox entry's
        // repo_node_id carries the filing repo's node id (resolved at enqueue
        // time by the filing-aware draft-conversion planner). The drainer must
        // pass that literal through to convert_draft_to_issue — never
        // re-deriving from the logical repo.
        let h = harness(test_backoff(5)).await;
        let ws = WorkspaceId::new();

        let mut task = Task::import_mirror(
            ws,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        task.project_item_id = Some("PVTI_cross_filed_draft".into());
        h.tasks.save(&task, SnapshotSource::Pull).await.unwrap();

        h.remote_projects
            .set_convert_returns_with_number("I_converted_cross", 99);

        // The filing-aware planner encodes the FILING repo's node id here.
        let filing_repo_node_id = "R_kgDOfiling";

        let entry = OutboxEntry::new(
            task.id,
            OutboxMutation::ConvertDraftToIssue {
                item_node_id: "PVTI_cross_filed_draft".into(),
                repo_node_id: filing_repo_node_id.into(),
            },
        );
        h.outbox.enqueue(&entry).await.unwrap();

        h.drainer.drain_once().await.unwrap();

        // The provider must have received the FILING repo node id, not the
        // logical one.
        let calls = h.remote_projects.calls();
        let convert_call = calls
            .iter()
            .find_map(|c| match c {
                ProjectCall::ConvertDraftToIssue {
                    item_node_id,
                    repo_node_id,
                } => Some((item_node_id.as_str(), repo_node_id.as_str())),
                _ => None,
            })
            .expect("ConvertDraftToIssue call must have been applied");

        // The task has no repo binding (import_mirror with repo_id = None), so
        // any re-derivation of a repo node id from the task — instead of using
        // the entry's carried literal — would yield None/empty, never
        // `filing_repo_node_id`. This assert_eq is therefore load-bearing
        // against a fall-back-to-logical regression.
        assert_eq!(
            convert_call.1, filing_repo_node_id,
            "ConvertDraftToIssue must target the FILING repo node id carried on \
             the entry, not one re-derived from the task's logical repo"
        );

        // Write-back: the task should now carry the returned issue node id and REST number.
        let reloaded = h.tasks.get(task.id).await.unwrap();
        let remote = reloaded.remote.as_ref().expect("remote populated");
        assert_eq!(remote.node_id.as_deref(), Some("I_converted_cross"));
        assert_eq!(remote.remote_id, "99");
    }
}
