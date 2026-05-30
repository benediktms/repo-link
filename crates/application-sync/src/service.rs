//! [`SyncService`] — orchestrates remote promotion / push / pull / link.

use std::sync::Arc;

use domain_core::TaskId;
use domain_sync::{SyncDecision, SyncPolicy, decide};
use domain_task::{RemoteRef, SnapshotSource, SyncState, Task};
use dto_shared::SyncSummaryDto;
use ports::{
    PortError, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskUpdate, RepoBindingRepository,
    TaskRepository,
};

use crate::error::{Result, SyncError};
use crate::summary::{
    ensure_not_archived, link_summary, provider_label, remote_mirrors_baseline, summary,
};

pub struct SyncService {
    tasks: Arc<dyn TaskRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
    provider: Arc<dyn RemoteTaskProvider>,
    policy: SyncPolicy,
}

impl SyncService {
    pub fn new(
        tasks: Arc<dyn TaskRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
        provider: Arc<dyn RemoteTaskProvider>,
    ) -> Self {
        Self {
            tasks,
            bindings,
            provider,
            policy: SyncPolicy::ManualMerge,
        }
    }

    pub fn with_policy(mut self, policy: SyncPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Stage (if needed) and promote a `LocalOnly`/`Staged` task to a remote
    /// issue. The issue is created in the task's logical repo — which is also
    /// its filing repo today, until RFC 0002 lets the filing repo differ.
    /// `previous_state` / `new_state` in the summary describe the **sync**
    /// state — lifecycle stays untouched.
    pub async fn promote(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        ensure_not_archived(&task)?;
        let canonical = self.canonical_for(&task).await?;
        let prev = task.sync;

        if task.sync == SyncState::LocalOnly {
            task.stage_for_sync()?;
        }
        if task.sync != SyncState::Staged {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot promote from sync={:?}", task.sync),
            )));
        }

        let snap = self
            .provider
            .create_remote(RemoteTaskCreate {
                canonical_repo: &canonical,
                title: &task.title,
                body: &task.body,
                assignees: &task.assignees,
                labels: &[],
            })
            .await?;

        let mut remote_ref = RemoteRef::new(provider_label(&canonical), snap.remote_id.clone());
        // Capture the GraphQL node id the REST create response carried, so the
        // freshly promoted task is immediately board-eligible (the §D1 AddItem
        // path needs it). Dropping it here was the create/promote half of the
        // bug — a promoted task landed with node_id null and never reached the
        // board (RFC 0001 §9 / §D1).
        remote_ref.node_id = snap.node_id.clone();
        task.promote_to_remote(remote_ref)?;
        self.tasks.save(&task, SnapshotSource::Promote).await?;
        Ok(summary(&task, prev, SyncDecision::PushLocal))
    }

    // TODO(online-sync-mode): the current model is "edit locally, daemon
    // pushes on its next tick" — non-blocking and offline-friendly. A
    // future opt-in mode would have CLI mutations fire the remote update
    // inline when sync is Synced + the network is reachable, so changes
    // round-trip in real time. Trade-off: every CLI command would block
    // on a GitHub round-trip (~200-800ms typical) and rate limits become
    // a concern with batch commands. Default stays offline-first; this
    // would land as `--online` flag or `RepoLinkConfig::online_mode: bool`.
    /// Push local edits (`sync = DirtyLocal` or `Staged`) to the remote.
    pub async fn push(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        let canonical = self.canonical_for(&task).await?;
        let prev = task.sync;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();

        // Two independent push axes: the title/body/status snapshot (gated on
        // DirtyLocal|Staged) and pending outbound comments (a separate axis —
        // they never dirty the task, so a Synced task may still owe comments).
        let snapshot_dirty = matches!(task.sync, SyncState::DirtyLocal | SyncState::Staged);
        let has_pending_comments = task.comments.iter().any(|c| c.remote_id.is_none());
        if !snapshot_dirty && !has_pending_comments {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!(
                    "cannot push from sync={:?} with no pending comments",
                    task.sync
                ),
            )));
        }

        if snapshot_dirty {
            // Mirror the lifecycle status onto the remote issue's open/closed
            // bit + state_reason. Shared with the outbox drainer so both
            // outbound paths derive the remote state identically (Stage 6).
            let (closed, state_reason) = crate::lifecycle_to_remote_state(task.status);
            self.provider
                .update_remote(RemoteTaskUpdate {
                    canonical_repo: &canonical,
                    remote_id: &remote.remote_id,
                    title: Some(&task.title),
                    body: Some(&task.body),
                    closed: Some(closed),
                    state_reason,
                })
                .await?;

            task.confirm_synced(SnapshotSource::Push)?;
            self.tasks.save(&task, SnapshotSource::Push).await?;
        }

        // Drain pending comments after the snapshot push (independent of it):
        // POST each, then promote them all to synced in one repo write.
        //
        // Not idempotent across a mid-batch failure: if a later POST fails, the
        // earlier comments are already on GitHub but their local rows stay
        // pending, so a re-run re-POSTs them (duplicate remote comments). GitHub
        // issue comments have no idempotency key, so this at-most-once-per-retry
        // duplication is an accepted tradeoff for a low-frequency operation —
        // never lost comments, never a corrupted sync state.
        if has_pending_comments {
            let mut drained_local_ids = Vec::new();
            let mut pushed = Vec::new();
            for comment in task.comments.iter().filter(|c| c.remote_id.is_none()) {
                // Pending comments loaded from storage carry a surrogate id;
                // skip any in-memory entries that don't (not safely drainable).
                let Some(local_id) = comment.local_id.clone() else {
                    continue;
                };
                pushed.push(
                    self.provider
                        .create_comment(&canonical, &remote.remote_id, &comment.body)
                        .await?,
                );
                drained_local_ids.push(local_id);
            }
            self.tasks
                .mark_comments_pushed(id, &drained_local_ids, &pushed)
                .await?;
        }

        let decision = if snapshot_dirty {
            SyncDecision::PushLocal
        } else {
            SyncDecision::Noop
        };
        Ok(summary(&task, prev, decision))
    }

    /// Pull the latest remote snapshot and reconcile.
    pub async fn pull(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        ensure_not_archived(&task)?;
        let canonical = self.canonical_for(&task).await?;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();
        let prev = task.sync;

        let snap = self
            .provider
            .fetch_remote(&canonical, &remote.remote_id)
            .await?;
        // Backfill the GraphQL node id for a pre-project-sync task whose
        // remote was recorded before node ids were persisted. Without it the
        // task can never be added to a board (addProjectV2ItemById needs it),
        // so eager backfill skips it silently. This was the fetch/pull half of
        // the bug (RFC 0001 §9 / §D1). `node_id` is invisible to dirty
        // detection, so capturing it here can't perturb the drift decision.
        let node_id_backfill: Option<String> = match task.remote.as_ref() {
            Some(r) if r.node_id.is_none() => snap.node_id.clone(),
            _ => None,
        };
        if let (Some(nid), Some(r)) = (node_id_backfill.as_ref(), task.remote.as_mut()) {
            // Keep the in-memory aggregate consistent so any whole-row save the
            // decision below performs (PrePull / Pull) persists the node id
            // too; the targeted update after the match covers the Noop branch.
            r.node_id = Some(nid.clone());
        }
        // Drift is decided on the *mirrored* content (title / body / assignees),
        // not on `updated_at`. GitHub bumps `updated_at` on any activity —
        // comments, reactions, label edits, sub-issue changes — none of which
        // we mirror, so the old timestamp gate forced cosmetic pull_remote
        // refreshes on every comment. Compare against the last aligned
        // baseline so genuine remote field changes still pull, and unrelated
        // remote activity stays a noop.
        //
        // A task with a remote but no synced_baseline is anomalous (some
        // history was rolled back). Pull-and-restore is the safer fallback.
        let remote_changed = task
            .synced_baseline
            .as_ref()
            .map(|b| !remote_mirrors_baseline(&snap, b))
            .unwrap_or(true);
        let decision = decide(task.sync, remote_changed, self.policy);

        // A conflict still records the conflicted state below, but we defer the
        // error so comment mirroring (orthogonal to title/body drift) still runs.
        let mut manual_merge: Option<String> = None;
        match decision {
            SyncDecision::Noop => {}
            SyncDecision::PullRemote => {
                // Capture local state *before* remote overwrites it — this is the
                // undo target if the user wants to revert the pull.
                self.tasks.save(&task, SnapshotSource::PrePull).await?;
                // Transition to DirtyRemote so confirm_synced accepts the Pull
                // source (it requires Staged | DirtyLocal | DirtyRemote).
                task.mark_dirty_remote()?;
                // Direct field assignment (bypassing setter helpers that would
                // re-trigger dirty detection against the OLD baseline). Status is
                // intentionally NOT overwritten — GitHub's open/closed doesn't
                // map onto our 5-state lifecycle cleanly.
                task.title = snap.title.clone();
                task.body = snap.body.clone();
                task.assignees = snap.assignees.clone();
                task.confirm_synced(SnapshotSource::Pull)?;
                self.tasks.save(&task, SnapshotSource::Pull).await?;
            }
            SyncDecision::PushLocal => {
                // TODO(rwr/push-on-pull): a PushLocal decision returned from
                // pull means the local side is ahead. Today the user has to
                // call `sync push` explicitly to flush it; we could fold
                // that into pull when we want a one-shot reconcile.
            }
            SyncDecision::RequireManualMerge => {
                task.mark_conflicted()?;
                self.tasks.save(&task, SnapshotSource::LocalEdit).await?;
                manual_merge = Some(task_id.to_string());
            }
        }

        // Mirror comments regardless of the snapshot decision (even on Noop or a
        // manual-merge conflict): comment activity is orthogonal to title/body
        // drift, and `replace_comments` writes no snapshot, so this can't cause
        // the cosmetic-refresh churn the field-level pull guards against.
        let comments = self
            .provider
            .fetch_comments(&canonical, &remote.remote_id)
            .await?;
        self.tasks.replace_comments(id, &comments).await?;

        // Persist the node-id backfill with a targeted single-column write.
        // Redundant after a PullRemote whole-row save (it already wrote the
        // mutated ref) but idempotent, and it's the *only* persistence on the
        // Noop branch — which is the common backfill case, since a
        // pre-project-sync task with no field drift never triggers a save.
        if let Some(nid) = node_id_backfill {
            self.tasks.cache_remote_node_id(id, nid).await?;
        }

        if let Some(tid) = manual_merge {
            return Err(SyncError::ManualMerge(tid));
        }

        Ok(summary(&task, prev, decision))
    }

    /// Re-wire a task to a different remote. Always Conflict by default
    /// (linking is destructive on remote identity; snapshots are the audit
    /// trail). `relink = true` verifies the supplied URL is GitHub's redirect
    /// target for the *current* remote — if it is, the task stays in its
    /// existing sync state (typically `Synced`) because identity is preserved.
    pub async fn link(
        &self,
        task_id: &str,
        new_canonical: &str,
        new_remote_id: &str,
        relink: bool,
    ) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        let prev = task.sync;

        // Same-URL no-op: linking a task to the URL it's already pointing at
        // shouldn't churn the sync state or rewrite history.
        let already_pointing = task
            .remote
            .as_ref()
            .is_some_and(|r| r.provider == "github" && r.remote_id == new_remote_id);
        if already_pointing
            && self.canonical_for(&task).await.ok().as_deref() == Some(new_canonical)
        {
            return Ok(link_summary(&task, prev, "noop", None));
        }

        // Binding precondition: the target repo must already be attached to
        // this workspace. We don't auto-attach — prefix choice and dedupe are
        // intentionally explicit on this repo.
        let workspace_id = task.workspace_id;
        let binding = self
            .bindings
            .find_by_canonical_url(workspace_id, new_canonical)
            .await?
            .ok_or_else(|| {
                SyncError::Domain(domain_core::DomainError::validation(format!(
                    "repo {new_canonical} is not attached to this workspace; \
                     run `rl repo attach <url>` first"
                )))
            })?;

        let mut new_remote = RemoteRef::new("github", new_remote_id.to_string());

        if relink {
            // Verified relink overwrites title/body/assignees from the new
            // remote — only safe when the task is otherwise clean. Reject
            // DirtyLocal / Staged so we don't silently clobber edits the user
            // was about to push (the most common reason they hit the move
            // error in the first place).
            if task.sync != SyncState::Synced {
                return Err(SyncError::Domain(domain_core::DomainError::validation(
                    format!(
                        "--relink is only safe for synced tasks (current: {:?}); \
                         finish syncing first or use bare `task link`",
                        task.sync
                    ),
                )));
            }
            // Need a current remote to verify the redirect against.
            let current_remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();
            let current_canonical = self.canonical_for(&task).await?;
            let target = self
                .provider
                .discover_move_target(&current_canonical, &current_remote.remote_id)
                .await?
                .ok_or_else(|| {
                    SyncError::Domain(domain_core::DomainError::validation(format!(
                        "--relink requires the current remote {current_canonical}#{} to \
                         redirect; it does not",
                        current_remote.remote_id
                    )))
                })?;
            if target.0 != new_canonical || target.1 != new_remote_id {
                return Err(SyncError::Domain(domain_core::DomainError::validation(
                    format!(
                        "--relink target {new_canonical}#{new_remote_id} does not match \
                         GitHub's redirect target {}#{}",
                        target.0, target.1
                    ),
                )));
            }
            // Rewrite fields to the new remote's authoritative state so the
            // saved Link snapshot is a coherent baseline.
            let snap = self
                .provider
                .fetch_remote(new_canonical, new_remote_id)
                .await?;
            task.title = snap.title;
            task.body = snap.body;
            task.assignees = snap.assignees;
            // The fetched snapshot is the authoritative target, so carry its
            // node id onto the relinked ref — a relinked task should be just as
            // board-eligible as a freshly promoted one (RFC 0001 §9 / §D1).
            new_remote.node_id = snap.node_id;
            task.link_to_remote(binding.id, new_remote.clone(), false)?;
            let new_comments = self
                .provider
                .fetch_comments(new_canonical, new_remote_id)
                .await?;
            // Save first so a comment-write failure can't leave a deleted-but-
            // not-relinked state. `replace_comments` only touches synced rows
            // (pending stays via the '' sentinel), so a one-shot replace with
            // the new set both drops stale comments and inserts the new ones.
            self.tasks.save(&task, SnapshotSource::Link).await?;
            self.tasks.replace_comments(id, &new_comments).await?;
        }
        let mut note: Option<String> = None;
        if !relink {
            // Validate the new remote exists. `fetch_remote` post-checks the
            // followed-redirect response, so a transferred-issue source URL
            // surfaces as `IssueMoved`. For bare link that is *not* an error
            // — the user knowingly wants the source-side pointer, even
            // though the live issue is elsewhere. Capture the destination
            // in a note so the CLI can surface it.
            match self
                .provider
                .fetch_remote(new_canonical, new_remote_id)
                .await
            {
                Ok(_) => {}
                Err(PortError::IssueMoved {
                    to_canonical,
                    to_remote_id,
                    ..
                }) => {
                    note = Some(format!(
                        "github.com/{}#{new_remote_id} 301-redirects to {to_canonical}#{to_remote_id}; \
                         linked source URL as requested",
                        new_canonical.trim_start_matches("github.com/")
                    ));
                }
                Err(e) => return Err(SyncError::Port(e)),
            }
            task.link_to_remote(binding.id, new_remote.clone(), true)?;
            // Same ordering as the relink branch: commit the link first; then
            // clear synced comments (pending preserved by contract). If the
            // comment write fails, the task is still on the new remote and a
            // subsequent `sync pull` will refresh the synced set.
            self.tasks.save(&task, SnapshotSource::Link).await?;
            self.tasks.replace_comments(id, &[]).await?;
        }

        Ok(link_summary(
            &task,
            prev,
            if relink { "relinked" } else { "linked" },
            note,
        ))
    }

    /// Canonical URL of the task's **logical** repo — also the repo the issue
    /// is filed in today (until RFC 0002). Errors with `NoRepo` for an orphan
    /// task, since there is no repo to address.
    async fn canonical_for(&self, task: &Task) -> Result<String> {
        let repo_id = task.repo_id.ok_or(SyncError::NoRepo)?;
        let binding = self.bindings.get(repo_id).await?;
        Ok(binding.canonical_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use domain_core::{Timestamp, WorkspaceId};
    use domain_repo::RepoBinding;
    use domain_task::Task;
    use ports::{PortResult, RemoteComment, RemoteStateReason, RemoteTaskSnapshot};
    use std::sync::Mutex;
    use testing_fixtures::{InMemoryRepoBindingRepository, InMemoryTaskRepository};

    #[derive(Clone)]
    struct RecordedUpdate {
        remote_id: String,
        body: Option<String>,
        closed: Option<bool>,
        state_reason: Option<RemoteStateReason>,
    }

    #[derive(Default)]
    struct FakeProvider {
        last_create: Mutex<Option<String>>,
        last_update: Mutex<Option<RecordedUpdate>>,
        fetch_returns: Mutex<Option<RemoteTaskSnapshot>>,
        comments: Mutex<Vec<RemoteComment>>,
        created_comments: Mutex<Vec<String>>,
        move_target: Mutex<Option<(String, String)>>,
        fetch_moved: Mutex<Option<(String, String)>>,
    }

    impl FakeProvider {
        fn set_fetch(&self, snap: RemoteTaskSnapshot) {
            *self.fetch_returns.lock().unwrap() = Some(snap);
        }

        fn set_comments(&self, comments: Vec<RemoteComment>) {
            *self.comments.lock().unwrap() = comments;
        }

        fn set_move_target(&self, canonical: &str, remote_id: &str) {
            *self.move_target.lock().unwrap() = Some((canonical.into(), remote_id.into()));
        }

        /// Make the *next* `fetch_remote` call return `IssueMoved` with the
        /// supplied target — simulates a source-side URL that 301-redirects.
        fn set_fetch_moved(&self, to_canonical: &str, to_remote_id: &str) {
            *self.fetch_moved.lock().unwrap() = Some((to_canonical.into(), to_remote_id.into()));
        }
    }

    #[async_trait]
    impl RemoteTaskProvider for FakeProvider {
        async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
            *self.last_create.lock().unwrap() = Some(cmd.title.to_string());
            Ok(RemoteTaskSnapshot {
                remote_id: "100".into(),
                node_id: Some("I_kwDOfake100".into()),
                title: cmd.title.into(),
                body: cmd.body.into(),
                closed: false,
                updated_at: Timestamp::from_utc(Utc::now()),
                assignees: cmd.assignees.to_vec(),
                labels: cmd.labels.to_vec(),
            })
        }

        async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot> {
            *self.last_update.lock().unwrap() = Some(RecordedUpdate {
                remote_id: cmd.remote_id.into(),
                body: cmd.body.map(str::to_owned),
                closed: cmd.closed,
                state_reason: cmd.state_reason,
            });
            Ok(RemoteTaskSnapshot {
                remote_id: cmd.remote_id.into(),
                node_id: None,
                title: cmd.title.unwrap_or("").into(),
                body: cmd.body.unwrap_or("").into(),
                closed: cmd.closed.unwrap_or(false),
                updated_at: Timestamp::from_utc(Utc::now()),
                assignees: vec![],
                labels: vec![],
            })
        }

        async fn fetch_remote(
            &self,
            canonical: &str,
            remote_id: &str,
        ) -> PortResult<RemoteTaskSnapshot> {
            // `take()` so the staged "moved" response is one-shot — the next
            // fetch_remote after this falls through to fetch_returns.
            if let Some((to_c, to_r)) = self.fetch_moved.lock().unwrap().take() {
                return Err(PortError::IssueMoved {
                    from_canonical: canonical.to_string(),
                    from_remote_id: remote_id.to_string(),
                    to_canonical: to_c,
                    to_remote_id: to_r,
                });
            }
            self.fetch_returns
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| PortError::NotFound("no fetch fixture".into()))
        }

        async fn fetch_comments(&self, _: &str, _: &str) -> PortResult<Vec<RemoteComment>> {
            Ok(self.comments.lock().unwrap().clone())
        }

        async fn create_comment(&self, _: &str, _: &str, body: &str) -> PortResult<RemoteComment> {
            let mut created = self.created_comments.lock().unwrap();
            created.push(body.to_string());
            Ok(RemoteComment {
                remote_id: format!("c{}", created.len()),
                author: "remote-bot".into(),
                body: body.to_string(),
                created_at: Timestamp::from_utc(Utc::now()),
            })
        }

        async fn discover_move_target(
            &self,
            _: &str,
            _: &str,
        ) -> PortResult<Option<(String, String)>> {
            Ok(self.move_target.lock().unwrap().clone())
        }
    }

    async fn setup() -> (
        SyncService,
        Arc<InMemoryTaskRepository>,
        Task,
        Arc<FakeProvider>,
    ) {
        let (svc, tasks, _bindings, task, provider) = setup_with_bindings().await;
        (svc, tasks, task, provider)
    }

    async fn setup_with_bindings() -> (
        SyncService,
        Arc<InMemoryTaskRepository>,
        Arc<InMemoryRepoBindingRepository>,
        Task,
        Arc<FakeProvider>,
    ) {
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let provider = Arc::new(FakeProvider::default());

        let workspace_id = WorkspaceId::new();
        let binding = RepoBinding::new(
            workspace_id,
            "git@github.com:o/r.git".into(),
            "github.com/o/r".into(),
        )
        .unwrap();
        let repo_id = binding.id;
        bindings.save(&binding).await.unwrap();

        let task = Task::new_draft(workspace_id, Some(repo_id), "ship it".into()).unwrap();
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        let svc = SyncService::new(tasks.clone(), bindings.clone(), provider.clone());
        (svc, tasks, bindings, task, provider)
    }

    #[tokio::test]
    async fn promote_creates_remote_and_marks_pushed() {
        let (svc, tasks, task, provider) = setup().await;
        let s = svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(s.previous_state, "local_only");
        assert_eq!(s.new_state, "synced");
        assert_eq!(s.remote.as_ref().unwrap().provider, "github");
        assert_eq!(s.remote.as_ref().unwrap().remote_id, "100");
        assert_eq!(
            provider.last_create.lock().unwrap().as_deref(),
            Some("ship it")
        );
        // The node id from the REST create response is captured onto the
        // RemoteRef and persisted, so the promoted task is board-eligible
        // (rpl-4ui — the create/promote half of the bug).
        let saved = tasks.get(task.id).await.unwrap();
        assert_eq!(
            saved.remote.unwrap().node_id.as_deref(),
            Some("I_kwDOfake100")
        );
    }

    #[tokio::test]
    async fn promote_requires_repo_binding() {
        let (svc, tasks, _, _) = setup().await;
        let mut t = Task::new_draft(WorkspaceId::new(), None, "rogue".into()).unwrap();
        t.repo_id = None;
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
        let err = svc.promote(&t.id.to_string()).await.unwrap_err();
        assert!(matches!(err, SyncError::NoRepo));
    }

    #[tokio::test]
    async fn push_after_local_edit_marks_synced() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("revised".into());
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.previous_state, "dirty_local");
        assert_eq!(s.new_state, "synced");
        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.remote_id, "100");
        assert_eq!(recorded.body.as_deref(), Some("revised"));
    }

    #[tokio::test]
    async fn pull_applies_remote_snapshot_when_newer() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Remote has a newer updated_at and a different title.
        let later = Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: "new title".into(),
            body: "remote body".into(),
            closed: false,
            updated_at: later,
            assignees: vec!["bob".into()],
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "pull_remote");
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.title, "new title");
        assert_eq!(after.body, "remote body");
        assert_eq!(after.assignees, vec!["bob".to_string()]);
        assert_eq!(after.sync, SyncState::Synced);
    }

    #[tokio::test]
    async fn pull_backfills_missing_remote_node_id_on_noop() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Simulate a pre-project-sync task: drop the node id the promote
        // captured, leaving a Synced task with a remote_id but no node id —
        // exactly the row eager backfill can't add to a board.
        let mut t = tasks.get(task.id).await.unwrap();
        t.remote.as_mut().unwrap().node_id = None;
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        // Remote mirrors the local baseline (no field drift → Noop) but the
        // fetched snapshot now carries the node id.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: Some("I_kwDObackfilled".into()),
            title: t.title.clone(),
            body: t.body.clone(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: t.assignees.clone(),
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        // No title/body/assignee drift, so the snapshot axis is a noop...
        assert_eq!(s.decision, "noop");
        // ...yet the node id is still backfilled via the targeted column write.
        let saved = tasks.get(task.id).await.unwrap();
        assert_eq!(
            saved.remote.unwrap().node_id.as_deref(),
            Some("I_kwDObackfilled"),
            "pull backfills the node id even when there's no content drift"
        );
        assert_eq!(
            saved.sync,
            SyncState::Synced,
            "backfill must not perturb sync state"
        );
    }

    #[tokio::test]
    async fn push_archived_task_closes_remote_with_not_planned() {
        let (svc, tasks, task, provider) = setup().await;
        // Promote → Synced, then archive → DirtyLocal.
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.archive().unwrap();
        // archive() + reconcile_dirty_against_baseline transitions Synced → DirtyLocal.
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.new_state, "synced");

        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.remote_id, "100");
        assert_eq!(recorded.closed, Some(true));
        assert!(matches!(
            recorded.state_reason,
            Some(RemoteStateReason::NotPlanned)
        ));
    }

    #[tokio::test]
    async fn pull_noop_when_remote_unchanged() {
        let (svc, tasks, task, provider) = setup().await;
        // promote lands directly on Synced now (sync state transition is
        // collapsed into the promotion), so no extra mark_synced needed.
        svc.promote(&task.id.to_string()).await.unwrap();

        let before = tasks.get(task.id).await.unwrap();
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: false,
            updated_at: Timestamp::from_utc(
                before.updated_at.into_inner() - chrono::Duration::seconds(10),
            ),
            assignees: before.assignees.clone(),
            labels: vec![],
        });
        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "noop");
    }

    #[tokio::test]
    async fn pull_is_noop_when_only_updated_at_bumps() {
        // Regression for the issue this drift-hash work addresses: GitHub
        // bumps `updated_at` on any activity (comments, reactions, label
        // edits), so the old `snap.updated_at > task.updated_at` gate forced
        // cosmetic pull_remote on every comment. Field-level drift detection
        // must still say "noop" here.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let before = tasks.get(task.id).await.unwrap();

        // Remote `updated_at` is *newer*, but title / body / assignees are
        // identical to the baseline.
        let much_later = Timestamp::from_utc(Utc::now() + chrono::Duration::hours(1));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: false,
            updated_at: much_later,
            assignees: before.assignees.clone(),
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(
            s.decision, "noop",
            "non-mirrored remote activity must not trigger pull_remote"
        );
        // And no spurious Pull snapshot lands in history.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.sync, SyncState::Synced);
    }

    #[tokio::test]
    async fn pull_is_noop_when_remote_assignees_are_reordered() {
        // GitHub doesn't guarantee a stable assignee order across responses;
        // a re-ordering must not be detected as drift. Mirrors the
        // order-insensitive comparison already used by the domain's
        // reconcile_dirty_against_baseline.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Plant a baseline with two assignees in a known order.
        let mut t = tasks.get(task.id).await.unwrap();
        t.assignees = vec!["alice".into(), "bob".into()];
        // Re-promote the baseline by saving with a Pull source so the
        // synced_baseline reflects the new assignees.
        tasks.save(&t, SnapshotSource::Pull).await.unwrap();

        let much_later = Timestamp::from_utc(Utc::now() + chrono::Duration::hours(1));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: t.title.clone(),
            body: t.body.clone(),
            closed: false,
            updated_at: much_later,
            assignees: vec!["bob".into(), "alice".into()],
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(
            s.decision, "noop",
            "assignee re-ordering must not trigger pull_remote"
        );
    }

    #[tokio::test]
    async fn pull_is_noop_on_remote_comment_only_activity_but_still_mirrors_the_comment() {
        // Confirms the comments-as-separate-axis design under the new drift
        // logic: a remote comment lands locally even when the snapshot
        // decision is `noop` (no field churn). Comments are NOT part of the
        // drift signal.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let before = tasks.get(task.id).await.unwrap();

        let much_later = Timestamp::from_utc(Utc::now() + chrono::Duration::hours(1));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: false,
            updated_at: much_later,
            assignees: before.assignees.clone(),
            labels: vec![],
        });
        provider.set_comments(vec![RemoteComment {
            remote_id: "42".into(),
            author: "octocat".into(),
            body: "from remote".into(),
            created_at: Timestamp::from_utc(Utc::now()),
        }]);

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "noop");

        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(
            after.comments.len(),
            1,
            "remote comment must still land locally"
        );
        assert_eq!(after.comments[0].body, "from remote");
    }

    #[tokio::test]
    async fn pull_mirrors_comments_even_on_manual_merge_conflict() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Local edit → DirtyLocal, and a newer remote → remote_dirty. Under the
        // default ManualMerge policy this resolves to RequireManualMerge.
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("local edit".into());
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let later = Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: "remote title".into(),
            body: "remote body".into(),
            closed: false,
            updated_at: later,
            assignees: vec![],
            labels: vec![],
        });
        provider.set_comments(vec![RemoteComment {
            remote_id: "7".into(),
            author: "octocat".into(),
            body: "ping".into(),
            created_at: Timestamp::from_utc(Utc::now()),
        }]);

        let err = svc.pull(&task.id.to_string()).await.unwrap_err();
        assert!(matches!(err, SyncError::ManualMerge(_)));

        // The conflict still surfaces an error, but comments are mirrored anyway.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.sync, SyncState::Conflict);
        assert_eq!(after.comments.len(), 1);
        assert_eq!(after.comments[0].body, "ping");
    }

    #[tokio::test]
    async fn push_drains_pending_comments_on_synced_task() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(tasks.get(task.id).await.unwrap().sync, SyncState::Synced);

        // A pending comment must NOT have dirtied the task.
        tasks
            .add_pending_comment(task.id, "me", "hello world", Timestamp::now())
            .await
            .unwrap();
        assert_eq!(tasks.get(task.id).await.unwrap().sync, SyncState::Synced);

        let s = svc.push(&task.id.to_string()).await.unwrap();
        // Comment-only push: the snapshot axis is a noop, task stays synced.
        assert_eq!(s.decision, "noop");
        assert_eq!(s.new_state, "synced");

        // create_comment was called; update_remote (title/body) was NOT.
        assert_eq!(
            *provider.created_comments.lock().unwrap(),
            vec!["hello world".to_string()]
        );
        assert!(provider.last_update.lock().unwrap().is_none());

        // The pending comment is now synced.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.comments.len(), 1);
        assert!(after.comments[0].remote_id.is_some());
    }

    #[tokio::test]
    async fn push_drains_comments_and_snapshot_when_dirty() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("revised".into());
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
        tasks
            .add_pending_comment(task.id, "me", "also a comment", Timestamp::now())
            .await
            .unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "push_local");
        assert_eq!(s.new_state, "synced");

        // Both axes pushed.
        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.body.as_deref(), Some("revised"));
        assert_eq!(
            *provider.created_comments.lock().unwrap(),
            vec!["also a comment".to_string()]
        );

        let after = tasks.get(task.id).await.unwrap();
        assert!(after.comments.iter().all(|c| c.remote_id.is_some()));
    }

    #[tokio::test]
    async fn push_errors_when_clean_and_no_pending_comments() {
        let (svc, tasks, task, _provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        assert_eq!(tasks.get(task.id).await.unwrap().sync, SyncState::Synced);

        let err = svc.push(&task.id.to_string()).await.unwrap_err();
        assert!(matches!(err, SyncError::Domain(_)));
    }

    async fn attach_second_binding(
        bindings: &Arc<InMemoryRepoBindingRepository>,
        workspace_id: WorkspaceId,
        canonical: &str,
    ) {
        let b = RepoBinding::new(
            workspace_id,
            format!(
                "git@github.com:{}",
                canonical.trim_start_matches("github.com/")
            ),
            canonical.to_string(),
        )
        .unwrap();
        bindings.save(&b).await.unwrap();
    }

    #[tokio::test]
    async fn link_bare_flips_synced_to_conflict_and_drops_synced_comments() {
        let (svc, tasks, bindings, task, provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Pre-condition: the second binding must exist before link.
        let workspace_id = task.workspace_id;
        attach_second_binding(&bindings, workspace_id, "github.com/o2/r2").await;

        // Some synced comment that must be dropped on link.
        tasks
            .replace_comments(
                task.id,
                &[RemoteComment {
                    remote_id: "old".into(),
                    author: "x".into(),
                    body: "stale".into(),
                    created_at: Timestamp::from_utc(Utc::now()),
                }],
            )
            .await
            .unwrap();
        // Stub the new remote so the existence check inside link() succeeds.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "999".into(),
            node_id: None,
            title: "irrelevant".into(),
            body: "irrelevant".into(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: vec![],
            labels: vec![],
        });

        let s = svc
            .link(&task.id.to_string(), "github.com/o2/r2", "999", false)
            .await
            .unwrap();
        assert_eq!(s.decision, "linked");
        assert_eq!(s.new_state, "conflict");

        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.sync, SyncState::Conflict);
        assert_eq!(after.remote.as_ref().unwrap().remote_id, "999");
        assert!(after.comments.is_empty(), "synced comments must be dropped");
    }

    #[tokio::test]
    async fn link_relink_verified_keeps_synced_and_rewrites_baseline() {
        let (svc, tasks, bindings, task, provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let workspace_id = task.workspace_id;
        attach_second_binding(&bindings, workspace_id, "github.com/o2/r2").await;

        // The current remote (100, from promote) must report 301 to the target.
        provider.set_move_target("github.com/o2/r2", "1506");
        // The post-relink fetch_remote returns the new authoritative snapshot.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "1506".into(),
            node_id: Some("I_kwDOtransferred1506".into()),
            title: "transferred title".into(),
            body: "transferred body".into(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: vec!["alice".into()],
            labels: vec![],
        });

        let s = svc
            .link(&task.id.to_string(), "github.com/o2/r2", "1506", true)
            .await
            .unwrap();
        assert_eq!(s.decision, "relinked");
        assert_eq!(s.new_state, "synced", "verified relink preserves Synced");

        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.remote.as_ref().unwrap().remote_id, "1506");
        assert_eq!(after.title, "transferred title");
        // The relinked ref carries the node id from the authoritative target
        // snapshot, so a relinked task is board-eligible like a promoted one.
        assert_eq!(
            after.remote.as_ref().unwrap().node_id.as_deref(),
            Some("I_kwDOtransferred1506")
        );
        // Baseline rewritten from the new remote → reconcile sees no diff.
        assert_eq!(after.sync, SyncState::Synced);
    }

    #[tokio::test]
    async fn link_relink_target_mismatch_errors() {
        let (svc, _tasks, bindings, task, provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let workspace_id = task.workspace_id;
        attach_second_binding(&bindings, workspace_id, "github.com/o2/r2").await;

        // Current remote redirects, but to a DIFFERENT target than the user supplied.
        provider.set_move_target("github.com/o3/r3", "777");

        let err = svc
            .link(&task.id.to_string(), "github.com/o2/r2", "1506", true)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Domain(_)));
    }

    #[tokio::test]
    async fn link_relink_refuses_when_task_is_dirty_local() {
        let (svc, tasks, bindings, task, provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let workspace_id = task.workspace_id;
        attach_second_binding(&bindings, workspace_id, "github.com/o2/r2").await;

        // Make the task DirtyLocal (the typical state when a user hits the
        // move error on `sync push`). `--relink` must refuse rather than
        // silently overwrite their unpushed edits with the new remote's snap.
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_body("local edit at risk".into());
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
        provider.set_move_target("github.com/o2/r2", "1506");

        let err = svc
            .link(&task.id.to_string(), "github.com/o2/r2", "1506", true)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Domain(_)));
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.body, "local edit at risk", "local edit must survive");
        assert_eq!(after.sync, SyncState::DirtyLocal);
    }

    #[tokio::test]
    async fn link_errors_when_target_binding_missing() {
        let (svc, _tasks, _bindings, task, _provider) = setup_with_bindings().await;

        // No second binding attached → bare link should refuse with a clear hint.
        let err = svc
            .link(
                &task.id.to_string(),
                "github.com/never/attached",
                "1",
                false,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Domain(_)));
    }

    #[tokio::test]
    async fn link_bare_to_source_side_url_succeeds_with_note() {
        // The user-supplied URL 301-redirects (it's the *source* side of a
        // GitHub transfer). Bare `task link` should accept it — the user
        // wants the source-side pointer, even though the live issue is
        // elsewhere — and emit a note naming the redirect target.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // `fetch_remote(o/r#5788)` will report the issue moved to o2/r2#1506.
        provider.set_fetch_moved("github.com/o2/r2", "1506");

        let s = svc
            .link(&task.id.to_string(), "github.com/o/r", "5788", false)
            .await
            .unwrap();
        assert_eq!(s.decision, "linked");
        assert_eq!(s.new_state, "conflict");
        let note = s.note.expect("note must be set when source URL 301s");
        assert!(note.contains("5788"), "note names the source: {note}");
        assert!(note.contains("1506"), "note names the destination: {note}");

        // Task's remote points at the SOURCE URL as requested.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.remote.as_ref().unwrap().remote_id, "5788");
    }
}
