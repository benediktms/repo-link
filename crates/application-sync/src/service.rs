//! [`SyncService`] — orchestrates remote promotion / push / pull / link.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use domain_core::{RepoId, RepoOriginId, TaskId, Timestamp, WorkspaceId};
use domain_sync::{OutboxEntry, SyncDecision, SyncPolicy, decide, resolve_filing_repo};
use domain_task::{RelationKind, RemoteRef, SnapshotSource, SyncState, Task};
use domain_workspace::Workspace;

use crate::enqueue;
use dto_shared::SyncSummaryDto;
use ports::{
    PortError, RemoteTaskCreate, RemoteTaskProvider, RepoBindingRepository, SyncedSource,
    TaskRepository, WorkspaceRepository,
};

use crate::error::{Result, SyncError};
use crate::summary::{
    ensure_not_archived, link_summary, provider_label, remote_mirrors_baseline, summary,
};

/// Outcome of [`SyncService::refresh`] (`rl task show --refresh`, RFC 0004 D4).
/// A *fetch* failure propagates as `Err` so the caller can decide whether it's
/// fatal (e.g. `IssueMoved` is actionable) or should degrade to a cached render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The task has no REST issue to observe — purely local, or draft-backed
    /// (whose board freshness is the poller's job, not `--refresh`). No fetch
    /// was attempted and nothing was stamped.
    NotIssueBacked,
    /// The remote was observed and `synced_at` was stamped.
    Stamped,
}

pub struct SyncService {
    tasks: Arc<dyn TaskRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
    // Needed to read the workspace default filing repo (`filing_repo_id`) for
    // step 2 of the RFC 0002 D2 chain when resolving where to file at promote.
    workspaces: Arc<dyn WorkspaceRepository>,
    provider: Arc<dyn RemoteTaskProvider>,
    policy: SyncPolicy,
}

impl SyncService {
    pub fn new(
        tasks: Arc<dyn TaskRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
        workspaces: Arc<dyn WorkspaceRepository>,
        provider: Arc<dyn RemoteTaskProvider>,
    ) -> Self {
        Self {
            tasks,
            bindings,
            workspaces,
            provider,
            policy: SyncPolicy::ManualMerge,
        }
    }

    pub fn with_policy(mut self, policy: SyncPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Stage (if needed) and promote a `LocalOnly`/`Staged` task to a remote
    /// issue. The issue is filed in the RFC 0002 D2 resolved filing repo
    /// (per-task override → workspace default → logical `repo_id`), which is
    /// recorded on the task before the remote is created. With no override or
    /// workspace default the chain collapses to the logical repo, so filing is
    /// unchanged from before RFC 0002. `previous_state` / `new_state` in the
    /// summary describe the **sync** state — lifecycle stays untouched.
    pub async fn promote(&self, task_id: &str) -> Result<SyncSummaryDto> {
        let id: TaskId = task_id.parse()?;
        let mut task = self.tasks.get(id).await?;
        ensure_not_archived(&task)?;
        let prev = task.sync;

        if task.sync == SyncState::LocalOnly {
            task.stage_for_sync()?;
        }
        if task.sync != SyncState::Staged {
            return Err(SyncError::Domain(domain_core::DomainError::transition(
                format!("cannot promote from sync={:?}", task.sync),
            )));
        }

        // RFC 0002 D2: promote is a first-filing transition, so resolve the
        // filing repo and record it on the task before creating the remote.
        // The per-task override is carried on the draft, not resolved here
        // (CLI lands in #122); step 2 reads the workspace default. With both
        // absent the chain collapses to the logical `repo_id`, so the issue is
        // filed exactly where it is today. `set_filing_repo_id(None)` is a
        // no-op when nothing resolved (orphan with no default), preserving the
        // existing "promote needs a repo" error via `filing_canonical_for`.
        let workspace = self.workspaces.get(task.workspace_id).await?;
        // Convert RepoId (instance id space) to RepoOriginId for the chain.
        // workspace.filing_repo_id already holds an origin id after the RFC 0005
        // migration; task.repo_id is an instance id → map to origin via the binding.
        let ws_default_origin = workspace
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        let logical_origin = if let Some(repo_id) = task.repo_id {
            match self.bindings.get(repo_id).await {
                Ok(v) => Some(v.instance.origin_id),
                Err(ports::PortError::NotFound(_)) => None,
                Err(e) => return Err(SyncError::Port(e)),
            }
        } else {
            None
        };
        let filing_origin = resolve_filing_repo(None, ws_default_origin, logical_origin);
        // Convert back to RepoId for set_filing_repo_id (domain field is Option<RepoId>)
        let filing = filing_origin.map(|o| RepoId::from_uuid(o.as_uuid()));
        task.set_filing_repo_id(filing)?;
        let filing_canonical = self.filing_canonical_for(&task).await?;

        let snap = self
            .provider
            .create_remote(RemoteTaskCreate {
                canonical_repo: &filing_canonical,
                title: &task.title,
                body: &task.body,
                assignees: &task.assignees,
                labels: &[],
            })
            .await?;

        let mut remote_ref =
            RemoteRef::new(provider_label(&filing_canonical), snap.remote_id.clone());
        // Capture the GraphQL node id the REST create response carried, so the
        // freshly promoted task is immediately board-eligible (the §D1 AddItem
        // path needs it). Dropping it here was the create/promote half of the
        // bug — a promoted task landed with node_id null and never reached the
        // board (RFC 0001 §9 / §D1).
        remote_ref.node_id = snap.node_id.clone();
        task.promote_to_remote(remote_ref)?;

        // Relation backfill (#151): edges set while this task was local-only were
        // skipped at relate-time (the gate requires both ends issue-backed). Now
        // that the task has an issue, re-scan its edges and enqueue the native
        // mutation for every neighbor that is itself issue-backed. Relations are
        // stored reciprocally, so scanning this task's own edges also covers the
        // case where the NEIGHBOR was the side that promoted first and skipped.
        // Enqueued atomically with the promote save (a torn write would leave the
        // relation permanently unsynced — relations have no dirty backstop).
        let this_coords = (filing_canonical.clone(), snap.remote_id.clone());
        let entries = self.relation_backfill_entries(&task, &this_coords).await?;
        self.tasks
            .save_with_outbox(&task, SnapshotSource::Promote, &entries)
            .await?;
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
        let filing_canonical = self.filing_canonical_for(&task).await?;
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
            // Build the PATCH from the live-vs-baseline diff. The shared
            // helper is the single point that decides which fields ride
            // the PATCH and when the call is skipped (empty patch ⇒
            // no PATCH). The drainer's `UpdateRemote` arm goes through
            // the same function — no drift possible.
            //
            // Re-baseline ONLY the fields actually transmitted (RFC 0003
            // D5): an untransmitted field (e.g. assignees when only a
            // title was pushed) must stay dirty and get re-pushed
            // later, instead of being silently rebaselined to the
            // un-pushed local value by a whole-snapshot rebaseline.
            // We thread the same `patch` we just sent so the rebaseline
            // and the PATCH cannot disagree on what was transmitted.
            let patch = task.diff_against_baseline();
            let canonical_repo = filing_canonical.clone();
            let remote_id = remote.remote_id.clone();
            if let Some(update) =
                crate::build_update_from_patch(&task, &patch, &canonical_repo, &remote_id)
            {
                self.provider.update_remote(update).await?;
            }

            task.confirm_synced_fields(SnapshotSource::Push, &patch)?;
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
                        .create_comment(&filing_canonical, &remote.remote_id, &comment.body)
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
        // rpl-s7k: pull resolves the filing repo via the same D2 chain as
        // promote (recorded `filing_repo_id` → workspace default → logical
        // `repo_id`) — see `filing_canonical_for`. A `NotFound` here means
        // a binding has been deleted; the durable fix is `rl repo doctor
        // --repair` to re-point the column. The previous rpl-sv2 soft-fall
        // is gone: it could only ever look up the same `repo_id` the
        // helper already tried, so it never resolved a binding the helper
        // missed.
        let filing_canonical = self.filing_canonical_for(&task).await?;
        let remote = task.remote.as_ref().ok_or(SyncError::NoRemote)?.clone();
        let prev = task.sync;

        let snap = self
            .provider
            .fetch_remote(&filing_canonical, &remote.remote_id)
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
                // re-trigger dirty detection against the OLD baseline). The
                // inbound set now includes the open/closed bit (RFC 0004 D1):
                // `copy_inbound_mirror_from_snap` reconciles the lifecycle from
                // `snap.closed` via `remote_state_to_lifecycle`. Routed through
                // the shared helper so the inbound copy shape is one function
                // signature, not a literal hand-rolled at two call sites.
                crate::copy_inbound_mirror_from_snap(
                    &mut task,
                    &snap.title,
                    &snap.body,
                    &snap.assignees,
                    snap.closed,
                );
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
            .fetch_comments(&filing_canonical, &remote.remote_id)
            .await?;
        self.tasks.replace_comments(id, &comments).await?;

        // Inbound relation reconcile (#150): bring local parent/child + blocked_by
        // edges in line with the issue's GitHub sub-issues and dependencies.
        // Orthogonal to the title/body drift decision (like comments), so it runs
        // on every pull including Noop / conflict. Applied via a plain save — NOT
        // save_with_outbox — so a relation pulled FROM GitHub does not re-enqueue
        // an outbound mutation back TO GitHub (which would loop).
        self.reconcile_relations_inbound(&mut task, &filing_canonical, &remote.remote_id)
            .await?;

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

    /// `rl task show --refresh` (RFC 0004 D4): observe the remote to stamp
    /// `synced_at`, WITHOUT reconciling content. The distinction from
    /// [`SyncService::pull`] is deliberate — `--refresh` never overwrites local
    /// title/body/assignees/lifecycle; it only refreshes the freshness clock so
    /// a subsequent offline `task show` reports an up-to-date "last refreshed".
    ///
    /// Errors PROPAGATE (fetch failure, deleted filing binding, `IssueMoved`):
    /// the caller decides which are fatal (e.g. `IssueMoved` is actionable —
    /// the link is wrong) and which should degrade to rendering the cached
    /// value with a `last_refresh_failed` annotation. Keeping that policy at the
    /// CLI mirrors `sync pull`'s `IssueMoved` enrichment instead of flattening
    /// every failure into a generic note.
    ///
    /// No `ensure_not_archived` guard (unlike `pull`): refresh is observe-only,
    /// so stamping an archived task's freshness clock is harmless — there is no
    /// content resurrection to guard against.
    pub async fn refresh(&self, task_id: &str) -> Result<RefreshOutcome> {
        let id: TaskId = task_id.parse()?;
        let task = self.tasks.get(id).await?;
        // Only an issue-backed task has a REST issue to observe. A purely-local
        // or draft-backed task (`remote.is_none()`) has nothing to fetch.
        let Some(remote) = task.remote.as_ref() else {
            return Ok(RefreshOutcome::NotIssueBacked);
        };
        let remote_id = remote.remote_id.clone();
        let filing_canonical = self.filing_canonical_for(&task).await?;
        // Observe-only: a successful fetch stamps freshness via the targeted
        // single-column write; the snapshot's content is intentionally
        // discarded (reconciling it is `sync pull`, not `--refresh`). Any fetch
        // error propagates for the caller to classify.
        self.provider
            .fetch_remote(&filing_canonical, &remote_id)
            .await?;
        self.tasks
            .cache_synced_at(id, Timestamp::now(), SyncedSource::Refresh)
            .await?;
        Ok(RefreshOutcome::Stamped)
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
            && self.logical_canonical_for(&task).await.ok().as_deref() == Some(new_canonical)
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
            let current_canonical = self.logical_canonical_for(&task).await?;
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
            // Routed through the shared copy helper so the 3-field shape
            // is a single function signature. The post-relink invariant
            // (live task matches the new Pull baseline on the inbound
            // set) is pinned end-to-end by
            // `relink_copy_back_uses_the_same_inbound_set_as_remote_mirrors_baseline`.
            //
            // Note: a same-content relink (new remote has identical
            // title/body/assignees to the pre-relink baseline) is
            // legitimate — the user is pointing at a moved issue whose
            // content hasn't changed, e.g. an org-wide repo rename
            // without content edit. The pre-condition check
            // `!helper(snap, baseline)` was REMOVED for that reason; it
            // would have panicked on a same-content relink in debug
            // builds after `fetch_remote` already succeeded.
            crate::copy_inbound_mirror_from_snap(
                &mut task,
                &snap.title,
                &snap.body,
                &snap.assignees,
                snap.closed,
            );
            // The fetched snapshot is the authoritative target, so carry its
            // node id onto the relinked ref — a relinked task should be just as
            // board-eligible as a freshly promoted one (RFC 0001 §9 / §D1).
            new_remote.node_id = snap.node_id;
            task.link_to_remote(binding.instance.id, new_remote.clone(), false)?;
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
            task.link_to_remote(binding.instance.id, new_remote.clone(), true)?;
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
    async fn logical_canonical_for(&self, task: &Task) -> Result<String> {
        let repo_id = task.repo_id.ok_or(SyncError::NoRepo)?;
        let view = self.bindings.get(repo_id).await?;
        Ok(view.instance.canonical_url)
    }

    /// Canonical URL of the repo the task's backing issue is *filed* in
    /// (RFC 0002). Walks the D2 chain — recorded `filing_repo_id` →
    /// workspace default → logical `repo_id` — via `resolve_filing_repo`.
    /// Single source of truth for every remote-issue address (create /
    /// update / fetch / comment / relation reconcile); the logical-only
    /// lookup stays for logical-binding ops (prefix / worktree / relink).
    ///
    /// **Caveat**: a saga task created when the workspace had no filing
    /// default will silently flip to a later default on its next
    /// mutation. Pin step 1 (`filing_repo_id`) on the task — promote
    /// records it automatically — to make the resolution permanent.
    async fn filing_canonical_for(&self, task: &Task) -> Result<String> {
        // RFC 0005 §D4: filing_repo_id holds ORIGIN id bytes. Steps 1+2 are
        // already in origin id space; step 3 (logical repo_id) is an instance
        // id that must be mapped to its origin first.
        //
        // A deleted workspace row is not a hard error here: it just means
        // step 2 of the D2 chain (workspace default) is unavailable, so
        // resolve with `workspace_default = None` and let step 1
        // (recorded `filing_repo_id`) or step 3 (logical `repo_id`) win.
        // Only when the chain itself returns `None` — meaning all three
        // inputs are absent — do we surface `NoRepo`. (CodeRabbit #191.)
        let workspace_default = match self.workspaces.get(task.workspace_id).await {
            Ok(ws) => ws
                .filing_repo_id
                .map(|r| RepoOriginId::from_uuid(r.as_uuid())),
            Err(ports::PortError::NotFound(_)) => None,
            Err(e) => return Err(SyncError::Port(e)),
        };
        // Step 1: recorded filing_repo_id (already in origin id space)
        let step1 = task
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        // Step 3: logical instance → origin
        let step3 = if let Some(repo_id) = task.repo_id {
            match self.bindings.get(repo_id).await {
                Ok(v) => Some(v.instance.origin_id),
                Err(ports::PortError::NotFound(_)) => None,
                Err(e) => return Err(SyncError::Port(e)),
            }
        } else {
            None
        };
        let origin_id =
            resolve_filing_repo(step1, workspace_default, step3).ok_or(SyncError::NoRepo)?;
        let origin = self.bindings.get_origin(origin_id).await?;
        Ok(origin.canonical_url)
    }

    /// Plan the outbox entries a just-promoted task owes for its existing
    /// relations: one native mutation per edge whose far end is issue-backed.
    /// `this_coords` is the promoting task's own `(filing_canonical, remote_id)`.
    async fn relation_backfill_entries(
        &self,
        task: &Task,
        this_coords: &(String, String),
    ) -> Result<Vec<OutboxEntry>> {
        let mut entries = Vec::new();
        // One `workspaces.get` per unique neighbor workspace, not per edge.
        let mut workspace_cache: HashMap<WorkspaceId, Workspace> = HashMap::new();
        for rel in &task.relations {
            let neighbor = self.tasks.get(rel.other).await?;
            let Some(neighbor_coords) = self
                .relation_remote_coords(&neighbor, &mut workspace_cache)
                .await?
            else {
                continue; // far end not issue-backed → not yet projectable
            };
            if let Some(m) =
                enqueue::relation_mutation(rel.kind, true, this_coords, &neighbor_coords)
            {
                entries.push(OutboxEntry::new(task.id, m));
            }
        }
        Ok(entries)
    }

    /// Issue coordinates `(filing_canonical, remote_id)` for a relation
    /// endpoint, or `None` when the task isn't issue-backed or its filing
    /// repo can't be resolved — in either case the relation can't be
    /// projected onto GitHub, so the caller skips it. Errors propagate so
    /// a transient I/O failure on `workspaces.get` / `bindings.get`
    /// bubbles up instead of silently dropping the relation.
    ///
    /// The `workspace_cache` collapses repeated `workspaces.get` calls
    /// across a backfill loop into one per unique `WorkspaceId`. The
    /// chain itself runs inline against the cached workspace, so
    /// `relation_backfill_entries` does one `workspaces.get` per unique
    /// neighbor workspace, not one per relation. A missing workspace is
    /// "no resolvable home" (`Ok(None)`) — see the pre-chain-port
    /// behavior where the helper was a pure in-memory
    /// `task.filing_repo_id.or(task.repo_id)`.
    async fn relation_remote_coords(
        &self,
        task: &Task,
        workspace_cache: &mut HashMap<WorkspaceId, Workspace>,
    ) -> Result<Option<(String, String)>> {
        let workspace = match workspace_cache.entry(task.workspace_id) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                match self.workspaces.get(task.workspace_id).await {
                    Ok(ws) => v.insert(ws),
                    Err(ports::PortError::NotFound(_)) => return Ok(None),
                    Err(e) => return Err(SyncError::Port(e)),
                }
            }
        };
        let Some(remote) = task.remote.as_ref() else {
            return Ok(None);
        };
        // RFC 0005: convert to origin id space for the chain
        let step1 = task
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        let ws_default = workspace
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        let step3 = if let Some(repo_id) = task.repo_id {
            match self.bindings.get(repo_id).await {
                Ok(v) => Some(v.instance.origin_id),
                Err(ports::PortError::NotFound(_)) => None,
                Err(e) => return Err(SyncError::Port(e)),
            }
        } else {
            None
        };
        let Some(origin_id) = resolve_filing_repo(step1, ws_default, step3) else {
            return Ok(None);
        };
        let canonical = self.bindings.get_origin(origin_id).await?.canonical_url;
        Ok(Some((canonical, remote.remote_id.clone())))
    }

    /// Inbound relation reconcile (#150): align `task`'s local parent/child and
    /// blocked_by edges with the issue's GitHub sub-issues and dependencies.
    /// Remote is authoritative for edges between two issue-backed local tasks;
    /// edges whose far end isn't a tracked local task (cross-repo, or never
    /// imported) are left untouched, as are edges to local-only tasks the remote
    /// can't represent. Saves via plain `save_many` so no outbound re-enqueue.
    async fn reconcile_relations_inbound(
        &self,
        task: &mut Task,
        filing_canonical: &str,
        remote_id: &str,
    ) -> Result<()> {
        let subs = self
            .provider
            .fetch_sub_issues(filing_canonical, remote_id)
            .await?;
        let blockers = self
            .provider
            .fetch_blocked_by(filing_canonical, remote_id)
            .await?;
        let desired_children = self.map_remote_to_local(task.workspace_id, &subs).await?;
        let desired_blockers = self
            .map_remote_to_local(task.workspace_id, &blockers)
            .await?;

        // Neighbor tasks whose reciprocal edge we touch, accumulated so each is
        // loaded and saved once even if it appears in both families.
        let mut neighbors: HashMap<TaskId, Task> = HashMap::new();
        let mut task_changed = self
            .reconcile_family(
                task,
                RelationKind::ParentOf,
                RelationKind::ChildOf,
                &desired_children,
                &mut neighbors,
            )
            .await?;
        task_changed |= self
            .reconcile_family(
                task,
                RelationKind::BlockedBy,
                RelationKind::Blocks,
                &desired_blockers,
                &mut neighbors,
            )
            .await?;

        // Persist whenever the subject task changed — not gated on
        // `neighbors`. Today every edge change also loads a neighbor, but gating
        // the subject's save on a non-empty neighbor map would silently drop a
        // future subject-only mutation. The neighbors ride along in the batch.
        if task_changed {
            let mut batch: Vec<(&Task, SnapshotSource)> = vec![(task, SnapshotSource::Pull)];
            batch.extend(neighbors.values().map(|n| (n, SnapshotSource::Pull)));
            self.tasks.save_many(&batch).await?;
        }
        Ok(())
    }

    /// Resolve each remote related issue to its local task id, dropping any that
    /// aren't tracked locally (binding for the issue's repo not attached, or the
    /// issue never imported). Keyed on the FILING repo per D6 `find_by_remote`.
    async fn map_remote_to_local(
        &self,
        workspace_id: domain_core::WorkspaceId,
        related: &[ports::RemoteChildIssue],
    ) -> Result<Vec<TaskId>> {
        let mut ids = Vec::new();
        for item in related {
            let Some(binding) = self
                .bindings
                .find_by_canonical_url(workspace_id, &item.canonical_repo)
                .await?
            else {
                continue; // related issue's repo not tracked in this workspace
            };
            if let Some(t) = self
                .tasks
                .find_by_remote(
                    binding.instance.origin_id,
                    "github",
                    &item.snapshot.remote_id,
                )
                .await?
            {
                ids.push(t.id);
            }
        }
        Ok(ids)
    }

    /// Reconcile one relation family on `task` toward `desired` (the local task
    /// ids the remote says are related via `kind`). Adds missing edges and
    /// removes edges the remote dropped — but only removals whose far end is
    /// itself issue-backed (remotely representable); a local-only neighbor the
    /// remote can't express is left alone. Reciprocals are mirrored onto the
    /// neighbors, which collect into `neighbors` for the caller's batch save.
    /// Returns whether `task`'s own edge set changed (so the caller knows to
    /// persist the subject even if it ever stops loading a neighbor per change).
    async fn reconcile_family(
        &self,
        task: &mut Task,
        kind: RelationKind,
        inverse: RelationKind,
        desired: &[TaskId],
        neighbors: &mut HashMap<TaskId, Task>,
    ) -> Result<bool> {
        let task_id = task.id;
        let mut changed = false;
        let desired_set: HashSet<TaskId> = desired.iter().copied().collect();
        let current: Vec<TaskId> = task
            .relations
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| r.other)
            .collect();
        let current_set: HashSet<TaskId> = current.iter().copied().collect();

        // Additions: remote has the edge, local doesn't.
        for &other in desired {
            if other == task_id || current_set.contains(&other) {
                continue;
            }
            let neighbor = self.load_neighbor(neighbors, other).await?;
            task.add_relation(kind, other);
            neighbor.add_relation(inverse, task_id);
            changed = true;
        }
        // Removals: local has the edge, remote dropped it — only when the far
        // end is issue-backed, so an unrepresentable local-only edge survives.
        for other in current {
            if other == task_id || desired_set.contains(&other) {
                continue;
            }
            let neighbor = self.load_neighbor(neighbors, other).await?;
            if neighbor.remote.is_none() {
                continue; // remote can't represent a local-only neighbor
            }
            task.remove_relation(kind, other);
            neighbor.remove_relation(inverse, task_id);
            changed = true;
        }
        Ok(changed)
    }

    /// Load `id` into the neighbor cache if absent, returning a mutable handle.
    // The `entry` API can't host the fallible async `tasks.get` between the miss
    // check and the insert, so the contains/insert split is intentional.
    #[allow(clippy::map_entry)]
    async fn load_neighbor<'a>(
        &self,
        neighbors: &'a mut HashMap<TaskId, Task>,
        id: TaskId,
    ) -> Result<&'a mut Task> {
        if !neighbors.contains_key(&id) {
            let t = self.tasks.get(id).await?;
            neighbors.insert(id, t);
        }
        Ok(neighbors.get_mut(&id).expect("just inserted"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use domain_core::{RepoId, Timestamp, WorkspaceId};
    use domain_repo::{RepoInstance, RepoOrigin};
    use domain_sync::OutboxMutation;
    use domain_task::Task;
    use domain_workspace::{Workspace, WorkspaceName};
    use ports::{
        PortResult, RemoteComment, RemoteStateReason, RemoteTaskSnapshot, RemoteTaskUpdate,
    };
    use std::sync::Mutex;
    use testing_fixtures::{
        InMemoryOutboxRepository, InMemoryRepoBindingRepository, InMemoryTaskRepository,
        InMemoryWorkspaceRepository,
    };

    /// Asserts the post-mutation task's inbound mirror set matches its
    /// new baseline. Shared by the pull and relink copy-back tests
    /// (`*_copy_back_uses_the_same_inbound_set_as_remote_mirrors_baseline`)
    /// — both test the same property at the same point in the lifecycle
    /// (right after the copy-back has run and the new baseline has been
    /// captured). `context` is the test name fragment used in the
    /// `expect`/`assert` messages so failures point at the right caller.
    fn assert_inbound_set_matches_baseline(after: &Task, context: &str) {
        let baseline = after
            .synced_baseline
            .clone()
            .unwrap_or_else(|| panic!("post-{context} baseline"));
        assert!(
            crate::inbound_mirrors_baseline(
                &after.title,
                &after.body,
                &after.assignees,
                !after.lifecycle.is_open(),
                &baseline,
            ),
            "post-{context} task and baseline must agree on the inbound set ({context} resolved the drift)"
        );
    }

    #[derive(Clone)]
    struct RecordedUpdate {
        remote_id: String,
        title: Option<String>,
        body: Option<String>,
        closed: Option<bool>,
        state_reason: Option<RemoteStateReason>,
        assignees: Option<Vec<String>>,
    }

    #[derive(Default)]
    struct FakeProvider {
        last_create: Mutex<Option<String>>,
        last_create_canonical: Mutex<Option<String>>,
        last_update: Mutex<Option<RecordedUpdate>>,
        // Canonical URL of the most recent `fetch_remote` call — pinned
        // by the rpl-s7k tests to assert the D2 chain resolved to the
        // expected repo (workspace default, not logical).
        last_fetch_canonical: Mutex<Option<String>>,
        // Canonical URL of the most recent `update_remote` call —
        // pinned by the rpl-s7k tests to assert push's D2 chain
        // resolved to the expected repo.
        last_update_canonical: Mutex<Option<String>>,
        fetch_returns: Mutex<Option<RemoteTaskSnapshot>>,
        comments: Mutex<Vec<RemoteComment>>,
        created_comments: Mutex<Vec<String>>,
        move_target: Mutex<Option<(String, String)>>,
        fetch_moved: Mutex<Option<(String, String)>>,
        sub_issues: Mutex<Vec<ports::RemoteChildIssue>>,
        blocked_by: Mutex<Vec<ports::RemoteChildIssue>>,
    }

    impl FakeProvider {
        fn set_fetch(&self, snap: RemoteTaskSnapshot) {
            *self.fetch_returns.lock().unwrap() = Some(snap);
        }

        fn set_comments(&self, comments: Vec<RemoteComment>) {
            *self.comments.lock().unwrap() = comments;
        }

        /// Seed the issue's GitHub sub-issues / blocked_by deps for inbound
        /// relation reconcile, as `(canonical, number)` pairs.
        fn set_sub_issues(&self, items: &[(&str, &str)]) {
            *self.sub_issues.lock().unwrap() = items.iter().map(|(c, n)| child(c, n)).collect();
        }

        fn set_blocked_by(&self, items: &[(&str, &str)]) {
            *self.blocked_by.lock().unwrap() = items.iter().map(|(c, n)| child(c, n)).collect();
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
            *self.last_create_canonical.lock().unwrap() = Some(cmd.canonical_repo.to_string());
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
            *self.last_update_canonical.lock().unwrap() = Some(cmd.canonical_repo.to_string());
            *self.last_update.lock().unwrap() = Some(RecordedUpdate {
                remote_id: cmd.remote_id.into(),
                title: cmd.title.map(str::to_owned),
                body: cmd.body.map(str::to_owned),
                closed: cmd.closed,
                state_reason: cmd.state_reason,
                assignees: cmd.assignees.map(|s| s.to_vec()),
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
            *self.last_fetch_canonical.lock().unwrap() = Some(canonical.to_string());
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

        async fn fetch_sub_issues(
            &self,
            _: &str,
            _: &str,
        ) -> PortResult<Vec<ports::RemoteChildIssue>> {
            Ok(self.sub_issues.lock().unwrap().clone())
        }

        async fn fetch_blocked_by(
            &self,
            _: &str,
            _: &str,
        ) -> PortResult<Vec<ports::RemoteChildIssue>> {
            Ok(self.blocked_by.lock().unwrap().clone())
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
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let provider = Arc::new(FakeProvider::default());

        // Seed the workspace so promote's D2 step-2 lookup
        // (`workspaces.get(task.workspace_id)`) resolves. No filing default
        // here, so resolution falls through to the logical repo as today.
        let workspace = Workspace::new(WorkspaceName::new("sync-ws").unwrap(), None, true);
        let workspace_id = workspace.id;
        workspaces.save(&workspace).await.unwrap();

        let origin =
            RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into()).unwrap();
        bindings.save_origin(&origin).await.unwrap();
        let instance =
            RepoInstance::new(workspace_id, origin.id, "github.com/o/r".into(), None).unwrap();
        let repo_id = instance.id;
        bindings.save_instance(&instance).await.unwrap();

        let task = Task::new_draft(workspace_id, Some(repo_id), "ship it".into()).unwrap();
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        let svc = SyncService::new(
            tasks.clone(),
            bindings.clone(),
            workspaces.clone(),
            provider.clone(),
        );
        (svc, tasks, bindings, task, provider)
    }

    /// Build a `RemoteChildIssue` (`canonical`, `number`) for seeding the
    /// inbound sub-issue / dependency stubs.
    fn child(canonical: &str, number: &str) -> ports::RemoteChildIssue {
        ports::RemoteChildIssue {
            canonical_repo: canonical.into(),
            snapshot: RemoteTaskSnapshot {
                remote_id: number.into(),
                node_id: None,
                title: format!("issue {number}"),
                body: String::new(),
                closed: false,
                updated_at: Timestamp::from_utc(Utc::now()),
                assignees: vec![],
                labels: vec![],
            },
        }
    }

    /// A `SyncService` whose task repo shares `outbox`, plus a helper to seed an
    /// already-promoted (issue-backed) task. Used by the relation-sync tests
    /// that need to inspect enqueued mutations and resolve neighbor coords.
    /// Returns `(svc, tasks, bindings, outbox, workspace_id, repo_id, filing_origin_as_repo_id, provider)`.
    /// `repo_id` is the instance id; `filing_origin_as_repo_id` is the origin id
    /// wrapped as `RepoId` for use in `seed_promoted`'s `filing_repo_id`.
    async fn setup_with_outbox() -> (
        SyncService,
        Arc<InMemoryTaskRepository>,
        Arc<InMemoryRepoBindingRepository>,
        Arc<InMemoryOutboxRepository>,
        WorkspaceId,
        domain_core::RepoId,
        domain_core::RepoId,
        Arc<FakeProvider>,
    ) {
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let tasks = Arc::new(InMemoryTaskRepository::with_outbox(&outbox));
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let provider = Arc::new(FakeProvider::default());

        let workspace = Workspace::new(WorkspaceName::new("sync-ws").unwrap(), None, true);
        let workspace_id = workspace.id;
        workspaces.save(&workspace).await.unwrap();
        let origin =
            RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into()).unwrap();
        let filing_origin_as_repo_id = domain_core::RepoId::from_uuid(origin.id.as_uuid());
        bindings.save_origin(&origin).await.unwrap();
        let instance =
            RepoInstance::new(workspace_id, origin.id, "github.com/o/r".into(), None).unwrap();
        let repo_id = instance.id;
        bindings.save_instance(&instance).await.unwrap();

        let svc = SyncService::new(
            tasks.clone(),
            bindings.clone(),
            workspaces.clone(),
            provider.clone(),
        );
        (
            svc,
            tasks,
            bindings,
            outbox,
            workspace_id,
            repo_id,
            filing_origin_as_repo_id,
            provider,
        )
    }

    /// Persist an already-promoted, issue-backed task bound to `repo_id`.
    /// `filing_repo_id` must be the origin id (as `RepoId`) for `find_by_remote`
    /// to resolve this task during relation reconciliation.
    async fn seed_promoted(
        tasks: &Arc<InMemoryTaskRepository>,
        ws: WorkspaceId,
        repo_id: domain_core::RepoId,
        filing_repo_id: domain_core::RepoId,
        remote_id: &str,
    ) -> Task {
        let mut t = Task::new_draft(ws, Some(repo_id), format!("issue {remote_id}")).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", remote_id))
            .unwrap();
        // RFC 0005 §D4: filing_repo_id is in origin id space. Set it manually
        // here because seed_promoted bypasses the service promote path.
        t.set_filing_repo_id(Some(filing_repo_id)).unwrap();
        tasks.save(&t, SnapshotSource::Promote).await.unwrap();
        t
    }

    #[tokio::test]
    async fn promote_backfills_existing_relation_to_issue_backed_neighbor() {
        // A child task related to an already-promoted parent BEFORE the child
        // had an issue: relate-time skipped it (child was local-only). Promoting
        // the child must now backfill the AddSubIssue mutation.
        let (svc, tasks, _b, outbox, ws, repo_id, filing_id, _p) = setup_with_outbox().await;
        let parent = seed_promoted(&tasks, ws, repo_id, filing_id, "200").await;

        // Local draft child, related child_of the promoted parent.
        let mut child_task = Task::new_draft(ws, Some(repo_id), "child".into()).unwrap();
        child_task.add_relation(RelationKind::ChildOf, parent.id);
        tasks
            .save(&child_task, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        svc.promote(&child_task.id.to_string()).await.unwrap();

        let entries = outbox.all();
        assert_eq!(entries.len(), 1, "promote backfills the one eligible edge");
        match &entries[0].mutation {
            OutboxMutation::AddSubIssue {
                parent_remote_id,
                child_remote_id,
                ..
            } => {
                assert_eq!(
                    parent_remote_id, "200",
                    "parent issue addresses the endpoint"
                );
                assert_eq!(
                    child_remote_id, "100",
                    "freshly promoted child is the sub-issue"
                );
            }
            other => panic!("expected AddSubIssue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn promote_skips_relation_to_local_only_neighbor() {
        // The neighbor has no issue yet → not projectable → no enqueue. (It will
        // backfill when the NEIGHBOR promotes, since the edge is reciprocal.)
        let (svc, tasks, _b, outbox, ws, repo_id, _filing_id, _p) = setup_with_outbox().await;
        let mut neighbor = Task::new_draft(ws, Some(repo_id), "neighbor".into()).unwrap();
        tasks
            .save(&neighbor, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        let mut t = Task::new_draft(ws, Some(repo_id), "t".into()).unwrap();
        t.add_relation(RelationKind::BlockedBy, neighbor.id);
        let _ = &mut neighbor;
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        svc.promote(&t.id.to_string()).await.unwrap();
        assert!(
            outbox.all().is_empty(),
            "neighbor not issue-backed ⇒ nothing to backfill"
        );
    }

    #[tokio::test]
    async fn pull_reconciles_inbound_sub_issue_and_dependency() {
        // Remote says #100 has a sub-issue #200 and is blocked_by #300; both map
        // to local tasks. Pull must add child_of #200... actually parent_of, and
        // blocked_by, locally — without enqueuing any outbound mutation.
        let (svc, tasks, _b, outbox, ws, repo_id, filing_id, provider) = setup_with_outbox().await;
        let mut subject = seed_promoted(&tasks, ws, repo_id, filing_id, "100").await;
        // Give it a baseline so pull's drift compare is a clean Noop.
        subject.confirm_synced(SnapshotSource::Pull).ok();
        tasks.save(&subject, SnapshotSource::Pull).await.unwrap();
        let child = seed_promoted(&tasks, ws, repo_id, filing_id, "200").await;
        let blocker = seed_promoted(&tasks, ws, repo_id, filing_id, "300").await;

        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: "issue 100".into(),
            body: String::new(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: vec![],
            labels: vec![],
        });
        provider.set_sub_issues(&[("github.com/o/r", "200")]);
        provider.set_blocked_by(&[("github.com/o/r", "300")]);

        svc.pull(&subject.id.to_string()).await.unwrap();

        let back = tasks.get(subject.id).await.unwrap();
        let kinds: Vec<_> = back.relations.iter().map(|r| (r.kind, r.other)).collect();
        assert!(
            kinds.contains(&(RelationKind::ParentOf, child.id)),
            "sub-issue #200 → parent_of locally: {kinds:?}"
        );
        assert!(
            kinds.contains(&(RelationKind::BlockedBy, blocker.id)),
            "dependency #300 → blocked_by locally: {kinds:?}"
        );
        // Reciprocals mirrored onto the neighbors.
        assert!(
            tasks
                .get(child.id)
                .await
                .unwrap()
                .relations
                .iter()
                .any(|r| r.kind == RelationKind::ChildOf && r.other == subject.id)
        );
        // Inbound reconcile must NOT re-enqueue outbound.
        assert!(
            outbox.all().is_empty(),
            "relations pulled FROM github must not enqueue outbound mutations"
        );
    }

    #[tokio::test]
    async fn pull_drops_local_edge_the_remote_removed() {
        // Local has parent_of #200, but the remote sub-issue list is now empty →
        // the edge must be dropped locally (both ends issue-backed).
        let (svc, tasks, _b, _o, ws, repo_id, filing_id, provider) = setup_with_outbox().await;
        let mut subject = seed_promoted(&tasks, ws, repo_id, filing_id, "100").await;
        let child = seed_promoted(&tasks, ws, repo_id, filing_id, "200").await;
        subject.add_relation(RelationKind::ParentOf, child.id);
        tasks.save(&subject, SnapshotSource::Pull).await.unwrap();
        let mut child = tasks.get(child.id).await.unwrap();
        child.add_relation(RelationKind::ChildOf, subject.id);
        tasks.save(&child, SnapshotSource::Pull).await.unwrap();

        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: "issue 100".into(),
            body: String::new(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: vec![],
            labels: vec![],
        });
        // No sub-issues, no deps on the remote anymore.

        svc.pull(&subject.id.to_string()).await.unwrap();

        let back = tasks.get(subject.id).await.unwrap();
        assert!(
            !back
                .relations
                .iter()
                .any(|r| r.kind == RelationKind::ParentOf),
            "remote dropped the sub-issue ⇒ local parent_of removed: {:?}",
            back.relations
        );
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
        // RemoteRef and persisted, so the promoted task is board-eligible.
        let saved = tasks.get(task.id).await.unwrap();
        assert_eq!(
            saved.remote.unwrap().node_id.as_deref(),
            Some("I_kwDOfake100")
        );
    }

    #[tokio::test]
    async fn promote_records_filing_repo_as_logical_with_no_default() {
        // RFC 0002 D2: no per-task override, no workspace default ⇒ the chain
        // collapses to the logical repo, so promote records filing == logical
        // and files at the same canonical as today.
        let (svc, tasks, bindings, task, provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let saved = tasks.get(task.id).await.unwrap();
        // The chain collapsed to the logical repo, so filing_repo_id must hold that
        // logical instance's ORIGIN id (RFC 0005 §D4 — filing is origin-space), not
        // the instance id and not an arbitrary value.
        let logical_instance = saved.repo_id.expect("promoted task has a logical repo");
        let logical_origin = bindings
            .get(logical_instance)
            .await
            .unwrap()
            .instance
            .origin_id;
        assert_eq!(
            saved.filing_repo_id,
            Some(domain_core::RepoId::from_uuid(logical_origin.as_uuid())),
            "no override/default ⇒ filing resolves to the logical repo's origin"
        );
        // ...and the issue is actually filed at that origin's canonical.
        assert_eq!(
            provider.last_create_canonical.lock().unwrap().as_deref(),
            Some("github.com/o/r")
        );
    }

    #[tokio::test]
    async fn promote_orphan_with_workspace_default_files_in_default_repo() {
        // RFC 0002 D2 step-2 edge case: an orphan task (no logical repo) whose
        // workspace has a filing default resolves to that default — a REAL
        // issue in the default repo, NOT a board draft. The resolved filing
        // repo is recorded and the issue is filed at the default's canonical.
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let provider = Arc::new(FakeProvider::default());

        let mut workspace = Workspace::new(WorkspaceName::new("orphan-ws").unwrap(), None, true);
        let default_origin = RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        bindings.save_origin(&default_origin).await.unwrap();
        let default_instance = RepoInstance::new(
            workspace.id,
            default_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        bindings.save_instance(&default_instance).await.unwrap();
        workspace.filing_repo_id =
            Some(domain_core::RepoId::from_uuid(default_origin.id.as_uuid()));
        workspaces.save(&workspace).await.unwrap();

        // Orphan: no logical repo at all.
        let task = Task::new_draft(workspace.id, None, "orphan".into()).unwrap();
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        let svc = SyncService::new(
            tasks.clone(),
            bindings.clone(),
            workspaces.clone(),
            provider.clone(),
        );
        svc.promote(&task.id.to_string()).await.unwrap();

        let saved = tasks.get(task.id).await.unwrap();
        assert_eq!(saved.repo_id, None, "still an orphan on the logical axis");
        assert_eq!(
            saved.filing_repo_id.map(|r| r.as_uuid()),
            Some(default_origin.id.as_uuid()),
            "filing resolved to the workspace default and was recorded"
        );
        assert_eq!(
            provider.last_create_canonical.lock().unwrap().as_deref(),
            Some("github.com/o/filing"),
            "the issue was filed in the workspace default repo"
        );
    }

    /// D2-chain test fixture (rpl-s7k): a workspace with a filing default
    /// and two bindings (default + logical code), and a saga task that
    /// lives in the default repo with a recorded `remote_id` but no
    /// `filing_repo_id` on the task. The `default_binding_id` lets the
    /// push test assert "promote recorded the default."
    struct SagaFixture {
        svc: SyncService,
        tasks: Arc<InMemoryTaskRepository>,
        provider: Arc<FakeProvider>,
        task: Task,
        default_binding_id: RepoId,
    }

    async fn setup_saga_workspace() -> SagaFixture {
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let provider = Arc::new(FakeProvider::default());

        let mut workspace = Workspace::new(WorkspaceName::new("saga-ws").unwrap(), None, true);
        let default_origin = RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        bindings.save_origin(&default_origin).await.unwrap();
        let default_instance = RepoInstance::new(
            workspace.id,
            default_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        bindings.save_instance(&default_instance).await.unwrap();
        let code_origin = RepoOrigin::new(
            "git@github.com:o/code.git".into(),
            "github.com/o/code".into(),
        )
        .unwrap();
        bindings.save_origin(&code_origin).await.unwrap();
        let code_instance = RepoInstance::new(
            workspace.id,
            code_origin.id,
            "github.com/o/code".into(),
            None,
        )
        .unwrap();
        bindings.save_instance(&code_instance).await.unwrap();
        workspace.filing_repo_id =
            Some(domain_core::RepoId::from_uuid(default_origin.id.as_uuid()));
        workspaces.save(&workspace).await.unwrap();

        // Saga task: `repo_id` is the code repo, `filing_repo_id` is
        // null, the issue itself lives in the default repo. The remote
        // ref is pre-recorded so pull can address it without going
        // through promote.
        let mut task = Task::new_draft(
            workspace.id,
            Some(code_instance.id),
            "saga: add a thing".into(),
        )
        .unwrap();
        task.remote = Some(RemoteRef::new("github", "1409"));
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        let svc = SyncService::new(
            tasks.clone(),
            bindings.clone(),
            workspaces.clone(),
            provider.clone(),
        );
        SagaFixture {
            svc,
            tasks,
            provider,
            task,
            default_binding_id: domain_core::RepoId::from_uuid(default_origin.id.as_uuid()),
        }
    }

    #[tokio::test]
    async fn pull_with_workspace_default_resolves_through_chain() {
        // Pull's `fetch_remote` must hit the workspace default, not the
        // logical code repo, when `filing_repo_id` is unset.
        let SagaFixture {
            svc,
            tasks: _,
            provider,
            task,
            default_binding_id: _,
        } = setup_saga_workspace().await;

        // A noop-shaped fetch — only `remote_id` + `title` need to match
        // local for the drift verdict to noop; the other fields don't
        // enter the mirror. `updated_at` is the project-poller epoch
        // sentinel (it sorts before any real timestamp, so the test
        // isn't clock-dependent).
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "1409".into(),
            node_id: None,
            title: "saga: add a thing".into(),
            body: String::new(),
            closed: false,
            updated_at: Timestamp::from_utc(
                chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            ),
            assignees: Vec::new(),
            labels: Vec::new(),
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(
            provider.last_fetch_canonical.lock().unwrap().as_deref(),
            Some("github.com/o/filing"),
            "pull fetched from the workspace default, not the logical code repo (rpl-s7k)"
        );
        assert_eq!(s.decision, "noop");
    }

    #[tokio::test]
    async fn push_with_workspace_default_targets_the_default_repo() {
        // Push's `update_remote` PATCH must target the workspace default,
        // not the logical code repo. Promote first so the task has a
        // `synced_baseline` to diff against; the D2 chain then records
        // the *default* on the task (step 1 hits).
        let SagaFixture {
            svc,
            tasks,
            provider,
            task,
            default_binding_id,
        } = setup_saga_workspace().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let promoted = tasks.get(task.id).await.unwrap();
        assert_eq!(
            promoted.filing_repo_id,
            Some(default_binding_id),
            "promote recorded the workspace default as the filing repo"
        );

        // Title edit + DirtyLocal so push has a diff to send.
        let mut edited = tasks.get(task.id).await.unwrap();
        edited.title = "saga: add a thing (revised)".into();
        edited.mark_dirty_local().unwrap();
        tasks
            .save(&edited, SnapshotSource::LocalEdit)
            .await
            .unwrap();

        let _s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(
            provider.last_update_canonical.lock().unwrap().as_deref(),
            Some("github.com/o/filing"),
            "push PATCH'd the workspace default, not the logical code repo (rpl-s7k)"
        );
    }

    #[tokio::test]
    async fn promote_requires_repo_binding() {
        let (svc, tasks, task, _) = setup().await;
        // Reuse the seeded workspace (which has no filing default) so D2 step 2
        // misses and resolution reaches the orphan case: no logical repo and no
        // default ⇒ filing stays None ⇒ NoRepo, not a workspace-not-found error.
        let mut t = Task::new_draft(task.workspace_id, None, "rogue".into()).unwrap();
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
    async fn push_sends_only_changed_fields() {
        // Title-only edit PATCHes only `title`, not body/closed/
        // state_reason. Asserting the field-level shape here is
        // the load-bearing test for the helper.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_title("renamed".into()).unwrap();
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        svc.push(&task.id.to_string()).await.unwrap();

        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.remote_id, "100");
        assert_eq!(
            recorded.title.as_deref(),
            Some("renamed"),
            "title in the patch"
        );
        assert_eq!(recorded.body, None, "body NOT in the patch");
        assert_eq!(recorded.closed, None, "closed NOT in the patch");
        assert_eq!(
            recorded.state_reason, None,
            "state_reason NOT in the patch (status unchanged)"
        );
        assert_eq!(
            recorded.assignees, None,
            "assignees NOT in the patch (assignees unchanged)"
        );
    }

    #[tokio::test]
    async fn push_skips_remote_call_when_no_field_changed() {
        // Title-equivalent push: the task is DirtyLocal (so push's
        // "nothing to do" gate doesn't reject it) but no mirrored
        // field actually differs from the baseline. The helper
        // short-circuits to None, so the remote is never PATCHed. The
        // push still confirms synced and the summary records the
        // PushLocal decision (the snapshot axis ran but had nothing
        // to send).
        //
        // Reaching "DirtyLocal + empty diff" cleanly: just
        // `mark_dirty_local()` directly. The task flips Synced →
        // DirtyLocal with no field change, so the diff stays empty.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let s = svc.push(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "push_local");
        assert_eq!(s.new_state, "synced");
        // The remote was never PATCHed.
        assert!(
            provider.last_update.lock().unwrap().is_none(),
            "empty diff ⇒ no remote call, even though the task was dirty"
        );
    }

    /// Positive counterpart of `push_sends_only_changed_fields`: a local
    /// assignee edit MUST ride the PATCH as `Some(&[..])`, and every other
    /// mirrored field must stay `None`. RFC 0003 §7 case 1 (rpl-oa6).
    #[tokio::test]
    async fn push_sends_changed_assignee() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_assignees(vec!["alice".into()]);
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        svc.push(&task.id.to_string()).await.unwrap();

        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.remote_id, "100");
        assert_eq!(
            recorded.assignees.as_deref(),
            Some(&["alice".to_string()][..]),
            "assignees MUST be in the patch when locally changed"
        );
        assert_eq!(recorded.title, None);
        assert_eq!(recorded.body, None);
        assert_eq!(recorded.closed, None);
        assert_eq!(recorded.state_reason, None);
    }

    /// Push-side mirror of `pull_is_noop_when_remote_assignees_are_reordered`:
    /// a local reorder (set-equivalent lists in different order) is NOT a
    /// PATCH. `assignees_equal` (domain-task) is order-insensitive, so
    /// `reconcile_dirty_against_baseline` collapses the would-be dirty task
    /// back to Synced before `push` runs. RFC 0003 §7 case 4 (rpl-oa6).
    #[tokio::test]
    async fn push_does_not_publish_on_pure_assignee_reorder() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Plant a baseline with two assignees in a known order.
        let mut t = tasks.get(task.id).await.unwrap();
        t.assignees = vec!["alice".into(), "bob".into()];
        // Save as Pull so the synced_baseline reflects the new assignees.
        tasks.save(&t, SnapshotSource::Pull).await.unwrap();

        // Reorder locally. The dirty detector sees set-equivalent lists
        // and collapses the task back to Synced.
        let mut t = tasks.get(task.id).await.unwrap();
        t.assignees = vec!["bob".into(), "alice".into()];
        t.mark_dirty_local().unwrap();
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        let _s = svc.push(&task.id.to_string()).await.unwrap();
        assert!(
            provider.last_update.lock().unwrap().is_none(),
            "reorder must not produce a PATCH (assignees_equal is set-equality)"
        );
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(
            after.sync,
            SyncState::Synced,
            "reconcile_dirty_against_baseline collapses back to Synced"
        );
        assert_eq!(
            after.assignees,
            vec!["bob".to_string(), "alice".to_string()]
        );
    }

    /// `set_assignees(vec![])` is an explicit CLEAR, distinct from omitting
    /// the field. The diff helper must emit `Some(vec![])` (which the wire
    /// adapter serializes as `"assignees": []`), not `None`. RFC 0003 §7
    /// case 5 (rpl-oa6).
    #[tokio::test]
    async fn push_with_empty_assignees_sends_explicit_clear() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // First push: seed non-empty assignees and re-baseline.
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_assignees(vec!["alice".into()]);
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();
        svc.push(&task.id.to_string()).await.unwrap();

        // Single-slot recorder: drain before the second push so the
        // assertion below isolates the clear.
        *provider.last_update.lock().unwrap() = None;

        // Clear the assignees list and push. The diff must carry
        // `Some(vec![])` (explicit clear), NOT `None` (omit).
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_assignees(vec![]);
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        svc.push(&task.id.to_string()).await.unwrap();

        let recorded = provider.last_update.lock().unwrap().clone().unwrap();
        assert!(
            recorded.assignees.is_some(),
            "Some(vec![]) is an explicit clear; it must reach the wire as Some, not None"
        );
        assert_eq!(
            recorded.assignees.as_deref(),
            Some(&[][..]),
            "explicit clear: empty slice, not None"
        );
        // Other fields unchanged from the previous push's empty payload.
        assert_eq!(recorded.title, None);
        assert_eq!(recorded.body, None);
        assert_eq!(recorded.closed, None);
        assert_eq!(recorded.state_reason, None);
    }

    /// RFC 0003 D5 (rpl-xq6) synchronous-path parity: the synchronous
    /// `push` uses `confirm_synced_fields` (per-field rebaseline) the
    /// same way the drainer does, so a title-only push rebaselines
    /// ONLY the title on the post-push baseline — body / status /
    /// assignees stay at their pre-push baseline values, and a
    /// subsequent un-pushed edit to any of those fields is still
    /// detected as a diff. (rpl-vvf nails the byte-identical
    /// assertion across both paths; this test confirms the call site
    /// moved and the basic happy-path property holds on the sync
    /// path too — without it, a `rl task claim` post-promote would
    /// still drop the assignee because the synchronous push was
    /// whole-snapshot rebaselining.)
    #[tokio::test]
    async fn push_uses_confirm_synced_fields_per_field_baseline() {
        let (svc, tasks, task, _provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Capture the Promote-time baseline (what push will later
        // re-baseline on top of).
        let pre = tasks.get(task.id).await.unwrap();
        let pre_baseline = pre.synced_baseline.clone().expect("baseline");
        assert_eq!(
            pre_baseline.body, pre.body,
            "baseline body == live body post-promote"
        );
        assert_eq!(pre_baseline.lifecycle, pre.lifecycle);
        assert_eq!(pre_baseline.assignees, pre.assignees);

        // Title-only edit: diff = {title: Some("renamed")},
        // body / status / assignees stay None in the patch.
        let mut t = tasks.get(task.id).await.unwrap();
        t.mark_dirty_local().unwrap();
        t.set_title("renamed".into()).unwrap();
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        svc.push(&task.id.to_string()).await.unwrap();

        let post = tasks.get(task.id).await.unwrap();
        assert_eq!(post.sync, SyncState::Synced);
        let post_baseline = post.synced_baseline.clone().expect("baseline");
        // The transmitted field moved.
        assert_eq!(post_baseline.title, "renamed", "title rebaselined");
        // The un-transmitted fields stayed at the pre-push baseline
        // entry — the per-field merge is the load-bearing property
        // that closes the silent-rebaseline class on the sync path.
        assert_eq!(
            post_baseline.body, pre_baseline.body,
            "body baseline entry must be unchanged after a title-only push"
        );
        assert_eq!(post_baseline.lifecycle, pre_baseline.lifecycle);
        assert_eq!(post_baseline.assignees, pre_baseline.assignees);
        assert_eq!(
            post_baseline.source,
            SnapshotSource::Push,
            "confirm source stamped on the rebaselined snapshot"
        );
        // The title diff is gone; the task is fully clean against
        // the new baseline.
        assert!(post.diff_against_baseline().is_empty());
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

    /// Pins down the inbound-mirror-set contract on the pull copy-back site.
    /// The pull decision copies four fields onto the local task: `title`,
    /// `body`, `assignees`, and the open/closed lifecycle bit (RFC 0004 D1).
    /// This test asserts the `inbound_mirrors_baseline` helper sees
    /// the same set on the same task — so a future PR that changes the helper's
    /// signature but forgets to update the copy-back literal fails here.
    ///
    /// Concretely: build a remote snapshot whose `closed` differs from the
    /// baseline's open lifecycle. The helper returns `false` (drift). The
    /// copy-back then runs and overwrites title/body/assignees AND adopts the
    /// remote's closed bit (Open → Completed). Both halves of the contract are
    /// exercised in one test.
    #[tokio::test]
    async fn pull_copy_back_uses_the_same_inbound_set_as_remote_mirrors_baseline() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // A different title — the helper WILL detect drift. The closed bit is
        // also flipped to closed=true; the inbound path now adopts it.
        let later = Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60));
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: "different title".into(),
            body: "remote body".into(),
            // closed differs from the local Open lifecycle — pull adopts it.
            closed: true,
            updated_at: later,
            assignees: vec!["bob".into()],
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "pull_remote");
        let after = tasks.get(task.id).await.unwrap();
        // Copy-back ran: title/body/assignees reflect the remote, and the
        // lifecycle adopted the remote close (Open → Completed).
        assert_eq!(after.title, "different title");
        assert_eq!(after.body, "remote body");
        assert_eq!(after.assignees, vec!["bob".to_string()]);
        assert_eq!(after.lifecycle, domain_task::Lifecycle::Completed);
        assert!(!after.is_open());
        // Cross-check: the helper's view of the baseline agrees that
        // these three fields are the inbound set. Post-pull the
        // baseline was just re-captured from the live task, so the
        // helper MUST return true — this is the post-condition that
        // makes `remote_mirrors_baseline` a no-op on the next pull
        // (a re-pull of the same remote must produce a `Noop`).
        assert_inbound_set_matches_baseline(&after, "pull");
        assert_eq!(after.sync, SyncState::Synced);
    }

    /// The core bug: an issue closed on GitHub with NO other change
    /// (title/body/assignees identical) must reconcile the local lifecycle to
    /// closed. Previously this returned `noop` and the task stayed open
    /// indefinitely. Re-pulling the same closed snapshot is then a clean `noop`.
    #[tokio::test]
    async fn pull_adopts_remote_close_and_is_idempotent() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let before = tasks.get(task.id).await.unwrap();
        assert!(before.is_open());

        // Only the open/closed bit differs from the local baseline.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: true,
            updated_at: Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60)),
            assignees: before.assignees.clone(),
            labels: vec![],
        });

        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "pull_remote");
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.lifecycle, domain_task::Lifecycle::Completed);
        assert!(!after.is_open());
        assert_eq!(after.sync, SyncState::Synced);

        // Re-pull the same closed state — nothing left to reconcile.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: true,
            updated_at: Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(120)),
            assignees: before.assignees.clone(),
            labels: vec![],
        });
        let s2 = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s2.decision, "noop");
    }

    /// A locally-closed task whose issue was reopened on GitHub reconciles back
    /// to open with the reopened marker (Completed -> Reopened).
    #[tokio::test]
    async fn pull_reopens_a_locally_closed_task() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let before = tasks.get(task.id).await.unwrap();

        // First close it via a pull (sets Completed + a fresh Synced baseline).
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: true,
            updated_at: Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(60)),
            assignees: before.assignees.clone(),
            labels: vec![],
        });
        svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(
            tasks.get(task.id).await.unwrap().lifecycle,
            domain_task::Lifecycle::Completed
        );

        // Now the issue is reopened on GitHub.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now() + chrono::Duration::seconds(120)),
            assignees: before.assignees.clone(),
            labels: vec![],
        });
        let s = svc.pull(&task.id.to_string()).await.unwrap();
        assert_eq!(s.decision, "pull_remote");
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.lifecycle, domain_task::Lifecycle::Reopened);
        assert!(after.is_open());
        assert_eq!(after.sync, SyncState::Synced);
    }

    #[tokio::test]
    async fn refresh_stamps_synced_at_on_success() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        assert!(tasks.get(task.id).await.unwrap().synced_at.is_none());

        let before = tasks.get(task.id).await.unwrap();
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "100".into(),
            node_id: None,
            title: before.title.clone(),
            body: before.body.clone(),
            closed: false,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: before.assignees.clone(),
            labels: vec![],
        });

        let outcome = svc.refresh(&task.id.to_string()).await.unwrap();
        assert_eq!(outcome, RefreshOutcome::Stamped);
        // Observed → freshness stamped; content untouched.
        let after = tasks.get(task.id).await.unwrap();
        assert!(after.synced_at.is_some(), "refresh stamps synced_at");
        // Stamp-source discipline (RFC 0004 §6 tripwire 2): the refresh path
        // must stamp with `SyncedSource::Refresh`, distinct from the poller's
        // `Polled` and the drainer's `Push`.
        assert_eq!(
            tasks.synced_stamps(),
            vec![(task.id, SyncedSource::Refresh)],
            "refresh must stamp with the Refresh source"
        );
        assert_eq!(
            after.title, before.title,
            "refresh must NOT reconcile content"
        );
        assert_eq!(after.sync, before.sync);
    }

    #[tokio::test]
    async fn refresh_propagates_fetch_error_and_does_not_stamp() {
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        // No fetch fixture set → the provider returns an error.
        let _ = &provider;

        // The fetch error PROPAGATES (the caller classifies it); freshness is
        // not stamped on a failed observe.
        assert!(
            svc.refresh(&task.id.to_string()).await.is_err(),
            "a fetch error must propagate for the caller to classify"
        );
        assert!(
            tasks.get(task.id).await.unwrap().synced_at.is_none(),
            "a failed refresh must not stamp synced_at"
        );
    }

    #[tokio::test]
    async fn refresh_skips_a_task_with_no_issue() {
        let (svc, _tasks, task, _provider) = setup().await;
        // Never promoted → no REST issue to observe.
        let outcome = svc.refresh(&task.id.to_string()).await.unwrap();
        assert_eq!(outcome, RefreshOutcome::NotIssueBacked);
    }

    #[tokio::test]
    async fn refresh_propagates_issue_moved() {
        // The CLI relies on `IssueMoved` propagating as a typed error to emit
        // the relink guidance (instead of flattening it into a generic
        // last_refresh_failed annotation). Pin that contract.
        let (svc, tasks, task, provider) = setup().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        provider.set_fetch_moved("github.com/o2/r2", "1506");

        let err = svc.refresh(&task.id.to_string()).await.unwrap_err();
        assert!(
            matches!(err, SyncError::Port(ports::PortError::IssueMoved { .. })),
            "IssueMoved must propagate as a typed error, got {err:?}"
        );
        assert!(
            tasks.get(task.id).await.unwrap().synced_at.is_none(),
            "a moved-issue refresh must not stamp synced_at"
        );
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
    async fn pull_errors_when_filing_binding_is_gone() {
        // rpl-s7k: the rpl-sv2 soft-fall to the logical repo is gone. A
        // dangling `filing_repo_id` (binding deleted, no live binding
        // anywhere on the chain) surfaces as a `NotFound` error so the
        // user runs `rl repo doctor --repair` instead of silently
        // fetching from a fabricated canonical. The fixture below has
        // no workspace default and no live replacement, so the chain
        // has nothing to resolve through.
        let (svc, tasks, bindings, task, _provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // `setup_with_bindings` returns a workspace with no filing
        // default, so the only thing on the D2 chain is the recorded
        // `filing_repo_id` (== `repo_id` for a freshly promoted task).
        // Delete that binding and pull must surface the missing repo
        // rather than soft-fall to a fabricated canonical.
        let original_binding_id = task.repo_id.expect("task has a logical repo");
        bindings.delete(original_binding_id).await.unwrap();

        let err = svc
            .pull(&task.id.to_string())
            .await
            .expect_err("pull with a deleted filing binding must error");
        assert!(
            matches!(err, SyncError::Port(PortError::NotFound(_))),
            "expected Port(NotFound) when the only D2 binding is gone, got {err:?}"
        );

        // Sanity check: the task is unchanged — no Pull / PrePull snapshot
        // was written because the resolution never reached the fetch.
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.sync, SyncState::Synced);
    }

    #[tokio::test]
    async fn pull_falls_through_chain_when_workspace_row_is_deleted() {
        // CodeRabbit #191: a deleted workspace row must not mask valid
        // step-1/step-3 inputs. Pull should resolve through the
        // recorded `filing_repo_id` even when the workspace row is
        // gone, surfacing the *recorded* binding's canonical URL
        // rather than a `NoRepo` or `NotFound("workspace ...")`.
        let tasks = Arc::new(InMemoryTaskRepository::new());
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let provider = Arc::new(FakeProvider::default());

        // Build a workspace + binding, then promote a task on it. The
        // promote records `filing_repo_id` (origin id) and `repo_id` (instance id)
        // on the task. Then delete the *workspace* row (not the
        // binding) and pull must still succeed.
        let workspace = Workspace::new(WorkspaceName::new("del-ws").unwrap(), None, true);
        let origin =
            RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into()).unwrap();
        bindings.save_origin(&origin).await.unwrap();
        let instance =
            RepoInstance::new(workspace.id, origin.id, "github.com/o/r".into(), None).unwrap();
        let binding_id = instance.id;
        bindings.save_instance(&instance).await.unwrap();
        workspaces.save(&workspace).await.unwrap();
        let task = Task::new_draft(workspace.id, Some(binding_id), "t".into()).unwrap();
        tasks.save(&task, SnapshotSource::LocalEdit).await.unwrap();
        let svc = SyncService::new(
            tasks.clone(),
            bindings.clone(),
            workspaces.clone(),
            provider.clone(),
        );
        svc.promote(&task.id.to_string()).await.unwrap();

        // Delete the workspace row. The task and its recorded
        // `filing_repo_id` are untouched.
        workspaces.delete(workspace.id).await.unwrap();

        // Re-read the task: promote updated the in-store task with a
        // `remote_id: "100"` from the FakeProvider.
        let promoted = tasks.get(task.id).await.unwrap();
        let remote_id = promoted
            .remote
            .as_ref()
            .expect("promote must have set a remote")
            .remote_id
            .clone();

        // Seed a noop-shaped fetch against the live binding.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: remote_id.clone(),
            node_id: None,
            title: promoted.title.clone(),
            body: promoted.body.clone(),
            closed: false,
            updated_at: Timestamp::from_utc(
                promoted.updated_at.into_inner() - chrono::Duration::seconds(10),
            ),
            assignees: promoted.assignees.clone(),
            labels: vec![],
        });

        let _s = svc.pull(&task.id.to_string()).await.unwrap();
        // The chain resolved to the recorded `filing_repo_id`'s
        // binding, not a `NoRepo` error. The binding is still live.
        let recorded = provider
            .last_fetch_canonical
            .lock()
            .unwrap()
            .clone()
            .expect("pull must have recorded a fetch canonical");
        assert_eq!(
            recorded, "github.com/o/r",
            "chain must fall through to step 1 (recorded filing_repo_id) when the workspace row is gone"
        );
    }

    #[tokio::test]
    async fn pull_hard_fails_when_neither_filing_nor_logical_resolves() {
        // rpl-s7k: the rpl-sv2 soft-fall is gone, so this is now the
        // *primary* "no resolvable home" test. With no workspace default
        // and both the recorded `filing_repo_id` and `repo_id` pointing
        // at a deleted binding, the D2 chain has nothing to resolve
        // through — pull must propagate the `NotFound` so the user
        // (or the daemon) can see the task has no GitHub home, not
        // silently fetch from a fabricated canonical.
        let (svc, tasks, bindings, task, _provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();

        // Mutate the task so `filing_repo_id` and `repo_id` differ
        // (filing != logical) before deleting both bindings. Without
        // this divergence the second binding doesn't exist and we can't
        // exercise the both-gone branch.
        let mut t = tasks.get(task.id).await.unwrap();
        let second_origin = RepoOrigin::new(
            "git@github.com:o/other.git".into(),
            "github.com/o/other".into(),
        )
        .unwrap();
        bindings.save_origin(&second_origin).await.unwrap();
        let second_instance = RepoInstance::new(
            t.workspace_id,
            second_origin.id,
            "github.com/o/other".into(),
            None,
        )
        .unwrap();
        let second_id = second_instance.id;
        bindings.save_instance(&second_instance).await.unwrap();
        t.repo_id = Some(second_id);
        t.force_set_filing_repo_id(Some(second_id));
        tasks.save(&t, SnapshotSource::LocalEdit).await.unwrap();

        // Delete the (now) only binding.
        bindings.delete(second_id).await.unwrap();

        let err = svc
            .pull(&t.id.to_string())
            .await
            .expect_err("pull with no resolvable binding must error");
        assert!(
            matches!(err, SyncError::Port(PortError::NotFound(_))),
            "expected Port(NotFound) when both bindings are gone, got {err:?}"
        );
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
        let mut origin = RepoOrigin::new(
            format!(
                "git@github.com:{}",
                canonical.trim_start_matches("github.com/")
            ),
            canonical.to_string(),
        )
        .unwrap();
        // In tests the InMemoryRepoBindingRepository enforces prefix uniqueness
        // but does not auto-break ties the way SQLite does. Derive a unique
        // prefix from the full canonical URL's alphabetic chars so collisions
        // with the primary binding ("rpe" from "github.com/o/r") cannot occur.
        let alpha: Vec<char> = canonical
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .collect();
        let mut prefix_chars: Vec<char> = alpha.iter().rev().take(3).cloned().collect();
        prefix_chars.resize(3, 'x');
        let prefix: String = prefix_chars.iter().collect::<String>().to_ascii_lowercase();
        origin.set_prefix(prefix).unwrap();
        bindings.save_origin(&origin).await.unwrap();
        let instance =
            RepoInstance::new(workspace_id, origin.id, canonical.to_string(), None).unwrap();
        bindings.save_instance(&instance).await.unwrap();
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

    /// Mirrors `pull_copy_back_uses_the_same_inbound_set_as_remote_mirrors_baseline`
    /// for the relink path. The relink copy-back overwrites the same three
    /// fields (title, body, assignees) — if a future PR adds a new field to
    /// the `inbound_mirrors_baseline` signature but forgets to update the
    /// relink literal, the `debug_assert!` at the relink call site fires
    /// and the post-relink task's inbound set will no longer match the
    /// helper's view of the baseline (the literal missed a field). This
    /// test exercises the path AND asserts the helper agrees post-relink.
    #[tokio::test]
    async fn relink_copy_back_uses_the_same_inbound_set_as_remote_mirrors_baseline() {
        let (svc, tasks, bindings, task, provider) = setup_with_bindings().await;
        svc.promote(&task.id.to_string()).await.unwrap();
        let workspace_id = task.workspace_id;
        attach_second_binding(&bindings, workspace_id, "github.com/o2/r2").await;

        provider.set_move_target("github.com/o2/r2", "1506");
        // Closed differs from the local Open lifecycle — relink adopts the new
        // remote's open/closed bit along with the rest of the inbound set.
        provider.set_fetch(RemoteTaskSnapshot {
            remote_id: "1506".into(),
            node_id: Some("I_kwDOrelink1506".into()),
            title: "relinked title".into(),
            body: "relinked body".into(),
            closed: true,
            updated_at: Timestamp::from_utc(Utc::now()),
            assignees: vec!["alice".into(), "carol".into()],
            labels: vec![],
        });

        let s = svc
            .link(&task.id.to_string(), "github.com/o2/r2", "1506", true)
            .await
            .unwrap();
        assert_eq!(s.decision, "relinked");
        let after = tasks.get(task.id).await.unwrap();
        assert_eq!(after.title, "relinked title");
        assert_eq!(after.body, "relinked body");
        assert_eq!(
            after.assignees,
            vec!["alice".to_string(), "carol".to_string()]
        );
        // Relink adopted the new remote's closed bit (Open → Completed).
        assert_eq!(after.lifecycle, domain_task::Lifecycle::Completed);
        // Cross-check: helper sees the same inbound set the copy just wrote.
        // Post-relink the baseline was re-captured from the new remote, so the
        // helper MUST return true (live task matches the new baseline).
        assert_inbound_set_matches_baseline(&after, "relink");
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
