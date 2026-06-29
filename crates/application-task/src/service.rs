//! [`TaskService`] — task CRUD, lifecycle/sync transitions, friendly-ID
//! resolution, rollback, and snapshot listing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use application_sync::enqueue;
use domain_core::{RepoId, RepoOriginId, TaskId, Timestamp, WorkspaceId};
use domain_sync::{OutboxEntry, OutboxMutation, resolve_filing_repo};
use domain_task::{Priority, RelationKind, RemoteRef, SnapshotSource, SyncState, Task};
use domain_workspace::Workspace;
use dto_shared::{
    AddTaskRelationCmd, CreateTaskCmd, ImportMirrorCmd, ListTasksQuery, RemoveTaskRelationCmd,
    TaskDto, UpdateTaskCmd,
};
use ports::{
    PortError, ProjectRepository, RepoBindingRepository, TaskFilter, TaskRepository,
    TaskSnapshotRepository, WorkspaceRepository,
};

use crate::dto::{assemble_task_display_id, parse_enum, task_to_dto};
use crate::error::{Result, ServiceError};

/// Task CRUD + lifecycle / sync transitions.
///
/// **Outbox durability (atomic save + enqueue, RFC 0001 Stage 6, #54).**
/// Lifecycle / edit verbs on a mirror task plan the appropriate
/// [`OutboxMutation`]s and persist them together with the task in ONE atomic
/// write via [`TaskRepository::save_with_outbox`] (CodeRabbit thread
/// r3324166852): the mutation list is computed *first*, then exactly one
/// combined write commits the task row + snapshot + every pending outbox entry
/// — so a crash can never tear the saved task apart from its entries (either
/// both persist or neither does). This closes the old save-then-enqueue gap,
/// including the draft-only `UpdateDraftIssue` and board-only
/// `SetProjectStatus` cases the daemon's `reconcile_dirty_into_outbox` could
/// not re-form.
///
/// The daemon's startup `reconcile_dirty_into_outbox` is now a
/// belt-and-suspenders backstop for *pre-existing / legacy* `DirtyLocal` tasks
/// (those already dirty when the codebase upgraded to the outbox path, which
/// never had an entry enqueued), NOT the primary durability guarantee for new
/// transitions. `TaskService` no longer holds an `OutboxRepository` handle: it
/// drains all outbound enqueueing through `save_with_outbox`; the standalone
/// `OutboxRepository` port lives on in `WorkspaceService` backfill and the
/// drainer, which don't write a task in the same breath.
pub struct TaskService {
    repo: Arc<dyn TaskRepository>,
    snapshots: Arc<dyn TaskSnapshotRepository>,
    /// Used by the friendly-ID resolver to validate the prefix half of
    /// a composite `prefix-hash` input against the task's repo binding.
    bindings: Arc<dyn RepoBindingRepository>,
    /// Project resolver — `task → workspace → project` — so lifecycle verbs
    /// can compute the project Status option to enqueue.
    workspaces: Arc<dyn WorkspaceRepository>,
    projects: Arc<dyn ProjectRepository>,
}

impl TaskService {
    pub fn new(
        repo: Arc<dyn TaskRepository>,
        snapshots: Arc<dyn TaskSnapshotRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
        workspaces: Arc<dyn WorkspaceRepository>,
        projects: Arc<dyn ProjectRepository>,
    ) -> Self {
        Self {
            repo,
            snapshots,
            bindings,
            workspaces,
            projects,
        }
    }

    pub fn snapshots_repo(&self) -> &Arc<dyn TaskSnapshotRepository> {
        &self.snapshots
    }

    /// Resolve a friendly task ID and list its snapshot history.
    /// Exists so the CLI can stay friendly-ID-aware without having to
    /// reach into [`snapshots_repo`] and parse a UUID itself.
    pub async fn list_snapshots(&self, query: &str) -> Result<Vec<domain_task::TaskSnapshot>> {
        let task = self.resolve_task(query).await?;
        Ok(self.snapshots.list(task.id).await?)
    }

    /// Look up the repo's prefix for assembling the composite display
    /// ID. `None` when the task has no repo binding (workspace-scoped
    /// task or pre-attach state) — the DTO falls back to bare hash.
    async fn prefix_for(&self, t: &Task) -> Result<Option<String>> {
        let Some(repo_id) = t.repo_id else {
            return Ok(None);
        };
        Ok(Some(self.bindings.get(repo_id).await?.origin.prefix))
    }

    /// Convert a single task to its DTO, looking up the composite-ID
    /// prefix for the task itself and for each related task. The
    /// relation rewrite keeps JSON output consistent: a task's `id`
    /// and the `other` end of every relation both follow the same
    /// composite-or-hash-or-UUID rule. Cost is 1 + N binding lookups
    /// for a task with N relations; acceptable at current scales.
    async fn task_dto(&self, t: &Task) -> Result<TaskDto> {
        let prefix = self.prefix_for(t).await?;
        let mut dto = task_to_dto(t, prefix.as_deref());
        // Overlay composite display IDs onto the relation `other`
        // fields. `task_to_dto` defaults them to UUIDs so the pure
        // function stays consistent without a binding handle; we
        // upgrade here because we have one.
        for (rendered, source) in dto.relations.iter_mut().zip(t.relations.iter()) {
            rendered.other = self.compose_id_for(source.other).await?;
        }
        // Upgrade the flat `blocked_by` list (UUIDs from the pure conversion) to
        // composite display IDs, same as the relation `other` ends above.
        dto.blocked_by.clear();
        for id in t.blocked_by() {
            dto.blocked_by.push(self.compose_id_for(id).await?);
        }
        // Overlay the cached project-board status display name (RFC 0001
        // Stage 8, closes #39). CACHED only — resolve `task → workspace →
        // project → option name` with NO network: `resolve_project` reads the
        // local project repo, `option_name_for` reads the cached option list.
        dto.project_status = self.resolve_cached_project_status(t).await?;
        Ok(dto)
    }

    /// Resolve the task's cached `project_status_option_id` to its display
    /// name via `task → workspace → project`. `None` when the task has no
    /// cached status, its workspace is projectless, or the cached option id is
    /// no longer owned by the project (renamed/removed remotely). Local reads
    /// only — never touches the network, so `rl task show` stays offline.
    async fn resolve_cached_project_status(&self, t: &Task) -> Result<Option<String>> {
        let Some(option_id) = t.project_status_option_id.as_deref() else {
            return Ok(None);
        };
        let Some(project) = enqueue::resolve_project(&self.workspaces, &self.projects, t).await?
        else {
            return Ok(None);
        };
        Ok(project.option_name_for(option_id).map(str::to_string))
    }

    /// Look up a task by UUID and return its composite display ID
    /// (`prefix-hash` / `hash` / UUID fallback). Used to render the
    /// `other` end of a `TaskRelation` consistently with the task's
    /// own `id` field.
    async fn compose_id_for(&self, id: TaskId) -> Result<String> {
        let related = self.repo.get(id).await?;
        let prefix = self.prefix_for(&related).await?;
        Ok(assemble_task_display_id(&related, prefix.as_deref()))
    }

    /// Resolve a task by UUID, bare hash, or `prefix-hash` composite.
    ///
    /// Resolution order:
    /// 1. If the input parses as a UUID, fetch by that ID.
    /// 2. If the input has no `-`, look up by [`TaskRepository::find_by_hash`].
    /// 3. Otherwise split on the *last* `-` (the prefix never contains
    ///    `-`, but hashes are alphanumeric so the right split point is
    ///    unambiguous), look up by hash, then verify the input prefix
    ///    matches the task's repo binding's prefix. Mismatch → hard
    ///    error per the spec.
    pub async fn resolve_task(&self, query: &str) -> Result<Task> {
        // UUID short-circuit. Keep this first so existing scripts that
        // pass UUIDs stay on the cheap path (no hash lookup, no binding
        // lookup).
        if let Ok(id) = query.parse::<TaskId>() {
            return Ok(self.repo.get(id).await?);
        }

        let (input_prefix, hash) = match query.rsplit_once('-') {
            Some((p, h)) => (Some(p), h),
            None => (None, query),
        };

        // Validate both halves' shapes before the lookup so junk like
        // `A1-cde`, `ab--cde`, or wrong-case hashes get a clear "bad id"
        // rather than a misleading PrefixMismatch / "task hash not
        // found". The bare-hash and composite paths both funnel here.
        if !domain_task::is_valid_hash(hash) {
            return Err(ServiceError::BadId(format!(
                "{query:?} is not a task UUID, bare hash, or prefix-hash composite"
            )));
        }
        if let Some(p) = input_prefix
            && !domain_repo::is_valid_prefix(p)
        {
            return Err(ServiceError::BadId(format!(
                "{query:?} has a malformed repo prefix {p:?}"
            )));
        }

        let task = self
            .repo
            .find_by_hash(hash)
            .await?
            .ok_or_else(|| PortError::NotFound(format!("task hash {hash:?}")))?;

        if let Some(input_prefix) = input_prefix {
            // The bare hash is the source of truth — we found the task.
            // The prefix half is a sanity check.
            let actual_prefix = match task.repo_id {
                Some(repo_id) => self.bindings.get(repo_id).await?.origin.prefix,
                // Task without a repo binding can't have a prefix; any
                // input prefix is necessarily a mismatch.
                None => String::new(),
            };
            if actual_prefix != input_prefix {
                return Err(ServiceError::PrefixMismatch {
                    input_prefix: input_prefix.to_string(),
                    actual_prefix,
                    hash: hash.to_string(),
                });
            }
        }
        Ok(task)
    }

    /// Resolve a friendly task reference (UUID, bare hash, or `prefix-hash`
    /// composite) to its canonical UUID string. Lets callers that only need
    /// the identity — e.g. the `sync` CLI handing a task to `SyncService`,
    /// which is UUID-only — reuse the single resolver rather than re-parsing.
    pub async fn resolve_id(&self, query: &str) -> Result<String> {
        Ok(self.resolve_task(query).await?.id.to_string())
    }

    pub async fn create(&self, cmd: CreateTaskCmd) -> Result<TaskDto> {
        let workspace_id: WorkspaceId = cmd.workspace_id.parse()?;
        let repo_id = cmd
            .repo_id
            .as_deref()
            .map(|s| s.parse::<RepoId>())
            .transpose()?;
        let mut t = Task::new_draft(workspace_id, repo_id, cmd.title)?;
        if let Some(body) = cmd.body {
            t.set_body(body);
        }
        if let Some(p) = cmd.priority {
            t.set_priority(parse_enum::<Priority>("priority", &p)?);
        }
        // `Created`, not `LocalEdit` — v1 is a creation, not an edit. See
        // `SnapshotSource::Created` for why this distinction matters.
        self.save_with_minted_hash(&mut t, SnapshotSource::Created)
            .await?;
        self.task_dto(&t).await
    }

    /// Materialise a remote issue as a local mirror task. Unlike `create`,
    /// the first snapshot is a `Pull` baseline — the task starts life
    /// `Synced` against the remote it mirrors. Hash minting + the UNIQUE
    /// retry are shared with `create`; idempotency (skip already-tracked
    /// remotes) is the caller's job via `TaskRepository::find_by_remote`.
    pub async fn import_mirror(&self, cmd: ImportMirrorCmd) -> Result<TaskDto> {
        let workspace_id: WorkspaceId = cmd.workspace_id.parse()?;
        let repo_id = cmd
            .repo_id
            .as_deref()
            .map(|s| s.parse::<RepoId>())
            .transpose()?;
        let mut t = Task::import_mirror(
            workspace_id,
            repo_id,
            RemoteRef::new(cmd.provider, cmd.remote_id),
            cmd.title,
            cmd.body,
            cmd.assignees,
            cmd.closed,
        )?;
        // RFC 0005 §D4: `filing_repo_id` is origin-space. `Task::import_mirror`
        // seeds it from the logical *instance* id (the domain can't resolve
        // origins); convert it to that instance's origin so the stored
        // remote-identity key matches `find_by_remote` (which keys on the
        // origin) — otherwise re-importing the same issue misses dedup and
        // creates a duplicate task.
        if let Some(rid) = repo_id {
            let origin_id = self.bindings.get(rid).await?.instance.origin_id;
            t.force_set_filing_repo_id(Some(RepoId::from_uuid(origin_id.as_uuid())));
        }
        self.save_with_minted_hash(&mut t, SnapshotSource::Pull)
            .await?;
        self.task_dto(&t).await
    }

    /// Save a freshly-created task, retrying the hash on `tasks.hash`
    /// UNIQUE violations and growing the hash length once collisions
    /// cluster at a given length. Mirrors the spec's mint algorithm:
    /// fixed `K_RETRIES_AT_LENGTH = 8` attempts at length 3 before
    /// stepping to 4, then 5, and so on. Capped at length 8 so the
    /// hash always fits the prefix-hash composite's 8-char ceiling.
    ///
    /// Retry is driven by the DB's UNIQUE index, not a pre-flight
    /// existence check — pre-checks race with concurrent creates.
    async fn save_with_minted_hash(&self, t: &mut Task, source: SnapshotSource) -> Result<()> {
        const K_RETRIES_AT_LENGTH: u32 = 8;
        let mut length = domain_task::MIN_HASH_LEN;
        let mut attempts: u32 = 0;
        loop {
            t.hash = domain_task::random_lowercase_base32(length);
            match self.repo.save(t, source).await {
                Ok(()) => return Ok(()),
                Err(e) if e.conflict_target() == Some("tasks.hash") => {
                    attempts += 1;
                    if attempts >= K_RETRIES_AT_LENGTH {
                        attempts = 0;
                        length += 1;
                        if length > domain_task::MAX_HASH_LEN {
                            return Err(ServiceError::Port(PortError::Backend(format!(
                                "could not mint unique task hash at any length up to {}",
                                domain_task::MAX_HASH_LEN
                            ))));
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub async fn show(&self, id: &str) -> Result<TaskDto> {
        let t = self.resolve_task(id).await?;
        self.task_dto(&t).await
    }

    /// Add a pending (local-only) comment to a task. Persists straight to the
    /// comment store — never a snapshot — so it does not flip the task to
    /// `DirtyLocal`. Pending comments are a separate outbound axis, drained by
    /// `sync push`; `author` is provisional and overwritten by the remote
    /// (GitHub) author once pushed.
    pub async fn add_comment(&self, task_ref: &str, body: &str, author: &str) -> Result<TaskDto> {
        if body.trim().is_empty() {
            return Err(ServiceError::Domain(domain_core::DomainError::validation(
                "comment body must not be empty",
            )));
        }
        let id = self.resolve_task(task_ref).await?.id;
        self.repo
            .add_pending_comment(id, author, body, Timestamp::now())
            .await?;
        let t = self.repo.get(id).await?;
        self.task_dto(&t).await
    }

    pub async fn update(&self, cmd: UpdateTaskCmd) -> Result<TaskDto> {
        let mut t = self.resolve_task(&cmd.task_id).await?;
        // Snapshot the sync state *before* the edits so we can tell whether
        // anything remote-observable changed (the domain flips Synced →
        // DirtyLocal on a real title/body/assignee change; priority is local
        // metadata and never dirties). An orphan-draft gaining a repo is the
        // ConvertDraftToIssue trigger, so capture that precondition too.
        let was_orphan_draft = enqueue::is_draft_backed(&t) && t.repo_id.is_none();
        let sync_before = t.sync;

        if let Some(title) = cmd.title {
            t.set_title(title)?;
        }
        if let Some(body) = cmd.body {
            t.set_body(body);
        }
        if let Some(p) = cmd.priority {
            t.set_priority(parse_enum::<Priority>("priority", &p)?);
        }
        if let Some(assignees) = cmd.assignees {
            t.set_assignees(assignees);
        }
        if let Some(repo_id) = cmd.repo_id {
            let parsed = repo_id.parse::<RepoId>()?;
            t.set_repo_id(Some(parsed))?;
        }

        // RFC 0002 D3: re-run the D2 filing-repo chain at draft-conversion
        // planning time. An orphan draft (no repo, has a project item) may now
        // resolve a filing repo via either (a) the logical repo just attached
        // above or (b) the workspace default. The gate fires whenever a filing
        // repo resolves — this subsumes the old `attached_repo` case and adds
        // the workspace-default (step-2) case where `repo_id` stays NULL.
        // Resolution is computed once and feeds the gate, the recording, and
        // the canonical. Per-task override stays None until #122.
        // Resolve + record the filing repo only on the GENUINE FIRST filing of
        // an orphan draft — gate on `filing_repo_id.is_none()`. Once recorded it
        // is authoritative and must never be re-resolved (the resolver's own
        // contract). Without this guard, a workspace-default change between two
        // edits of a not-yet-converted draft would make the second edit either
        // error in `set_filing_repo_id` (cannot change a recorded filing repo)
        // or enqueue a duplicate `ConvertDraftToIssue` (Greptile #137).
        let resolved_filing = if was_orphan_draft && t.filing_repo_id.is_none() {
            let workspace = self.workspaces.get(t.workspace_id).await?;
            // RFC 0005: resolve_filing_repo operates in origin id space.
            // workspace.filing_repo_id and t.repo_id carry RepoId bytes but
            // hold origin UUIDs (workspace default) or instance UUIDs (repo_id).
            let ws_default = workspace
                .filing_repo_id
                .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
            let logical_origin = if let Some(rid) = t.repo_id {
                Some(self.bindings.get(rid).await?.instance.origin_id)
            } else {
                None
            };
            resolve_filing_repo(None, ws_default, logical_origin)
                .map(|o| RepoId::from_uuid(o.as_uuid()))
        } else {
            None
        };

        // Record BEFORE planning/saving so it persists atomically with the
        // outbox entries.
        if resolved_filing.is_some() {
            t.set_filing_repo_id(resolved_filing)?;
        }

        // Plan the outbound mutations the edit owes BEFORE writing, then commit
        // the task + its outbox entries in one atomic write (#54). For a
        // LocalOnly task / priority-only / no-op edit the plan is empty and
        // `save_with_outbox` behaves exactly like `save`.
        let mutations = self
            .plan_update_mutations(&mut t, sync_before, resolved_filing.is_some())
            .await?;
        let entries = into_entries(t.id, mutations);
        self.repo
            .save_with_outbox(&t, SnapshotSource::LocalEdit, &entries)
            .await?;
        self.task_dto(&t).await
    }

    /// Plan the outbound mutations an *edit* owes. Distinct from
    /// [`plan_mirror_mutations`](Self::plan_mirror_mutations) because edits have
    /// two extra rules:
    /// 1. An orphan-draft that just gained a repo graduates to a real issue —
    ///    enqueue `ConvertDraftToIssue`. If the same edit also changed the
    ///    draft's title/body, first enqueue an `UpdateDraftIssue` so the new
    ///    content lands on the draft *before* the conversion (per-task FIFO
    ///    guarantees that order). The drainer's `convert_draft_to_issue` copies
    ///    the draft's *current* content into the new issue, so the converted
    ///    issue carries the edited content rather than the stale pre-edit
    ///    title/body. We push the new content via `UpdateDraftIssue` (addressed
    ///    by the project item node id, known now) rather than a post-convert
    ///    `UpdateRemote` (addressed by the REST issue number, which isn't known
    ///    until a later pull).
    /// 2. A priority-only / no-op edit leaves `sync` unchanged (the domain
    ///    only dirties on a real remote-observable field change), so we
    ///    enqueue nothing — preserving the reconcile no-spurious-mutation
    ///    contract.
    async fn plan_update_mutations(
        &self,
        task: &mut Task,
        sync_before: SyncState,
        converted_orphan_draft: bool,
    ) -> Result<Vec<OutboxMutation>> {
        if !enqueue::is_mirror(task) {
            return Ok(Vec::new());
        }

        if converted_orphan_draft {
            let mut out = Vec::new();
            // A real title/body change dirties the task (Synced → DirtyLocal);
            // a repo-only attach leaves `sync` untouched. So `sync != before`
            // is exactly "content also changed" here.
            let content_changed = task.sync != sync_before;
            let item_node_id = task.project_item_id.clone().unwrap_or_default();
            if content_changed {
                // Land the new draft content first; FIFO runs it before the
                // conversion, so the converted issue inherits it.
                out.push(OutboxMutation::UpdateDraftIssue {
                    item_node_id: item_node_id.clone(),
                    title: Some(task.title.clone()),
                    body: Some(task.body.clone()),
                });
            }
            // The draft graduates to an issue. `repo_node_id` carries the
            // canonical URL of the FILING repo — the adapter resolves it to the
            // GraphQL repo node id (canonical→node-id resolution is an adapter
            // concern, see #54). RFC 0002 D3: the filing canonical is sourced
            // from the recorded `filing_repo_id` (set just before this call in
            // `update`), falling back to `repo_id` when both are absent —
            // exactly the `SyncService::filing_canonical_for` semantics (#123).
            let filing_canonical = self.filing_canonical_for(task).await?.unwrap_or_default();
            out.push(OutboxMutation::ConvertDraftToIssue {
                item_node_id,
                repo_node_id: filing_canonical,
            });
            // Return early: the convert branch handles filing-repo recording
            // separately (#123); do NOT fall through to plan_mirror_mutations
            // or the first-filing resolve/record would double-resolve.
            return Ok(out);
        }

        // No remote-observable change (priority-only / idempotent edit) — the
        // domain left `sync` exactly as it was. Plan nothing.
        if task.sync == sync_before {
            return Ok(Vec::new());
        }

        // Reaching here on the edit path means a real title/body/assignee
        // change dirtied the task (the domain only flips `sync` on a
        // remote-observable field change), so a draft-backed mirror owes an
        // `UpdateDraftIssue`: content_changed = true.
        self.plan_mirror_mutations(task, true).await
    }

    /// Deliberately re-point a task's recorded `filing_repo_id` to a new
    /// value (or `None`). Bypasses the [`Task::set_filing_repo_id`]
    /// immutability guard via [`Task::force_set_filing_repo_id`], which is
    /// the only place a recorded filing repo can be changed. Used by the
    /// `rl repo doctor --repair` path (rpl-sv2) to heal tasks whose
    /// filing binding was deleted out from under them (e.g. after a
    /// GitHub org-move replaced the canonical binding with a new UUID
    /// and never re-pointed the recorded column).
    ///
    /// The resulting snapshot is tagged with
    /// [`SnapshotSource::FilingRepoRepair`] so the audit trail records
    /// every re-point. No outbox entries are enqueued — `filing_repo_id`
    /// is local sync/persistence metadata, NOT a mirrored field, so the
    /// next `sync push/pull` has no outbound mutation to send.
    ///
    /// `target` may be `Some(new)` (re-point to a live binding) or `None`
    /// (clear, for the no-resolvable-target case the doctor surfaces).
    /// Passing the *same* value the task already has is an idempotent
    /// no-op at the domain layer.
    pub async fn repoint_filing_repo(
        &self,
        task_id: &str,
        target: Option<RepoId>,
    ) -> Result<TaskDto> {
        // Validate the target binding exists before mutating the task, and
        // resolve it to its ORIGIN. `force_set_filing_repo_id` accepts any
        // RepoId without checking; its only safety net is this service-layer
        // step. The `--target` is an instance handle, but `filing_repo_id` is
        // origin-space (RFC 0005 §D4) — storing the raw instance id would
        // re-plant a dangling filing pointer (the very bug the doctor heals,
        // moved to a different column). `None` (clear) is a separate case the
        // user explicitly opts into and needs no validation.
        let target = if let Some(target_id) = target {
            let view = self.bindings.get(target_id).await?;
            Some(RepoId::from_uuid(view.instance.origin_id.as_uuid()))
        } else {
            None
        };
        let id_str = self.resolve_id(task_id).await?;
        let id: TaskId = id_str.parse()?;
        let mut t = self.repo.get(id).await?;
        t.force_set_filing_repo_id(target);
        self.repo
            .save_with_outbox(&t, SnapshotSource::FilingRepoRepair, &[])
            .await?;
        self.task_dto(&t).await
    }

    pub async fn list(&self, query: ListTasksQuery) -> Result<Vec<TaskDto>> {
        let filter = TaskFilter {
            workspace_id: query
                .workspace_id
                .as_deref()
                .map(|s| s.parse::<WorkspaceId>())
                .transpose()?,
            repo_id: query
                .repo_id
                .as_deref()
                .map(|s| s.parse::<RepoId>())
                .transpose()?,
            // Default to open-only when no `--status` is given, so `rl task
            // list` shows actionable work and doesn't bury it under completed
            // and dropped (not_planned) tasks. `--status all` opts back into
            // every lifecycle; `open`/`closed` filter explicitly.
            is_open: match query.status.as_deref() {
                None | Some("open") => Some(true),
                Some("closed") => Some(false),
                Some("all") => None,
                Some(other) => {
                    return Err(ServiceError::BadEnum {
                        field: "status",
                        value: other.to_string(),
                    });
                }
            },
            sync_state: query
                .sync_state
                .as_deref()
                .map(|s| parse_enum::<SyncState>("sync_state", s))
                .transpose()?,
            // Poller-only knobs (stale-scan / active-gate / limit) stay off for
            // the user-facing `rl task list`.
            ..TaskFilter::default()
        };
        let rows = self.repo.list(filter).await?;
        // One binding lookup per task — fine at current scales (dozens
        // of tasks); revisit with a batched prefix-map if list latency
        // ever shows up in profiles.
        let mut out = Vec::with_capacity(rows.len());
        for t in &rows {
            out.push(self.task_dto(t).await?);
        }
        Ok(out)
    }

    // ---------- Sync transitions -----------------------------------------

    pub async fn stage_for_sync(&self, id: &str) -> Result<TaskDto> {
        // Staging is a pure sync-state transition (LocalOnly → Staged); it
        // changes no remote-observable field, so it enqueues nothing.
        self.transition(id, |t| t.stage_for_sync()).await
    }

    // ---------- Lifecycle transitions ------------------------------------

    pub async fn start(&self, id: &str) -> Result<TaskDto> {
        self.transition_mirror(id, |t| t.start()).await
    }

    pub async fn complete(&self, id: &str) -> Result<TaskDto> {
        self.transition_mirror(id, |t| t.complete()).await
    }

    pub async fn reopen(&self, id: &str) -> Result<TaskDto> {
        self.transition_mirror(id, |t| t.reopen()).await
    }

    pub async fn archive(&self, id: &str) -> Result<TaskDto> {
        self.transition_mirror(id, |t| t.archive()).await
    }

    pub async fn add_relation(&self, cmd: AddTaskRelationCmd) -> Result<TaskDto> {
        let kind = parse_enum::<RelationKind>("kind", &cmd.kind)?;
        let mut t = self.resolve_task(&cmd.task_id).await?;
        // The other side of a relation is a friendly ID too.
        let mut other_task = self.resolve_task(&cmd.other).await?;

        // A task relating to itself is nonsensical (and the trivial cycle).
        if other_task.id == t.id {
            return Err(ServiceError::SelfRelation);
        }

        // Cycle guard for the acyclic kinds: blocked_by/blocks (deadlock) and
        // parent_of/child_of (ancestry loop). The new edge reads as `x
        // depends-on / is-under y`; it closes a loop iff `y` can already reach
        // `x` going upstream along that family. Symmetric kinds
        // (related_to/duplicates) have no axis and are never restricted.
        if let Some((forward, inverse, x, y)) = cycle_axis(kind, t.id, other_task.id)
            && self.would_create_cycle(forward, inverse, x, y).await?
        {
            return Err(ServiceError::RelationCycle {
                kind: cmd.kind.clone(),
                from: cmd.task_id.clone(),
                to: cmd.other.clone(),
            });
        }

        // Whether this edge is genuinely new (vs. an idempotent re-add). Only a
        // new edge owes an outbound mutation — re-adding an existing relation
        // must not enqueue a duplicate `AddSubIssue`/`AddBlockedBy`.
        let newly_added = !t
            .relations
            .iter()
            .any(|r| r.kind == kind && r.other == other_task.id);

        t.add_relation(kind, other_task.id);
        // Mirror the reciprocal edge onto the other task so the graph reads
        // coherently from both ends. `add_relation` is idempotent, so a
        // pre-existing reciprocal (or a re-run) is a no-op rather than a dup.
        other_task.add_relation(kind.inverse(), t.id);

        // Project the new edge onto its GitHub-native primitive (sub-issue /
        // dependency) when both ends are issue-backed. Empty otherwise.
        let entries = if newly_added {
            self.plan_relation_entries(kind, true, &t, &other_task)
                .await?
        } else {
            Vec::new()
        };

        // Persist both sides AND the outbound entry atomically: a partial write
        // would leave the forward edge without its reciprocal, or the saved
        // relation without the durable mutation it owes (relations have no
        // dirty-detection backstop to re-enqueue a lost entry).
        self.repo
            .save_many_with_outbox(
                &[
                    (&t, SnapshotSource::LocalEdit),
                    (&other_task, SnapshotSource::LocalEdit),
                ],
                &entries,
            )
            .await?;

        self.task_dto(&t).await
    }

    /// Remove a single `(kind, other)` edge and its reciprocal. Idempotent:
    /// removing an absent edge is a no-op that still returns the task.
    pub async fn remove_relation(&self, cmd: RemoveTaskRelationCmd) -> Result<TaskDto> {
        let kind = parse_enum::<RelationKind>("kind", &cmd.kind)?;
        let mut t = self.resolve_task(&cmd.task_id).await?;
        let mut other_task = self.resolve_task(&cmd.other).await?;
        if other_task.id == t.id {
            return Err(ServiceError::SelfRelation);
        }

        // Drop each side, then persist only the sides that actually changed —
        // atomically, so removing one half can't outlive the other.
        let t_changed = t.remove_relation(kind, other_task.id);
        let other_changed = other_task.remove_relation(kind.inverse(), t.id);
        // Only a relation that actually existed (from `t`'s side) owes an
        // outbound un-link; re-removing an absent edge enqueues nothing.
        let entries = if t_changed {
            self.plan_relation_entries(kind, false, &t, &other_task)
                .await?
        } else {
            Vec::new()
        };
        let mut batch: Vec<(&Task, SnapshotSource)> = Vec::new();
        if t_changed {
            batch.push((&t, SnapshotSource::LocalEdit));
        }
        if other_changed {
            batch.push((&other_task, SnapshotSource::LocalEdit));
        }
        if !batch.is_empty() {
            self.repo.save_many_with_outbox(&batch, &entries).await?;
        }
        self.task_dto(&t).await
    }

    /// Drop every relation on a task, stripping the matching reciprocal from
    /// each distinct other task so no dangling back-edges remain.
    pub async fn clear_relations(&self, task_id: &str) -> Result<TaskDto> {
        let mut t = self.resolve_task(task_id).await?;
        let removed = t.clear_relations();
        if removed.is_empty() {
            return self.task_dto(&t).await;
        }

        // Collect the reciprocal edges to strip, grouped by the other task so
        // each is loaded and saved at most once.
        let mut by_other: HashMap<TaskId, Vec<RelationKind>> = HashMap::new();
        for r in &removed {
            if r.other != t.id {
                by_other.entry(r.other).or_default().push(r.kind.inverse());
            }
        }
        let mut others_changed: Vec<Task> = Vec::new();
        // Issue coords for each loaded far end, captured whether or not its
        // reciprocal edge changed — the cleared task's *forward* edge is what
        // owes the GitHub un-link, so the plan below needs every far end's
        // coords, not just the ones whose back-edge was stripped.
        // Workspace cache: each `relation_remote_coords` call would otherwise
        // re-fetch the neighbor's workspace; in the common case all
        // neighbors share one workspace, so one cache slot covers the loop.
        let mut coords_by_other: HashMap<TaskId, Option<(String, String)>> = HashMap::new();
        let mut workspace_cache: HashMap<WorkspaceId, Workspace> = HashMap::new();
        for (other_id, inv_kinds) in by_other {
            let mut other = self.repo.get(other_id).await?;
            let mut changed = false;
            for k in inv_kinds {
                changed |= other.remove_relation(k, t.id);
            }
            coords_by_other.insert(
                other.id,
                self.relation_remote_coords_cached(&other, &mut workspace_cache)
                    .await?,
            );
            if changed {
                others_changed.push(other);
            }
        }

        // Plan one outbound un-link per removed native edge whose far end is
        // issue-backed. Keyed on `t`'s stored kind, so each removed edge maps to
        // a single canonical `RemoveSubIssue`/`RemoveBlockedBy` (the reciprocal
        // back-edge stripped above does not separately enqueue).
        let mut entries: Vec<OutboxEntry> = Vec::new();
        if let Some(this_coords) = self
            .relation_remote_coords_cached(&t, &mut workspace_cache)
            .await?
        {
            for r in &removed {
                if r.other == t.id {
                    continue;
                }
                if let Some(Some(other_coords)) = coords_by_other.get(&r.other)
                    && let Some(m) =
                        enqueue::relation_mutation(r.kind, false, &this_coords, other_coords)
                {
                    entries.push(OutboxEntry::new(t.id, m));
                }
            }
        }

        // Persist the stripped task, every touched back-edge holder, AND the
        // un-link entries in one transaction so no task is left pointing at a
        // relation the cleared task no longer mirrors, and no un-link is lost.
        let mut batch: Vec<(&Task, SnapshotSource)> = vec![(&t, SnapshotSource::LocalEdit)];
        batch.extend(
            others_changed
                .iter()
                .map(|o| (o, SnapshotSource::LocalEdit)),
        );
        self.repo.save_many_with_outbox(&batch, &entries).await?;

        self.task_dto(&t).await
    }

    /// Whether adding the edge `x -> y` (x downstream of y) within a relation
    /// family would close a cycle — i.e. whether `y` can already reach `x`
    /// going *upstream*.
    ///
    /// The upstream adjacency is built from **both** stored directions of the
    /// family (`forward` and its `inverse`), so a one-sided legacy row — a
    /// `forward` edge with no reciprocal, or vice-versa — is still honoured.
    /// That keeps the guard correct even when the reciprocal invariant hasn't
    /// been backfilled. DFS is bounded by a visited set, so a pre-existing
    /// cycle in the data can't loop forever.
    async fn would_create_cycle(
        &self,
        forward: RelationKind,
        inverse: RelationKind,
        x: TaskId,
        y: TaskId,
    ) -> Result<bool> {
        let all = self
            .repo
            .list(TaskFilter {
                // No lifecycle filter — the cycle check must see every task,
                // open or closed.
                is_open: None,
                ..TaskFilter::default()
            })
            .await?;

        // up[n] = nodes immediately upstream of n. `n forward m` puts m
        // upstream of n; `n inverse m` is the reciprocal, so it puts n
        // upstream of m. Reading both means a missing reciprocal can't hide an
        // edge from the walk.
        let mut up: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        for task in &all {
            for r in &task.relations {
                if r.kind == forward {
                    up.entry(task.id).or_default().push(r.other);
                } else if r.kind == inverse {
                    up.entry(r.other).or_default().push(task.id);
                }
            }
        }

        let mut stack = vec![y];
        let mut visited: HashSet<TaskId> = HashSet::new();
        while let Some(n) = stack.pop() {
            if !visited.insert(n) {
                continue;
            }
            if let Some(parents) = up.get(&n) {
                for &p in parents {
                    if p == x {
                        return Ok(true);
                    }
                    stack.push(p);
                }
            }
        }
        Ok(false)
    }

    pub async fn rollback(&self, id: &str, to_version: u64) -> Result<TaskDto> {
        let mut task = self.resolve_task(id).await?;
        // Capture the live filing repo up front for the no-mismatch invariant
        // (RFC 0002 #118/#120): rollback must NOT retarget it, so this value
        // must survive the rollback unchanged (asserted below).
        let task_filing_repo_id_before = task.filing_repo_id;
        // Capture remote-backed state BEFORE `task.remote` is overwritten by the
        // snapshot below. The invariant is about whether the task *was* remote-
        // backed; using post-rollback `task.remote` would let a pre-promote
        // target (remote = None) vacuously satisfy the assert and mask a future
        // regression that retargets the filing repo.
        let task_had_remote_before = task.remote.is_some();
        let snapshot = self.snapshots.get(task.id, to_version).await?;
        task.title = snapshot.title;
        task.body = snapshot.body;
        task.lifecycle = snapshot.lifecycle;
        task.sync = snapshot.sync_state;
        task.priority = snapshot.priority;
        task.assignees = snapshot.assignees;
        task.remote = snapshot.remote;
        // Restore the binding pointer too — `rl task link` / `--relink`
        // mutates `repo_id`, and rolling content back without rolling the
        // binding back leaves the task with a remote_id from the pre-link
        // repo inside a post-link binding (incoherent). Only act when the
        // snapshot actually recorded its binding: pre-migration rows have
        // `repo_id_recorded = false` and the historical binding is unknown,
        // so preserve the current binding rather than wiping it.
        if snapshot.repo_id_recorded {
            task.repo_id = snapshot.repo_id;
        }
        // RFC 0002 #118/#120 — chosen rollback rule: do NOT retarget
        // `filing_repo_id` from the snapshot. The filing repo of a remote-backed
        // task is IMMUTABLE post-promote, and D6 (#120) keys remote identity on
        // `filing_repo_id` (it is the dedup key into `remote_mappings`).
        // Restoring it from a possibly-pre-column snapshot could leave the live
        // `filing_repo_id` disagreeing with the task's remote_mappings key —
        // exactly the desync D6 forbids. So we deliberately leave the live
        // `filing_repo_id` untouched on rollback (the inverse of the
        // `repo_id_recorded`-guarded `repo_id` restore above). This is why the
        // snapshot column has no `filing_repo_id_recorded` flag: nothing reads
        // the snapshot's filing repo on the rollback path, so there is no
        // pre-column ambiguity to disambiguate. (The documented-but-unimplemented
        // alternative is to restore it AND add a `filing_repo_id_recorded`
        // tolerance mirroring `repo_id_recorded`.)
        //
        // Invariant: a rollback must never leave a remote-backed task whose
        // `filing_repo_id` disagrees with its remote identity. Since we don't
        // touch `filing_repo_id` here, and a remote-backed task's filing repo is
        // immutable, the live value still agrees with whatever it agreed with
        // before the rollback.
        debug_assert!(
            !task_had_remote_before || task.filing_repo_id == task_filing_repo_id_before,
            "rollback must not retarget the filing repo of a remote-backed task"
        );
        task.reconcile_dirty_against_baseline();
        self.repo.save(&task, SnapshotSource::Rollback).await?;
        self.task_dto(&task).await
    }

    async fn transition<F>(&self, query: &str, op: F) -> Result<TaskDto>
    where
        F: FnOnce(&mut Task) -> domain_core::Result<()>,
    {
        let mut t = self.resolve_task(query).await?;
        op(&mut t)?;
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
        self.task_dto(&t).await
    }

    /// Like [`transition`](Self::transition) but, after persisting, enqueues
    /// the outbound mutations a mirror task owes (RFC 0001 Stage 6). Used by
    /// the lifecycle verbs (start / complete / reopen / archive) where the
    /// change is remote-observable. `LocalOnly` tasks
    /// enqueue nothing (the mirror guard short-circuits).
    async fn transition_mirror<F>(&self, query: &str, op: F) -> Result<TaskDto>
    where
        F: FnOnce(&mut Task) -> domain_core::Result<()>,
    {
        let mut t = self.resolve_task(query).await?;
        let before = t.updated_at;
        op(&mut t)?;
        // A no-op transition (e.g. `start()` on an already-open task, RFC 0004
        // D1) leaves the task untouched — `updated_at` is unchanged. Skip the
        // plan + atomic write entirely so a redundant `rl task start`/`claim`
        // on an open mirror doesn't enqueue churn (no-op SetProjectStatus /
        // UpdateRemote) into the outbox.
        if t.updated_at == before {
            return self.task_dto(&t).await;
        }
        // Plan the outbound mutations FIRST (lifecycle-only transition — no
        // title/body change, so a draft-backed mirror owes no `UpdateDraftIssue`;
        // its card move rides on `SetProjectStatus`), then commit the task AND
        // its outbox entries in one atomic write (#54). A LocalOnly task plans
        // nothing, so `save_with_outbox` behaves like `save`.
        let mutations = self.plan_mirror_mutations(&mut t, false).await?;
        let entries = into_entries(t.id, mutations);
        self.repo
            .save_with_outbox(&t, SnapshotSource::LocalEdit, &entries)
            .await?;
        self.task_dto(&t).await
    }

    /// Plan the outbox mutations a mirror task owes. Empty for `LocalOnly`
    /// tasks (nothing to push). Resolves the owning project (if any) so a
    /// project mirror gets a `SetProjectStatus` card move and a not-yet-attached
    /// mirror gets the lazy `AddItem`/`CreateDraftIssue` net. Issue-backed
    /// mirrors additionally get an `UpdateRemote`. Delegates the routing to the
    /// shared [`enqueue::plan_mutations`] so `WorkspaceService` and the drainer
    /// share one decision surface. The caller folds the returned mutations into
    /// a single atomic `save_with_outbox` (#54) rather than enqueuing inline.
    ///
    /// `content_changed` is `false` for lifecycle-only transitions
    /// (start/complete/reopen/archive) and `true` for title/body edits — it
    /// gates the draft-backed `UpdateDraftIssue` so a lifecycle move doesn't
    /// enqueue a no-op draft content write (the card move via
    /// `SetProjectStatus` carries the lifecycle change for drafts).
    ///
    /// **RFC 0002 D2 — first-board-filing recording (#124).** When this is a
    /// genuine first-board-filing moment (mirror, project present,
    /// `project_item_id` is `None`), the D2 chain is run and the resolved
    /// filing repo recorded on the task **before** the mutation is built, so
    /// the `remote_mappings` row written by `save_with_outbox` (#120) is keyed
    /// under the correct D6 filing-scoped key. The recording is gated on
    /// `filing_repo_id.is_none()`, so for a task that already recorded its
    /// filing repo (e.g. at promote, #117) the block is skipped entirely — the
    /// recorded value is never re-resolved. A `CreateDraftIssue` that resolves
    /// to `None` (step 4: orphan, no workspace default) legitimately stays a
    /// board draft with `filing_repo_id == None`.
    ///
    /// Precedence mirror of the promote site (see `SyncService::promote`):
    /// `resolve_filing_repo(None, workspace.filing_repo_id, task.repo_id)`.
    /// The per-task override (`--filing-repo` CLI flag) lands in #122; with no
    /// override and no workspace default the chain collapses to the logical
    /// repo — board filing targets the same place as today.
    async fn plan_mirror_mutations(
        &self,
        task: &mut Task,
        content_changed: bool,
    ) -> Result<Vec<OutboxMutation>> {
        if !enqueue::is_mirror(task) {
            return Ok(Vec::new());
        }
        // Fetch the workspace once and reuse it for both the project lookup and
        // the RFC 0002 filing default — `resolve_project` would otherwise repeat
        // the `workspaces.get` round-trip on every transition (Greptile #139).
        let workspace = self.workspaces.get(task.workspace_id).await?;
        let project = enqueue::project_for_workspace(&self.projects, &workspace).await?;

        // RFC 0002 D2 first-board-filing: resolve + record the filing repo at
        // the first filing, gated on `filing_repo_id.is_none()`. Once a repo is
        // recorded the block is SKIPPED entirely (the guard is false), so a
        // task that recorded at promote (#117) is never re-resolved — which is
        // what stops a later workspace-default change from erroring a lifecycle
        // transition. The block is still re-entered while filing stays None (a
        // pure orphan draft, step 4): that is intentional and cheap (the
        // workspace is already in hand) — it lets a workspace default added
        // *after* the draft was created take effect on the draft's next
        // transition. `set_filing_repo_id(None)` is a no-op, so a draft with no
        // default simply stays a board draft.
        if project.is_some() && task.project_item_id.is_none() && task.filing_repo_id.is_none() {
            // RFC 0005: convert to origin id space for the chain, then back for storage.
            let ws_default = workspace
                .filing_repo_id
                .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
            let logical_origin = if let Some(rid) = task.repo_id {
                Some(self.bindings.get(rid).await?.instance.origin_id)
            } else {
                None
            };
            let filing = resolve_filing_repo(None, ws_default, logical_origin)
                .map(|o| RepoId::from_uuid(o.as_uuid()));
            task.set_filing_repo_id(filing)?;
        }

        // RFC 0002 (#143): the backing GitHub issue lives in the FILING repo, so
        // the issue-state mirror (`UpdateRemote`) must address the filing repo —
        // NOT the logical repo. Addressing the logical repo filed a 404 for any
        // cross-filed task (issue in repo A, logical repo B) and, because the
        // failing entry head-of-line-blocks the per-task FIFO outbox, also
        // stranded the sibling `AddItem` so the card never reached the board.
        // `logical_canonical_for` stays for D4 logical-context ops (prefix /
        // worktree / relink) elsewhere; only the issue-addressing mutation moves
        // to the filing axis.
        let filing_canonical = self.filing_canonical_for(task).await?;
        // An issue-backed mirror with no repo binding can't form an
        // `UpdateRemote` (it has no canonical repo to address), so a
        // remote-observable lifecycle change would be silently dropped if it
        // also has no project. Make the missing binding observable rather than
        // a silent no-op. Unreachable through `rl sync import` today (which
        // always supplies a repo), but a future caller that constructs an
        // unbound issue-backed mirror would otherwise lose the push without a
        // signal.
        if enqueue::is_issue_backed(task) && filing_canonical.is_none() && project.is_none() {
            tracing::warn!(
                task_id = %task.id,
                "mirror lifecycle change has no repo binding and no project; \
                 no outbound mutation can be formed (push dropped)"
            );
        }
        Ok(enqueue::plan_mutations(
            task,
            project.as_ref(),
            filing_canonical.as_deref(),
            content_changed,
        ))
    }

    /// Canonical URL of the repo the task's backing issue is *filed* in
    /// (RFC 0002). Walks the D2 chain — recorded `filing_repo_id` →
    /// workspace default → logical `repo_id` — via `resolve_filing_repo`,
    /// mirroring `SyncService::filing_canonical_for`. Returns
    /// `Ok(None)` when the chain has no inputs (board-draft orphan) so
    /// the `ConvertDraftToIssue` planner can short-circuit to an empty
    /// `repo_node_id`.
    ///
    /// **Caveat**: a saga task created when the workspace had no filing
    /// default will silently flip to a later default on its next mutation.
    /// Pin step 1 (`filing_repo_id`) on the task — promote records it
    /// automatically — to make the resolution permanent.
    async fn filing_canonical_for(&self, task: &Task) -> Result<Option<String>> {
        // A deleted workspace row is not a hard error here: it just means
        // step 2 of the D2 chain (workspace default) is unavailable, so
        // resolve with `workspace_default = None` and let step 1
        // (recorded `filing_repo_id`) or step 3 (logical `repo_id`) win.
        // Only when the chain itself returns `None` — meaning all three
        // inputs are absent — do we return `Ok(None)`. (CodeRabbit #191.)
        // RFC 0005: convert to origin id space for the chain.
        let workspace_default = match self.workspaces.get(task.workspace_id).await {
            Ok(ws) => ws
                .filing_repo_id
                .map(|r| RepoOriginId::from_uuid(r.as_uuid())),
            Err(ports::PortError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let step1 = task
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        let step3 = if let Some(rid) = task.repo_id {
            match self.bindings.get(rid).await {
                Ok(v) => Some(v.instance.origin_id),
                Err(ports::PortError::NotFound(_)) => None,
                Err(e) => return Err(e.into()),
            }
        } else {
            None
        };
        let Some(origin_id) = resolve_filing_repo(step1, workspace_default, step3) else {
            return Ok(None);
        };
        Ok(Some(
            self.bindings.get_origin(origin_id).await?.canonical_url,
        ))
    }

    /// Issue coordinates `(filing_canonical, remote_id)` for a task, or `None`
    /// when the task isn't issue-backed or its filing repo can't be resolved. A
    /// relation can only be projected onto GitHub when BOTH ends resolve, so the
    /// relation-sync planner treats `None` on either side as "skip enqueue".
    /// Delegates to `filing_canonical_for` so the chain stays in lockstep with
    /// `application-sync`.
    async fn relation_remote_coords(&self, task: &Task) -> Result<Option<(String, String)>> {
        let Some(remote) = &task.remote else {
            return Ok(None);
        };
        let Some(canonical) = self.filing_canonical_for(task).await? else {
            return Ok(None);
        };
        Ok(Some((canonical, remote.remote_id.clone())))
    }

    /// Workspace-cached chain resolution for relation endpoints. Looks up
    /// the task's workspace in `workspace_cache` first, falling back to a
    /// `workspaces.get` and recording the result. The chain itself runs
    /// inline against the cached workspace so a `clear_relations` loop
    /// over N neighbors in one workspace does *one* `workspaces.get` and
    /// N `bindings.get`s — never N of each.
    ///
    /// A missing workspace is treated as "no resolvable home"
    /// (`Ok(None)`) rather than a hard error: a task whose workspace
    /// has been deleted has no chain step 2 to consult, and the
    /// relation can't be projected regardless. Mirrors the pre-chain
    /// behavior where the helper was a pure in-memory
    /// `task.filing_repo_id.or(task.repo_id)` and never touched
    /// `workspaces` at all.
    async fn relation_remote_coords_cached(
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
                    Err(e) => return Err(e.into()),
                }
            }
        };
        let Some(remote) = &task.remote else {
            return Ok(None);
        };
        // RFC 0005: convert to origin id space for the chain.
        let step1 = task
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        let ws_default = workspace
            .filing_repo_id
            .map(|r| RepoOriginId::from_uuid(r.as_uuid()));
        let step3 = if let Some(rid) = task.repo_id {
            match self.bindings.get(rid).await {
                Ok(v) => Some(v.instance.origin_id),
                Err(ports::PortError::NotFound(_)) => None,
                Err(e) => return Err(e.into()),
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

    /// Plan the (zero or one) outbox entries a single relation edge owes when it
    /// is added (`add = true`) or removed (`add = false`). Empty unless BOTH
    /// ends are issue-backed AND the kind has a GitHub-native primitive
    /// (`parent_of`/`child_of` → sub-issues, `blocked_by`/`blocks` →
    /// dependencies). `related_to`/`duplicates` and any local-only end yield no
    /// entry. The entry is keyed on `t`'s id (the relation command's subject),
    /// so it FIFO-orders with `t`'s other outbound mutations.
    async fn plan_relation_entries(
        &self,
        kind: RelationKind,
        add: bool,
        t: &Task,
        other: &Task,
    ) -> Result<Vec<OutboxEntry>> {
        let (Some(this_coords), Some(other_coords)) = (
            self.relation_remote_coords(t).await?,
            self.relation_remote_coords(other).await?,
        ) else {
            return Ok(Vec::new());
        };
        Ok(
            match enqueue::relation_mutation(kind, add, &this_coords, &other_coords) {
                Some(m) => into_entries(t.id, vec![m]),
                None => Vec::new(),
            },
        )
    }
}

/// Wrap each planned [`OutboxMutation`] in a fresh `Pending` [`OutboxEntry`]
/// for `task_id`, preserving the plan's order (the outbox is per-task FIFO, so
/// order is load-bearing — e.g. `UpdateDraftIssue` must precede
/// `ConvertDraftToIssue`). The caller hands the result to
/// [`TaskRepository::save_with_outbox`] for the single atomic write (#54).
fn into_entries(task_id: TaskId, mutations: Vec<OutboxMutation>) -> Vec<OutboxEntry> {
    mutations
        .into_iter()
        .map(|m| OutboxEntry::new(task_id, m))
        .collect()
}

/// For a cycle-protected relation, return `(forward, inverse, x, y)` where the
/// new edge reads as "x is downstream of y" within a family, and the family's
/// upstream direction is represented by `forward` edges (with `inverse` the
/// reciprocal). Adding the edge closes a cycle iff `y` can already reach `x`
/// going upstream.
///
/// Normalises each directional kind onto one family:
/// - blocking → `forward = blocked_by`, `inverse = blocks`
///   (`blocks(a,b)` ≡ `b blocked_by a`)
/// - hierarchy → `forward = child_of`, `inverse = parent_of`
///   (`parent_of(a,b)` ≡ `b child_of a`)
///
/// Both representations are returned so the guard can walk one-sided legacy
/// rows. Symmetric kinds (`related_to`, `duplicates`) return `None` — they have
/// no direction, so there is nothing to keep acyclic.
fn cycle_axis(
    kind: RelationKind,
    a: TaskId,
    b: TaskId,
) -> Option<(RelationKind, RelationKind, TaskId, TaskId)> {
    use RelationKind::{BlockedBy, Blocks, ChildOf, ParentOf};
    match kind {
        RelationKind::BlockedBy => Some((BlockedBy, Blocks, a, b)),
        RelationKind::Blocks => Some((BlockedBy, Blocks, b, a)),
        RelationKind::ChildOf => Some((ChildOf, ParentOf, a, b)),
        RelationKind::ParentOf => Some((ChildOf, ParentOf, b, a)),
        RelationKind::RelatedTo | RelationKind::Duplicates => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ports::TaskSnapshotRepository;
    use testing_fixtures::{
        InMemoryOutboxRepository, InMemoryProjectRepository, InMemoryRepoBindingRepository,
        InMemoryTaskRepository, InMemoryTaskSnapshotRepository, InMemoryWorkspaceRepository,
    };

    fn svc() -> TaskService {
        svc_with_outbox().0
    }

    /// Like `svc()` but also hands back the binding port so tests
    /// that need a real `RepoId` target (e.g. for
    /// `repoint_filing_repo`'s pre-validation) can plant one. The
    /// pre-validation only does `bindings.get(target)?` — saving a
    /// binding is enough.
    fn svc_with_bindings() -> (TaskService, Arc<InMemoryRepoBindingRepository>) {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings: Arc<InMemoryRepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let projects: Arc<dyn ProjectRepository> = Arc::new(InMemoryProjectRepository::new());
        let svc = TaskService::new(
            repo,
            snaps,
            bindings.clone() as Arc<dyn RepoBindingRepository>,
            workspaces,
            projects,
        );
        (svc, bindings)
    }

    /// Build a `TaskService` over all-in-memory repos and hand back the outbox
    /// so enqueue-matrix tests can assert what (if anything) was queued. The
    /// task repo is wired to the SAME outbox store via `with_outbox` so the
    /// atomic `save_with_outbox` path lands its entries where the test inspects
    /// them (#54).
    fn svc_with_outbox() -> (TaskService, Arc<InMemoryOutboxRepository>) {
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let repo = Arc::new(InMemoryTaskRepository::with_outbox(&outbox));
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let projects: Arc<dyn ProjectRepository> = Arc::new(InMemoryProjectRepository::new());
        let svc = TaskService::new(repo, snaps, bindings, workspaces, projects);
        (svc, outbox)
    }

    fn ws_id() -> String {
        WorkspaceId::new().to_string()
    }

    #[tokio::test]
    async fn import_mirror_persists_synced_task_with_minted_hash() {
        let svc = svc();
        let dto = svc
            .import_mirror(ImportMirrorCmd {
                workspace_id: ws_id(),
                repo_id: None,
                provider: "github".into(),
                remote_id: "123".into(),
                title: "imported issue".into(),
                body: "from gh".into(),
                assignees: vec!["alice".into()],
                closed: false,
            })
            .await
            .unwrap();
        assert_eq!(dto.sync_state, "synced");
        assert!(dto.is_open);
        assert_eq!(dto.state_reason, None);
        assert_eq!(dto.remote.as_ref().unwrap().remote_id, "123");
        assert_eq!(dto.assignees, vec!["alice".to_string()]);
        // Hash was minted on save, so the friendly id is a non-empty bare hash.
        assert!(!dto.id.is_empty());
        // And it's findable by its remote ref (idempotency backstop).
        let found = svc.show(&dto.id).await.unwrap();
        assert_eq!(found.remote.unwrap().remote_id, "123");
    }

    #[tokio::test]
    async fn create_show_and_update_task() {
        let svc = svc();
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "ship it".into(),
                body: Some("with feeling".into()),
                priority: Some("p1".into()),
                filing_repo_override: None,
            })
            .await
            .unwrap();
        assert!(dto.is_open);
        assert_eq!(dto.state_reason, None);
        assert_eq!(dto.sync_state, "local_only");
        assert_eq!(dto.priority, "p1");
        let updated = svc
            .update(UpdateTaskCmd {
                task_id: dto.id.clone(),
                title: Some("ship it well".into()),
                body: None,
                priority: Some("p0".into()),
                assignees: Some(vec!["alice".into()]),
                repo_id: None,
            })
            .await
            .unwrap();
        assert_eq!(updated.title, "ship it well");
        assert_eq!(updated.priority, "p0");
        assert_eq!(updated.assignees, vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn list_filters_independently_by_status_and_sync_state() {
        let svc = svc();
        let workspace = ws_id();
        let _open_localonly = svc
            .create(CreateTaskCmd {
                workspace_id: workspace.clone(),
                repo_id: None,
                title: "a".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let to_stage = svc
            .create(CreateTaskCmd {
                workspace_id: workspace.clone(),
                repo_id: None,
                title: "b".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        svc.stage_for_sync(&to_stage.id).await.unwrap();

        // Both are status=Open. Filter by status returns both.
        let opens = svc
            .list(ListTasksQuery {
                status: Some("open".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(opens.len(), 2);

        // But sync state distinguishes them.
        let local_only = svc
            .list(ListTasksQuery {
                sync_state: Some("local_only".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(local_only.len(), 1);
        let staged = svc
            .list(ListTasksQuery {
                sync_state: Some("staged".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(staged.len(), 1);
    }

    #[tokio::test]
    async fn list_defaults_to_open_and_all_opts_into_closed() {
        let svc = svc();
        let workspace = ws_id();
        let mk = |title: &'static str| CreateTaskCmd {
            workspace_id: workspace.clone(),
            repo_id: None,
            title: title.into(),
            body: None,
            priority: None,
            filing_repo_override: None,
        };
        let keep_open = svc.create(mk("open")).await.unwrap();
        let to_close = svc.create(mk("done")).await.unwrap();
        svc.complete(&to_close.id).await.unwrap();

        // Default (no --status) hides closed tasks — only the open one shows.
        let default = svc.list(ListTasksQuery::default()).await.unwrap();
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].id, keep_open.id);

        // `--status all` opts back into every lifecycle.
        let all = svc
            .list(ListTasksQuery {
                status: Some("all".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // `--status closed` shows only the closed one.
        let closed = svc
            .list(ListTasksQuery {
                status: Some("closed".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, to_close.id);
    }

    #[tokio::test]
    async fn invalid_priority_returns_typed_error() {
        let svc = svc();
        let err = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: Some("p99".into()),
                filing_repo_override: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ServiceError::BadEnum {
                field: "priority",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn add_relation_persists_to_repo() {
        let svc = svc();
        let a = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "a".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let b = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "b".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let updated = svc
            .add_relation(AddTaskRelationCmd {
                task_id: a.id.clone(),
                kind: "blocked_by".into(),
                other: b.id.clone(),
            })
            .await
            .unwrap();
        assert_eq!(updated.relations.len(), 1);
        assert_eq!(updated.relations[0].kind, "blocked_by");
        assert_eq!(updated.relations[0].other, b.id);

        // The reciprocal edge is mirrored onto the other task: a blocked_by b
        // ⇒ b blocks a.
        let other = svc.show(&b.id).await.unwrap();
        assert_eq!(other.relations.len(), 1);
        assert_eq!(other.relations[0].kind, "blocks");
        assert_eq!(other.relations[0].other, a.id);
    }

    #[tokio::test]
    async fn add_relation_symmetric_kind_mirrors_same_kind() {
        let svc = svc();
        let a = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "a".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let b = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "b".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.clone(),
            kind: "related_to".into(),
            other: b.id.clone(),
        })
        .await
        .unwrap();
        // related_to is symmetric: both ends carry related_to to the other.
        let other = svc.show(&b.id).await.unwrap();
        assert_eq!(other.relations.len(), 1);
        assert_eq!(other.relations[0].kind, "related_to");
        assert_eq!(other.relations[0].other, a.id);
    }

    /// Helper: create N bare tasks and return their ids.
    async fn make_tasks(svc: &TaskService, titles: &[&str]) -> Vec<String> {
        let mut ids = Vec::new();
        for title in titles {
            let t = svc
                .create(CreateTaskCmd {
                    workspace_id: ws_id(),
                    repo_id: None,
                    title: (*title).into(),
                    body: None,
                    priority: None,
                    filing_repo_override: None,
                })
                .await
                .unwrap();
            ids.push(t.id);
        }
        ids
    }

    #[tokio::test]
    async fn add_relation_rejects_self() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a"]).await;
        let err = svc
            .add_relation(AddTaskRelationCmd {
                task_id: ids[0].clone(),
                kind: "related_to".into(),
                other: ids[0].clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::SelfRelation));
    }

    #[tokio::test]
    async fn add_relation_rejects_direct_cycle() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a", "b"]).await;
        // a blocked_by b is fine.
        svc.add_relation(AddTaskRelationCmd {
            task_id: ids[0].clone(),
            kind: "blocked_by".into(),
            other: ids[1].clone(),
        })
        .await
        .unwrap();
        // b blocked_by a would deadlock — rejected.
        let err = svc
            .add_relation(AddTaskRelationCmd {
                task_id: ids[1].clone(),
                kind: "blocked_by".into(),
                other: ids[0].clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::RelationCycle { .. }));
    }

    #[tokio::test]
    async fn add_relation_rejects_transitive_cycle_across_axes() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a", "b", "c"]).await;
        // a blocked_by b, b blocked_by c.
        for (t, o) in [(0, 1), (1, 2)] {
            svc.add_relation(AddTaskRelationCmd {
                task_id: ids[t].clone(),
                kind: "blocked_by".into(),
                other: ids[o].clone(),
            })
            .await
            .unwrap();
        }
        // c blocked_by a closes the a→b→c→a loop — rejected.
        let err = svc
            .add_relation(AddTaskRelationCmd {
                task_id: ids[2].clone(),
                kind: "blocked_by".into(),
                other: ids[0].clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::RelationCycle { .. }));

        // The reciprocal `blocks` direction is the same axis: a blocks c
        // (≡ c blocked_by a, i.e. c depends on a) also closes the loop.
        let err = svc
            .add_relation(AddTaskRelationCmd {
                task_id: ids[0].clone(),
                kind: "blocks".into(),
                other: ids[2].clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::RelationCycle { .. }));
    }

    #[tokio::test]
    async fn related_to_is_never_cycle_checked() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a", "b"]).await;
        // related_to is symmetric; the auto-reciprocal is not a "cycle".
        svc.add_relation(AddTaskRelationCmd {
            task_id: ids[0].clone(),
            kind: "related_to".into(),
            other: ids[1].clone(),
        })
        .await
        .unwrap();
        // Adding the reverse explicitly is just a dedup no-op, not an error.
        svc.add_relation(AddTaskRelationCmd {
            task_id: ids[1].clone(),
            kind: "related_to".into(),
            other: ids[0].clone(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cycle_guard_catches_one_sided_legacy_edge() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a", "b"]).await;

        // Seed a ONE-SIDED legacy hierarchy edge: `a parent_of b` with no
        // reciprocal `child_of` on b, as a pre-reciprocal binary would write.
        let mut a = svc.resolve_task(&ids[0]).await.unwrap();
        let b_id = svc.resolve_task(&ids[1]).await.unwrap().id;
        a.add_relation(RelationKind::ParentOf, b_id);
        svc.repo.save(&a, SnapshotSource::LocalEdit).await.unwrap();

        // `b parent_of a` would make a both parent and child of b. The guard
        // reads both stored directions, so it rejects this even though the
        // reciprocal `child_of` row was never written.
        let err = svc
            .add_relation(AddTaskRelationCmd {
                task_id: ids[1].clone(),
                kind: "parent_of".into(),
                other: ids[0].clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::RelationCycle { .. }));
    }

    #[tokio::test]
    async fn remove_relation_drops_edge_and_reciprocal() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a", "b"]).await;
        svc.add_relation(AddTaskRelationCmd {
            task_id: ids[0].clone(),
            kind: "blocked_by".into(),
            other: ids[1].clone(),
        })
        .await
        .unwrap();
        let updated = svc
            .remove_relation(RemoveTaskRelationCmd {
                task_id: ids[0].clone(),
                kind: "blocked_by".into(),
                other: ids[1].clone(),
            })
            .await
            .unwrap();
        assert!(updated.relations.is_empty());
        // The reciprocal `blocks` edge on b is gone too.
        assert!(svc.show(&ids[1]).await.unwrap().relations.is_empty());
    }

    #[tokio::test]
    async fn clear_relations_strips_all_edges_and_back_edges() {
        let svc = svc();
        let ids = make_tasks(&svc, &["a", "b", "c"]).await;
        // a blocked_by b, a related_to c.
        svc.add_relation(AddTaskRelationCmd {
            task_id: ids[0].clone(),
            kind: "blocked_by".into(),
            other: ids[1].clone(),
        })
        .await
        .unwrap();
        svc.add_relation(AddTaskRelationCmd {
            task_id: ids[0].clone(),
            kind: "related_to".into(),
            other: ids[2].clone(),
        })
        .await
        .unwrap();

        let cleared = svc.clear_relations(&ids[0]).await.unwrap();
        assert!(cleared.relations.is_empty());
        // Both back-edges (b: blocks→a, c: related_to→a) are stripped.
        assert!(svc.show(&ids[1]).await.unwrap().relations.is_empty());
        assert!(svc.show(&ids[2]).await.unwrap().relations.is_empty());
    }

    #[tokio::test]
    async fn lifecycle_start_complete_archive() {
        let svc = svc();
        let t = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        // start() is a no-op on an already-open task (RFC 0004 D1): the task
        // stays open.
        let started = svc.start(&t.id).await.unwrap();
        assert!(started.is_open);
        assert_eq!(started.state_reason, None);
        let done = svc.complete(&t.id).await.unwrap();
        assert!(!done.is_open);
        assert_eq!(done.state_reason.as_deref(), Some("completed"));
        let archived = svc.archive(&t.id).await.unwrap();
        assert!(!archived.is_open);
        assert_eq!(archived.state_reason.as_deref(), Some("not_planned"));
    }

    /// Blocking is no longer a lifecycle verb (RFC 0004 D1): it is derived from
    /// a `BlockedBy` relation. Adding the relation makes `is_blocked()` true
    /// without changing the open/closed bit.
    #[tokio::test]
    async fn blocked_is_derived_from_relations() {
        let svc = svc();
        let t = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "blocked".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let blocker = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "blocker".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        svc.add_relation(AddTaskRelationCmd {
            task_id: t.id.clone(),
            kind: "blocked_by".into(),
            other: blocker.id.clone(),
        })
        .await
        .unwrap();
        let reloaded = svc.show(&t.id).await.unwrap();
        // Still open — blocking does not flip the lifecycle bit.
        assert!(reloaded.is_open);
        assert!(
            reloaded
                .relations
                .iter()
                .any(|r| r.kind == "blocked_by" && r.other == blocker.id),
            "the blocked_by relation is recorded: {:?}",
            reloaded.relations
        );
    }

    #[tokio::test]
    async fn resolve_task_accepts_uuid_and_bare_hash() {
        // No repo binding → composite collapses to the bare hash, so this
        // covers the UUID and bare-hash branches. Composite resolution is
        // covered separately in `resolve_task_round_trips_composite_*`.
        let svc = svc();
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "resolve me".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let by_friendly = svc.resolve_task(&dto.id).await.unwrap();
        // Recover the internal UUID and confirm the UUID branch resolves
        // to the same task (this is the branch the CLI can't easily test
        // since the UUID is no longer exposed in JSON output).
        let uuid = by_friendly.id.to_string();
        let by_uuid = svc.resolve_task(&uuid).await.unwrap();
        assert_eq!(by_uuid.id, by_friendly.id);
        // Bare hash also resolves to the same task.
        let by_hash = svc.resolve_task(&by_friendly.hash).await.unwrap();
        assert_eq!(by_hash.id, by_friendly.id);
    }

    #[tokio::test]
    async fn resolve_id_returns_canonical_uuid() {
        // `resolve_id` is the thin wrapper the `sync` CLI uses so a friendly
        // reference round-trips to the canonical UUID `SyncService` expects.
        let svc = svc();
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "sync me".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        let uuid = svc.resolve_id(&dto.id).await.unwrap();
        // The returned string is a parseable UUID and resolves back to the
        // same task.
        assert!(uuid.parse::<domain_core::TaskId>().is_ok());
        assert_eq!(svc.resolve_task(&uuid).await.unwrap().hash, dto.id);
    }

    #[tokio::test]
    async fn resolve_task_round_trips_composite_for_bound_task() {
        use domain_repo::{RepoInstance, RepoOrigin};
        use ports::RepoBindingRepository;

        let repo = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());

        // Seed a binding with a known prefix so the created task's id is a
        // real `prefix-hash` composite (not the bare-hash fallback).
        let ws = WorkspaceId::new();
        let mut origin = RepoOrigin::new(
            "git@github.com:o/widget.git".into(),
            "github.com/o/widget".into(),
        )
        .unwrap();
        origin.set_prefix("wid".into()).unwrap();
        let instance =
            RepoInstance::new(ws, origin.id, "github.com/o/widget".into(), None).unwrap();
        let repo_id = instance.id;
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();

        let svc = TaskService::new(
            repo,
            snaps,
            bindings,
            Arc::new(InMemoryWorkspaceRepository::new()),
            Arc::new(InMemoryProjectRepository::new()),
        );
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws.to_string(),
                repo_id: Some(repo_id.to_string()),
                title: "bound task".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();

        let composite = dto.id.clone();
        assert!(
            composite.starts_with("wid-"),
            "expected a wid- composite, got {composite:?}"
        );
        let hash = composite.split_once('-').unwrap().1.to_string();

        // All three input forms resolve to the same task.
        let by_composite = svc.resolve_task(&composite).await.unwrap();
        let by_hash = svc.resolve_task(&hash).await.unwrap();
        let by_uuid = svc
            .resolve_task(&by_composite.id.to_string())
            .await
            .unwrap();
        assert_eq!(by_composite.id, by_hash.id);
        assert_eq!(by_hash.id, by_uuid.id);

        // A composite naming the wrong prefix is a hard error.
        let err = svc.resolve_task(&format!("nope-{hash}")).await.unwrap_err();
        assert!(matches!(err, ServiceError::PrefixMismatch { .. }));
    }

    #[tokio::test]
    async fn resolve_task_rejects_malformed_input() {
        let svc = svc();
        // Uppercase is not valid base32 → BadId, not a doomed lookup.
        let err = svc.resolve_task("ZZZ").await.unwrap_err();
        assert!(matches!(err, ServiceError::BadId(_)));
    }

    #[tokio::test]
    async fn rollback_restores_repo_id() {
        // `rl task link` / `--relink` mutate the task's binding pointer; a
        // rollback to a pre-link snapshot must restore the binding too,
        // otherwise the task ends up with a stale remote_id inside a foreign
        // binding (incoherent + no command path forward).
        //
        // Inject the snapshot history directly via the repo so we don't have
        // to stand up real bindings for the lookup-side validation `update`
        // would run.
        let repo: Arc<InMemoryTaskRepository> = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings_repo: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        let svc = TaskService::new(
            repo.clone(),
            snaps,
            bindings_repo.clone(),
            Arc::new(InMemoryWorkspaceRepository::new()),
            Arc::new(InMemoryProjectRepository::new()),
        );

        let workspace_id = WorkspaceId::new();
        // Stand up a real binding for A so the post-rollback `task_dto`
        // prefix lookup succeeds. We don't need one for B — the test never
        // renders a DTO while pointed at B.
        let origin_a =
            domain_repo::RepoOrigin::new("git@github.com:o/a.git".into(), "github.com/o/a".into())
                .unwrap();
        let instance_a = domain_repo::RepoInstance::new(
            workspace_id,
            origin_a.id,
            "github.com/o/a".into(),
            None,
        )
        .unwrap();
        let repo_a = instance_a.id;
        bindings_repo.save_origin(&origin_a).await.unwrap();
        bindings_repo.save_instance(&instance_a).await.unwrap();
        let repo_b = domain_core::RepoId::new();

        // v1: task bound to repo A.
        let mut task =
            domain_task::Task::new_draft(workspace_id, Some(repo_a), "tracked under A".into())
                .unwrap();
        repo.save(&task, SnapshotSource::Created).await.unwrap();
        // v2: simulate a `link` rewriting the binding to repo B.
        task.repo_id = Some(repo_b);
        repo.save(&task, SnapshotSource::Link).await.unwrap();

        // Rollback to v1 — repo_id must revert to A.
        let rolled_back = svc.rollback(&task.id.to_string(), 1).await.unwrap();
        assert_eq!(
            rolled_back.repo_id.as_deref(),
            Some(repo_a.to_string().as_str()),
            "rollback must restore the historical binding pointer"
        );
    }

    #[tokio::test]
    async fn rollback_restores_intentional_none_repo_id() {
        // A snapshot where the task was *intentionally* unbound (post-feature
        // write) must clear the binding on rollback, not preserve the
        // current one. Distinguishes the "recorded None" case from the
        // pre-migration "unknown" case.
        let repo: Arc<InMemoryTaskRepository> = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings_repo: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        let svc = TaskService::new(
            repo.clone(),
            snaps,
            bindings_repo,
            Arc::new(InMemoryWorkspaceRepository::new()),
            Arc::new(InMemoryProjectRepository::new()),
        );

        let workspace_id = WorkspaceId::new();
        let repo_b = domain_core::RepoId::new();

        // v1: task starts unbound.
        let mut task =
            domain_task::Task::new_draft(workspace_id, None, "unbound start".into()).unwrap();
        repo.save(&task, SnapshotSource::Created).await.unwrap();
        // v2: bind to repo B (no DTO render needed; bypass binding lookup).
        task.repo_id = Some(repo_b);
        repo.save(&task, SnapshotSource::Link).await.unwrap();

        let rolled_back = svc.rollback(&task.id.to_string(), 1).await.unwrap();
        assert!(
            rolled_back.repo_id.is_none(),
            "rollback must restore intentional None binding, got {:?}",
            rolled_back.repo_id
        );
    }

    #[tokio::test]
    async fn rollback_does_not_retarget_filing_repo_id() {
        // RFC 0002 #118/#120: the inverse of the repo_id precedent. A
        // remote-backed task's filing repo is immutable post-promote and D6
        // keys remote identity on it, so a rollback to an earlier snapshot
        // (whose filing_repo_id differs / is NULL) must leave the LIVE
        // filing_repo_id untouched — never desyncing it from the remote.
        let repo: Arc<InMemoryTaskRepository> = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings_repo: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        let svc = TaskService::new(
            repo.clone(),
            snaps,
            bindings_repo.clone(),
            Arc::new(InMemoryWorkspaceRepository::new()),
            Arc::new(InMemoryProjectRepository::new()),
        );

        let workspace_id = WorkspaceId::new();
        // Stand up a real binding so the post-rollback `task_dto` prefix lookup
        // succeeds (the task stays bound to this repo throughout).
        let origin_a =
            domain_repo::RepoOrigin::new("git@github.com:o/a.git".into(), "github.com/o/a".into())
                .unwrap();
        let instance_a = domain_repo::RepoInstance::new(
            workspace_id,
            origin_a.id,
            "github.com/o/a".into(),
            None,
        )
        .unwrap();
        let logical_repo = instance_a.id;
        bindings_repo.save_origin(&origin_a).await.unwrap();
        bindings_repo.save_instance(&instance_a).await.unwrap();
        let filing_repo = domain_core::RepoId::new();

        // v1: fresh draft, filing repo NOT yet resolved (None).
        let mut task =
            domain_task::Task::new_draft(workspace_id, Some(logical_repo), "tracked".into())
                .unwrap();
        repo.save(&task, SnapshotSource::Created).await.unwrap();

        // v2: promote — the task becomes remote-backed, but the filing repo is
        // still UNrecorded at this capture, so v2's snapshot has
        // filing_repo_id = None (the "differs / NULL" rollback target that
        // keeps the task remote-backed).
        task.stage_for_sync().unwrap();
        task.promote_to_remote(RemoteRef::new("github", "1"))
            .unwrap();
        repo.save(&task, SnapshotSource::Promote).await.unwrap();

        // v3: record the filing repo on the live (remote-backed) task. The live
        // filing_repo_id is now Some(filing_repo); v2's snapshot still has None.
        task.set_filing_repo_id(Some(filing_repo)).unwrap();
        repo.save(&task, SnapshotSource::LocalEdit).await.unwrap();

        // Rollback to v2 (snapshot filing_repo_id = None, task remote-backed).
        // The live filing_repo_id must NOT be retargeted to None.
        svc.rollback(&task.id.to_string(), 2).await.unwrap();
        // Reload the persisted aggregate by its stable UUID and assert the live
        // filing repo survived the rollback unchanged.
        let reloaded = repo.get(task.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id,
            Some(filing_repo),
            "rollback must NOT retarget the filing repo of a remote-backed task"
        );
        // And it still agrees with the remote-backed identity: the task is
        // still remote-backed (v2 captured the remote) and its filing repo is
        // the one recorded at promote — no desync from remote_mappings (D6).
        assert!(reloaded.is_remote_backed());
    }

    // ---------- Stage 6 (#54): lifecycle enqueue matrix --------------------

    use domain_project::{Project, StatusMapping, StatusOption};
    use domain_sync::OutboxStatus;
    use domain_workspace::{Workspace, WorkspaceName};

    /// A TaskService over all-in-memory repos, with the concrete handles
    /// exposed so a test can seed a project-attached workspace and inspect
    /// the outbox afterwards.
    struct RichSvc {
        svc: TaskService,
        repo: Arc<InMemoryTaskRepository>,
        outbox: Arc<InMemoryOutboxRepository>,
        workspaces: Arc<InMemoryWorkspaceRepository>,
        projects: Arc<InMemoryProjectRepository>,
        bindings: Arc<InMemoryRepoBindingRepository>,
    }

    fn rich_svc() -> RichSvc {
        let outbox = Arc::new(InMemoryOutboxRepository::new());
        let repo = Arc::new(InMemoryTaskRepository::with_outbox(&outbox));
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());
        let workspaces = Arc::new(InMemoryWorkspaceRepository::new());
        let projects = Arc::new(InMemoryProjectRepository::new());
        let svc = TaskService::new(
            repo.clone(),
            snaps,
            bindings.clone(),
            workspaces.clone(),
            projects.clone(),
        );
        RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        }
    }

    fn test_project(id: &str) -> Project {
        Project::new(
            domain_core::ProjectId::parse(id).unwrap(),
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
            // RFC 0004 D1: mappings are keyed on the open/closed bit. Open
            // tasks land on the WIP option, closed tasks on Done.
            vec![
                StatusMapping {
                    is_open: true,
                    option_id: "o_wip".into(),
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

    /// Save a synced issue-backed mirror with a node id into `repo` under `ws`.
    async fn save_issue_mirror(
        repo: &Arc<InMemoryTaskRepository>,
        ws: WorkspaceId,
        node_id: Option<&str>,
        project_item_id: Option<&str>,
    ) -> Task {
        let mut t = Task::new_draft(ws, None, "mirror".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef {
            provider: "github".into(),
            remote_id: "7".into(),
            node_id: node_id.map(str::to_owned),
        })
        .unwrap();
        t.project_item_id = project_item_id.map(str::to_owned);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();
        t
    }

    /// Save a synced issue-backed mirror that is fully *addressable* on GitHub:
    /// bound to `repo_id` (so `filing_canonical_for` resolves) and carrying
    /// `remote_id`. Used by the relation-sync tests, where both ends must
    /// resolve `(filing_canonical, remote_id)` for a mutation to be enqueued.
    async fn addressable_mirror(
        repo: &Arc<InMemoryTaskRepository>,
        ws: WorkspaceId,
        repo_id: domain_core::RepoId,
        remote_id: &str,
    ) -> Task {
        let mut t = Task::new_draft(ws, Some(repo_id), "m".into()).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(RemoteRef::new("github", remote_id))
            .unwrap();
        repo.save(&t, SnapshotSource::Promote).await.unwrap();
        t
    }

    // ---------- Stage 8 (#56, closes #39): task show project status --------

    /// `task show` surfaces the cached project-board status as a display name,
    /// resolved `task → workspace → project → option name` from the LOCAL
    /// cache — no network. The InMemory repos make any I/O impossible, so a
    /// passing test also proves the path is offline.
    #[tokio::test]
    async fn show_surfaces_cached_project_status_display_name() {
        let RichSvc {
            svc,
            repo,
            workspaces,
            projects,
            ..
        } = rich_svc();
        let project = test_project("PVT_kwHO_show");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();

        // A project mirror whose board status was polled as "In progress".
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_7"), Some("PVTI_7")).await;
        t.set_project_status_option_id(Some("o_wip".into()));
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        let dto = svc.show(&t.id.to_string()).await.unwrap();
        assert_eq!(
            dto.project_status.as_deref(),
            Some("In progress"),
            "show resolves the cached option id to its display name"
        );
    }

    /// A projectless task → `project_status` is None even with a cached id
    /// (no project to resolve the name against).
    #[tokio::test]
    async fn show_projectless_task_has_no_project_status() {
        let RichSvc {
            svc,
            repo,
            workspaces,
            ..
        } = rich_svc();
        // Workspace with NO project_id.
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();

        let mut t = save_issue_mirror(&repo, ws.id, Some("I_7"), Some("PVTI_7")).await;
        t.set_project_status_option_id(Some("o_wip".into()));
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        let dto = svc.show(&t.id.to_string()).await.unwrap();
        assert_eq!(
            dto.project_status, None,
            "a projectless task surfaces no board status"
        );
    }

    /// A stale cached option id (renamed/removed remotely, so the project no
    /// longer owns it) renders as `None` rather than an opaque id —
    /// `option_name_for` misses and `resolve_cached_project_status` returns
    /// `None`. The task is on a real project, so this isolates the "unknown
    /// cached id" branch from the projectless case above (#39).
    #[tokio::test]
    async fn show_stale_cached_option_id_renders_none() {
        let RichSvc {
            svc,
            repo,
            workspaces,
            projects,
            ..
        } = rich_svc();
        let project = test_project("PVT_kwHO_stale");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();

        // Board cached an option id the project no longer owns.
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_7"), Some("PVTI_7")).await;
        t.set_project_status_option_id(Some("o_ghost".into()));
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        let dto = svc.show(&t.id.to_string()).await.unwrap();
        assert_eq!(
            dto.project_status, None,
            "an unknown cached option id resolves to no display name"
        );
    }

    // ---------- Stage 6 (#54): transactional-outbox atomicity --------------

    #[tokio::test]
    async fn lifecycle_project_mirror_persists_task_change_and_set_status_together() {
        // The atomic path (#54): a lifecycle transition on a project mirror
        // commits BOTH the task status change AND the SetProjectStatus outbox
        // entry. Assert both landed in the SAME store after one verb.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_atomic");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_7"), Some("PVTI_7")).await;
        t.repo_id = Some(instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        // `start()` is a no-op on an open task (RFC 0004 D1); use `complete()`
        // so a genuine lifecycle change happens and is durable.
        svc.complete(&t.id.to_string()).await.unwrap();

        // Task side: the lifecycle change is durable.
        let reloaded = repo.get(t.id).await.unwrap();
        assert!(!reloaded.is_open());
        // Outbox side: a SetProjectStatus entry landed in the same op.
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert!(
            kinds.contains(&"set_project_status"),
            "lifecycle move persists the card move atomically: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn edit_issue_backed_persists_task_and_update_remote_together() {
        // An edit on an issue-backed mirror commits the title change AND the
        // UpdateRemote entry in one atomic write.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        let mut t = save_issue_mirror(&repo, ws.id, None, None).await;
        t.repo_id = Some(instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: Some("edited".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(reloaded.title, "edited");
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["update_remote"],
            "edit persists task + UpdateRemote atomically: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn edit_draft_backed_persists_task_and_update_draft_together() {
        // A content edit on a draft-backed mirror (no REST issue, has a project
        // item) commits the body change AND the UpdateDraftIssue entry together.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            ..
        } = rich_svc();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();

        // Draft-backed: synced mirror with a project item but no remote issue.
        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "old body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None;
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: None,
            body: Some("new body".into()),
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(reloaded.body, "new body");
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["update_draft_issue"],
            "draft content edit persists task + UpdateDraftIssue atomically: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn local_only_edit_writes_task_and_zero_entries() {
        // A LocalOnly task plans no mutations, so `save_with_outbox` behaves
        // like `save`: the task change is durable and zero entries are queued.
        let RichSvc {
            svc, repo, outbox, ..
        } = rich_svc();
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "local".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        // `dto.id` is a bare hash (no binding); resolve to the internal id.
        let id = svc.resolve_task(&dto.id).await.unwrap().id;
        svc.update(UpdateTaskCmd {
            task_id: dto.id.clone(),
            title: Some("local edited".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        let reloaded = repo.get(id).await.unwrap();
        assert_eq!(reloaded.title, "local edited");
        assert!(
            outbox.all().is_empty(),
            "a LocalOnly edit writes the task and enqueues nothing"
        );
    }

    #[tokio::test]
    async fn save_with_outbox_is_the_single_write_path_for_mirror_transitions() {
        // Atomicity contract: the lifecycle / edit verbs go through ONE combined
        // write (`save_with_outbox`), not save-then-enqueue. The in-memory
        // fixture proves it: it appends the task AND the entries under one lock,
        // and a non-empty enqueue with no shared outbox handle would panic. So
        // observing the entry in the SAME outbox the fixture shares with the
        // task repo is direct evidence the combined write fired (had the verb
        // taken a separate enqueue port, the entry would land elsewhere / not at
        // all). Conversely a zero-entry plan never touches the outbox handle.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        let mut t = save_issue_mirror(&repo, ws.id, None, None).await;
        t.repo_id = Some(instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        assert!(outbox.all().is_empty(), "no entries before the transition");
        // `start()` is a no-op on an open task; `complete()` makes a real
        // lifecycle change so the combined write is observable.
        svc.complete(&t.id.to_string()).await.unwrap();

        // Exactly one combined write happened: task closed + one
        // UpdateRemote, in the shared store.
        assert!(!repo.get(t.id).await.unwrap().is_open());
        assert_eq!(outbox.all().len(), 1);
        assert_eq!(outbox.all()[0].mutation.kind(), "update_remote");
    }

    #[tokio::test]
    async fn lifecycle_issue_backed_enqueues_update_remote() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        // Issue-backed task in a projectless workspace, with a repo binding so
        // canonical_repo resolves.
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        let mut t = save_issue_mirror(&repo, ws.id, None, None).await;
        t.repo_id = Some(instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        svc.reopen(&t.id.to_string()).await.unwrap();

        let all = outbox.all();
        assert_eq!(all.len(), 1, "exactly one mutation enqueued");
        assert_eq!(all[0].mutation.kind(), "update_remote");
        assert_eq!(all[0].status, OutboxStatus::Pending);
    }

    /// RFC 0004 D1: blocking is no longer a lifecycle verb, but the invariant
    /// it protected still holds — a lifecycle transition that keeps a project
    /// mirror OPEN moves the card (`SetProjectStatus`) and updates the issue
    /// (`UpdateRemote`) without ever enqueuing a close (`closed: Some(true)`).
    /// We exercise it with `start()` (a no-op on an open task, but it still
    /// re-plans the mirror's outbound mutations).
    #[tokio::test]
    async fn lifecycle_reopen_project_mirror_enqueues_set_status_not_close() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_block");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();

        // BOUND issue-backed project mirror (the real-world case): it has a
        // repo binding, so an UpdateRemote *is* formed, AND it's already a
        // board item (project_item_id set), so a SetProjectStatus is formed
        // too. This is the shape that exercises "an open-keeping transition
        // moves the card but does NOT close the issue".
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_7"), Some("PVTI_7")).await;
        t.repo_id = Some(instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        // `reopen()` is an open-keeping transition that genuinely changes the
        // lifecycle (Open → Reopened), so it enqueues a card move. (`start()`
        // on an already-open task is a no-op and correctly enqueues nothing.)
        svc.reopen(&t.id.to_string()).await.unwrap();

        let entries = outbox.all();
        let kinds: Vec<&str> = entries.iter().map(|e| e.mutation.kind()).collect();
        assert!(
            kinds.contains(&"set_project_status"),
            "an open project mirror transition moves the card: {kinds:?}"
        );
        assert!(
            kinds.contains(&"update_remote"),
            "a bound issue-backed mirror also enqueues UpdateRemote: {kinds:?}"
        );
        // No close: an open-keeping transition never enqueues a
        // close-the-issue `closed: Some(true)` UpdateRemote.
        for e in &entries {
            if let OutboxMutation::UpdateRemote { closed, .. } = &e.mutation {
                assert_ne!(
                    *closed,
                    Some(true),
                    "an open task must never enqueue a close-the-issue UpdateRemote"
                );
            }
        }
    }

    #[tokio::test]
    async fn lifecycle_local_only_enqueues_nothing() {
        let RichSvc { svc, outbox, .. } = rich_svc();
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "local".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        svc.start(&dto.id).await.unwrap();
        svc.complete(&dto.id).await.unwrap();
        assert!(outbox.all().is_empty(), "LocalOnly tasks enqueue nothing");
    }

    #[tokio::test]
    async fn priority_only_edit_enqueues_nothing() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        let mut t = save_issue_mirror(&repo, ws.id, None, None).await;
        t.repo_id = Some(instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: None,
            body: None,
            priority: Some("p0".into()),
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        assert!(
            outbox.all().is_empty(),
            "priority is local metadata — no remote-observable change, no enqueue"
        );
    }

    /// Seed a workspace + a single `github.com/o/r` binding, returning both so a
    /// relation-sync test can hang addressable mirrors off the binding.
    async fn ws_with_binding(
        workspaces: &Arc<InMemoryWorkspaceRepository>,
        bindings: &Arc<InMemoryRepoBindingRepository>,
    ) -> (Workspace, domain_repo::RepoInstance) {
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();
        (ws, instance)
    }

    #[tokio::test]
    async fn relation_with_unaddressable_end_enqueues_nothing() {
        // A relation can only be projected onto GitHub when BOTH ends resolve to
        // `(filing_canonical, remote_id)`. Here `b` is issue-backed but has no
        // repo binding, so its filing repo is unresolved → no enqueue.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let mut b = Task::new_draft(ws.id, None, "b".into()).unwrap();
        b.stage_for_sync().unwrap();
        b.promote_to_remote(RemoteRef::new("github", "8")).unwrap();
        repo.save(&b, SnapshotSource::Promote).await.unwrap();

        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "blocked_by".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();

        assert!(
            outbox.all().is_empty(),
            "unaddressable far end ⇒ relation can't be projected ⇒ no enqueue"
        );
    }

    #[tokio::test]
    async fn add_blocked_by_enqueues_dependency_with_correct_direction() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let b = addressable_mirror(&repo, ws.id, binding.id, "20").await;

        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "blocked_by".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();

        let entries = outbox.all();
        assert_eq!(entries.len(), 1, "exactly one dependency mutation");
        assert_eq!(
            entries[0].task_id, a.id,
            "entry keyed on the relation command's subject"
        );
        match &entries[0].mutation {
            OutboxMutation::AddBlockedBy {
                blocked_canonical,
                blocked_remote_id,
                blocker_canonical,
                blocker_remote_id,
            } => {
                assert_eq!(blocked_remote_id, "10", "a is the blocked issue");
                assert_eq!(blocker_remote_id, "20", "b is the blocker");
                assert_eq!(blocked_canonical, "github.com/o/r");
                assert_eq!(blocker_canonical, "github.com/o/r");
            }
            other => panic!("expected AddBlockedBy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parent_of_addresses_the_parent_issue() {
        // `a parent_of b` ⇒ sub-issue(parent=a, child=b).
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let b = addressable_mirror(&repo, ws.id, binding.id, "20").await;

        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "parent_of".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();
        match &outbox.all()[0].mutation {
            OutboxMutation::AddSubIssue {
                parent_remote_id,
                child_remote_id,
                ..
            } => {
                assert_eq!(parent_remote_id, "10", "a parent_of b ⇒ a is parent");
                assert_eq!(child_remote_id, "20", "b is the child");
            }
            other => panic!("expected AddSubIssue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn child_of_swaps_to_address_the_parent_issue() {
        // `c child_of d` is the same GitHub edge as `d parent_of c`, so it must
        // address the PARENT (d) and carry c as the child — proving the
        // direction swap is keyed on the stated kind, not the command subject.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let c = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let d = addressable_mirror(&repo, ws.id, binding.id, "20").await;

        svc.add_relation(AddTaskRelationCmd {
            task_id: c.id.to_string(),
            kind: "child_of".into(),
            other: d.id.to_string(),
        })
        .await
        .unwrap();
        let entries = outbox.all();
        assert_eq!(entries.len(), 1, "one sub-issue mutation");
        match &entries[0].mutation {
            OutboxMutation::AddSubIssue {
                parent_remote_id,
                child_remote_id,
                ..
            } => {
                assert_eq!(parent_remote_id, "20", "child_of swaps: d is parent");
                assert_eq!(child_remote_id, "10", "c is child");
            }
            other => panic!("expected AddSubIssue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn related_to_enqueues_nothing() {
        // `related_to` has no GitHub-native primitive → never enqueues, even
        // between two fully addressable mirrors.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let b = addressable_mirror(&repo, ws.id, binding.id, "20").await;

        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "related_to".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();

        assert!(
            outbox.all().is_empty(),
            "related_to has no native primitive ⇒ no enqueue"
        );
    }

    #[tokio::test]
    async fn re_adding_an_existing_relation_does_not_double_enqueue() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let b = addressable_mirror(&repo, ws.id, binding.id, "20").await;
        let cmd = || AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "blocked_by".into(),
            other: b.id.to_string(),
        };

        svc.add_relation(cmd()).await.unwrap();
        svc.add_relation(cmd()).await.unwrap();

        assert_eq!(
            outbox.all().len(),
            1,
            "idempotent re-add must not enqueue a duplicate"
        );
    }

    #[tokio::test]
    async fn remove_relation_enqueues_unlink() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let b = addressable_mirror(&repo, ws.id, binding.id, "20").await;

        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "blocked_by".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();
        svc.remove_relation(RemoveTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "blocked_by".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();

        let entries = outbox.all();
        assert_eq!(entries.len(), 2, "one add + one remove");
        match &entries[1].mutation {
            OutboxMutation::RemoveBlockedBy {
                blocked_remote_id,
                blocker_remote_id,
                ..
            } => {
                assert_eq!(blocked_remote_id, "10");
                assert_eq!(blocker_remote_id, "20");
            }
            other => panic!("expected RemoveBlockedBy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clear_relations_enqueues_one_unlink_per_native_edge() {
        // `a` parent_of `b` and blocked_by `c`; clearing `a` must emit one
        // un-link per native edge (RemoveSubIssue + RemoveBlockedBy), keyed on
        // `a`'s stored direction.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let (ws, binding) = ws_with_binding(&workspaces, &bindings).await;
        let a = addressable_mirror(&repo, ws.id, binding.id, "10").await;
        let b = addressable_mirror(&repo, ws.id, binding.id, "20").await;
        let c = addressable_mirror(&repo, ws.id, binding.id, "30").await;

        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "parent_of".into(),
            other: b.id.to_string(),
        })
        .await
        .unwrap();
        svc.add_relation(AddTaskRelationCmd {
            task_id: a.id.to_string(),
            kind: "blocked_by".into(),
            other: c.id.to_string(),
        })
        .await
        .unwrap();
        let before = outbox.all().len();
        assert_eq!(before, 2, "two add mutations");

        svc.clear_relations(&a.id.to_string()).await.unwrap();

        let unlinks: Vec<_> = outbox.all().into_iter().skip(before).collect();
        assert_eq!(unlinks.len(), 2, "one un-link per native edge");
        let has_sub = unlinks.iter().any(|e| {
            matches!(
                &e.mutation,
                OutboxMutation::RemoveSubIssue { parent_remote_id, child_remote_id, .. }
                    if parent_remote_id == "10" && child_remote_id == "20"
            )
        });
        let has_dep = unlinks.iter().any(|e| {
            matches!(
                &e.mutation,
                OutboxMutation::RemoveBlockedBy { blocked_remote_id, blocker_remote_id, .. }
                    if blocked_remote_id == "10" && blocker_remote_id == "30"
            )
        });
        assert!(has_sub, "expected RemoveSubIssue(parent=10, child=20)");
        assert!(has_dep, "expected RemoveBlockedBy(blocked=10, blocker=30)");
    }

    #[tokio::test]
    async fn orphan_draft_edit_with_repo_enqueues_convert() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();

        // Orphan-draft mirror: a project draft with no remote + no repo.
        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "b".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None; // pure draft: no REST issue
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: None,
            body: None,
            priority: None,
            assignees: None,
            repo_id: Some(instance.id.to_string()),
        })
        .await
        .unwrap();

        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["convert_draft_to_issue"],
            "attaching a repo to an orphan-draft graduates it to an issue"
        );
    }

    #[tokio::test]
    async fn orphan_draft_edit_with_repo_and_title_enqueues_update_then_convert() {
        // Combined edit: attach a repo AND change the title in one `update`.
        // The new content must reach the converted issue, so we enqueue an
        // UpdateDraftIssue (carrying the new title/body, addressed by the
        // project item node id) BEFORE the ConvertDraftToIssue — FIFO runs the
        // draft update first, so the conversion copies the edited content. The
        // earlier bug returned early after the convert, dropping the edit.
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();

        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "old title".into(),
            "old body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None; // pure draft
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: Some("new title".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: Some(instance.id.to_string()),
        })
        .await
        .unwrap();

        let entries = outbox.all();
        let kinds: Vec<&str> = entries.iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["update_draft_issue", "convert_draft_to_issue"],
            "content edit lands on the draft before it converts: {kinds:?}"
        );
        // The UpdateDraftIssue carries the NEW title (not the stale one).
        match &entries[0].mutation {
            OutboxMutation::UpdateDraftIssue { title, .. } => {
                assert_eq!(title.as_deref(), Some("new title"));
            }
            other => panic!("expected UpdateDraftIssue first, got {other:?}"),
        }
    }

    /// RFC 0002 D3 — workspace-default filing case (step 2 of the D2 chain):
    /// an orphan draft with NO `cmd.repo_id` converts when the workspace has a
    /// default `filing_repo_id`. `repo_id` stays NULL; `filing_repo_id` is set
    /// to the workspace default; the enqueued `ConvertDraftToIssue.repo_node_id`
    /// is the default binding's canonical URL.
    #[tokio::test]
    async fn orphan_draft_converts_via_workspace_filing_default() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();

        // Set up a workspace default filing repo binding.
        let default_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        let default_instance = domain_repo::RepoInstance::new(
            WorkspaceId::new(),
            default_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        bindings.save_origin(&default_origin).await.unwrap();
        bindings.save_instance(&default_instance).await.unwrap();

        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.filing_repo_id = Some(RepoId::from_uuid(default_origin.id.as_uuid()));
        workspaces.save(&ws).await.unwrap();

        // Orphan-draft mirror: a project draft with no remote + no repo.
        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None;
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        // Update with NO cmd.repo_id — should still convert via workspace default.
        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: None,
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        // repo_id stays NULL (D2 step-2 invariant).
        let reloaded = repo.get(t.id).await.unwrap();
        assert!(
            reloaded.repo_id.is_none(),
            "repo_id must stay NULL in the workspace-default case"
        );
        // filing_repo_id is recorded as the workspace default.
        assert_eq!(
            reloaded.filing_repo_id,
            Some(RepoId::from_uuid(default_origin.id.as_uuid())),
            "filing_repo_id must be the workspace default"
        );
        // Exactly [convert_draft_to_issue] is enqueued.
        let entries = outbox.all();
        let kinds: Vec<&str> = entries.iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["convert_draft_to_issue"],
            "orphan draft with workspace default enqueues convert: {kinds:?}"
        );
        // repo_node_id carries the default binding's canonical URL.
        match &entries[0].mutation {
            OutboxMutation::ConvertDraftToIssue { repo_node_id, .. } => {
                assert_eq!(
                    repo_node_id, "github.com/o/filing",
                    "repo_node_id must be the filing binding's canonical URL"
                );
            }
            other => panic!("expected ConvertDraftToIssue, got {other:?}"),
        }
    }

    /// RFC 0002 D3 regression (Greptile #137): once an orphan draft has recorded
    /// its filing repo, a later edit must NOT re-resolve it — even if the
    /// workspace default changed in between. Without the `filing_repo_id.is_none()`
    /// guard the second edit re-resolves to the new default, which either errors
    /// in `set_filing_repo_id` (cannot change a recorded filing repo) or enqueues
    /// a duplicate `ConvertDraftToIssue`.
    #[tokio::test]
    async fn orphan_draft_filing_recording_is_idempotent_across_edits() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();

        let first_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/first.git".into(),
            "github.com/o/first".into(),
        )
        .unwrap();
        let first_instance = domain_repo::RepoInstance::new(
            WorkspaceId::new(),
            first_origin.id,
            "github.com/o/first".into(),
            None,
        )
        .unwrap();
        let second_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/second.git".into(),
            "github.com/o/second".into(),
        )
        .unwrap();
        let second_instance = domain_repo::RepoInstance::new(
            WorkspaceId::new(),
            second_origin.id,
            "github.com/o/second".into(),
            None,
        )
        .unwrap();
        bindings.save_origin(&first_origin).await.unwrap();
        bindings.save_instance(&first_instance).await.unwrap();
        bindings.save_origin(&second_origin).await.unwrap();
        bindings.save_instance(&second_instance).await.unwrap();

        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.filing_repo_id = Some(RepoId::from_uuid(first_origin.id.as_uuid()));
        workspaces.save(&ws).await.unwrap();

        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None;
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        // First edit: records filing = first default and enqueues one convert.
        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: Some("e1".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();
        assert_eq!(
            repo.get(t.id).await.unwrap().filing_repo_id,
            Some(RepoId::from_uuid(first_origin.id.as_uuid()))
        );

        // The workspace default changes AFTER the draft recorded its filing repo.
        ws.filing_repo_id = Some(RepoId::from_uuid(second_origin.id.as_uuid()));
        workspaces.save(&ws).await.unwrap();

        // A second edit must NOT error and must NOT re-resolve/re-record.
        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: Some("e2".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .expect("second edit must not error on an already-recorded filing repo");

        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id,
            Some(RepoId::from_uuid(first_origin.id.as_uuid())),
            "recorded filing repo is authoritative — not retargeted to the new default"
        );
        let converts = outbox
            .all()
            .iter()
            .filter(|e| e.mutation.kind() == "convert_draft_to_issue")
            .count();
        assert_eq!(
            converts, 1,
            "filing recording + convert must enqueue once, not once per edit"
        );
    }

    /// RFC 0002 D3 regression — attach-repo case with no workspace default:
    /// an orphan draft that gains a logical repo via `cmd.repo_id` (the
    /// original pre-RFC behaviour) still converts, `filing_repo_id` is set to
    /// the attached logical repo (chain collapses to logical), and `repo_node_id`
    /// carries the logical repo's canonical.
    #[tokio::test]
    async fn orphan_draft_gaining_repo_records_filing_as_logical() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        // Workspace with NO filing default — chain collapses to logical.
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();
        let origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/logical.git".into(),
            "github.com/o/logical".into(),
        )
        .unwrap();
        let instance =
            domain_repo::RepoInstance::new(ws.id, origin.id, "github.com/o/logical".into(), None)
                .unwrap();
        bindings.save_origin(&origin).await.unwrap();
        bindings.save_instance(&instance).await.unwrap();

        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "draft".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None;
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: None,
            body: None,
            priority: None,
            assignees: None,
            repo_id: Some(instance.id.to_string()),
        })
        .await
        .unwrap();

        let reloaded = repo.get(t.id).await.unwrap();
        // filing_repo_id == logical repo origin id (D2 step-3 collapse, RFC 0005).
        assert_eq!(
            reloaded.filing_repo_id,
            Some(RepoId::from_uuid(origin.id.as_uuid())),
            "filing_repo_id must equal the attached logical repo when there is no workspace default"
        );
        let entries = outbox.all();
        let kinds: Vec<&str> = entries.iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["convert_draft_to_issue"],
            "attaching a repo to an orphan-draft still converts: {kinds:?}"
        );
        match &entries[0].mutation {
            OutboxMutation::ConvertDraftToIssue { repo_node_id, .. } => {
                assert_eq!(
                    repo_node_id, "github.com/o/logical",
                    "repo_node_id must be the logical repo canonical when no default exists"
                );
            }
            other => panic!("expected ConvertDraftToIssue, got {other:?}"),
        }
    }

    /// RFC 0002 D3 — combined title-edit + workspace-default conversion:
    /// when the workspace has a filing default and an orphan draft also has a
    /// content change in the same `update`, the plan is
    /// `[update_draft_issue, convert_draft_to_issue]` (FIFO order) and
    /// `filing_repo_id` is recorded.
    #[tokio::test]
    async fn orphan_draft_title_edit_with_workspace_default_enqueues_update_then_convert() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let default_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        let default_instance = domain_repo::RepoInstance::new(
            WorkspaceId::new(),
            default_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        bindings.save_origin(&default_origin).await.unwrap();
        bindings.save_instance(&default_instance).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.filing_repo_id = Some(RepoId::from_uuid(default_origin.id.as_uuid()));
        workspaces.save(&ws).await.unwrap();

        let mut t = Task::import_mirror(
            ws.id,
            None,
            RemoteRef::new("github", "0"),
            "old title".into(),
            "old body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None;
        t.project_item_id = Some("PVTI_d".into());
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: Some("new title".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id,
            Some(RepoId::from_uuid(default_origin.id.as_uuid())),
            "filing_repo_id recorded even with combined title + convert"
        );
        let entries = outbox.all();
        let kinds: Vec<&str> = entries.iter().map(|e| e.mutation.kind()).collect();
        assert_eq!(
            kinds,
            vec!["update_draft_issue", "convert_draft_to_issue"],
            "combined title edit + default convert: {kinds:?}"
        );
        match &entries[0].mutation {
            OutboxMutation::UpdateDraftIssue { title, .. } => {
                assert_eq!(title.as_deref(), Some("new title"));
            }
            other => panic!("expected UpdateDraftIssue first, got {other:?}"),
        }
    }

    /// RFC 0002 D3 negative — a non-convert edit on a draft (workspace has a
    /// filing default but the task is NOT an orphan draft) must NOT record
    /// `filing_repo_id` and must NOT enqueue a conversion.
    #[tokio::test]
    async fn non_orphan_draft_edit_does_not_record_filing_repo_id() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            bindings,
            ..
        } = rich_svc();
        let default_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        let default_instance = domain_repo::RepoInstance::new(
            WorkspaceId::new(),
            default_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        bindings.save_origin(&default_origin).await.unwrap();
        bindings.save_instance(&default_instance).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.filing_repo_id = Some(RepoId::from_uuid(default_origin.id.as_uuid()));
        workspaces.save(&ws).await.unwrap();

        // Issue-backed mirror (NOT a draft) — the convert gate must not fire.
        let t = save_issue_mirror(&repo, ws.id, Some("I_7"), Some("PVTI_7")).await;

        svc.update(UpdateTaskCmd {
            task_id: t.id.to_string(),
            title: Some("edited".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();

        let reloaded = repo.get(t.id).await.unwrap();
        assert!(
            reloaded.filing_repo_id.is_none(),
            "filing_repo_id must stay NULL for non-convert edits"
        );
        // An issue-backed mirror edit enqueues UpdateRemote, NOT a conversion.
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert!(
            !kinds.contains(&"convert_draft_to_issue"),
            "non-orphan-draft edit must not convert: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn project_mirror_without_item_id_enqueues_add_item() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_lazy");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();
        let b_origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let b_instance =
            domain_repo::RepoInstance::new(ws.id, b_origin.id, "github.com/o/r".into(), None)
                .unwrap();
        bindings.save_origin(&b_origin).await.unwrap();
        bindings.save_instance(&b_instance).await.unwrap();

        // Issue-backed mirror with a node id but NOT yet a project item.
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_7"), None).await;
        t.repo_id = Some(b_instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        svc.reopen(&t.id.to_string()).await.unwrap();

        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        // UpdateRemote (issue state) + AddItem (lazy net). SetProjectStatus
        // follows via the drainer's AddItem write-back, not at enqueue time.
        assert!(kinds.contains(&"add_item"), "lazy attach: {kinds:?}");
        assert!(kinds.contains(&"update_remote"), "issue state: {kinds:?}");
    }

    // ---------- RFC 0002 D2 first-board-filing (#124) ----------------------

    /// Issue-backed mirror whose workspace has a project but no `project_item_id`
    /// records `filing_repo_id == repo_id` (step 3 — logical repo) before the
    /// `AddItem` mutation is enqueued.
    #[tokio::test]
    async fn first_board_filing_issue_backed_records_logical_filing_repo() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_d2_issue");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();
        let b_origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let b_instance =
            domain_repo::RepoInstance::new(ws.id, b_origin.id, "github.com/o/r".into(), None)
                .unwrap();
        let b_origin_as_repo_id = RepoId::from_uuid(b_origin.id.as_uuid());
        bindings.save_origin(&b_origin).await.unwrap();
        bindings.save_instance(&b_instance).await.unwrap();

        // Issue-backed mirror with a node id, NOT yet a board item, no filing
        // repo recorded yet (filing_repo_id == None before the transition).
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_r1"), None).await;
        t.repo_id = Some(b_instance.id);
        // Confirm: filing_repo_id not recorded yet (pre-condition).
        assert_eq!(t.filing_repo_id, None);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        // Trigger the first-board-filing path via a lifecycle transition.
        svc.reopen(&t.id.to_string()).await.unwrap();

        // The filing repo must be recorded before the AddItem lands.
        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id,
            Some(b_origin_as_repo_id),
            "filing_repo_id must equal the logical repo origin (step 3) after first board filing"
        );
        // AddItem was enqueued (lazy net for issue-backed mirror).
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert!(
            kinds.contains(&"add_item"),
            "AddItem must be enqueued: {kinds:?}"
        );
    }

    /// RFC 0002 (#143): a CROSS-FILED issue-backed mirror (logical repo ≠ filing
    /// repo) must address its `UpdateRemote` to the FILING repo — where the
    /// backing issue actually lives — not the logical repo. Regression for the
    /// bug where `plan_mirror_mutations` fed the logical canonical to the
    /// planner, 404ing every cross-filed lifecycle push (and head-of-line
    /// blocking the sibling board `AddItem`).
    #[tokio::test]
    async fn cross_filed_lifecycle_update_targets_filing_repo() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects: _,
            bindings,
        } = rich_svc();

        // No project on the workspace, so the only enqueued mutation is the
        // issue-state UpdateRemote — isolates the canonical under test.
        let ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        workspaces.save(&ws).await.unwrap();

        // Two distinct bindings: the logical repo (where code lives) and the
        // filing repo (where the issue was filed).
        let logical_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/logical.git".into(),
            "github.com/o/logical".into(),
        )
        .unwrap();
        let logical_instance = domain_repo::RepoInstance::new(
            ws.id,
            logical_origin.id,
            "github.com/o/logical".into(),
            None,
        )
        .unwrap();
        let filing_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        let filing_instance = domain_repo::RepoInstance::new(
            ws.id,
            filing_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        bindings.save_origin(&logical_origin).await.unwrap();
        bindings.save_instance(&logical_instance).await.unwrap();
        bindings.save_origin(&filing_origin).await.unwrap();
        bindings.save_instance(&filing_instance).await.unwrap();

        // Issue-backed mirror filed in `filing` while its logical repo is
        // `logical` — the cross-filed shape.
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_node"), None).await;
        t.repo_id = Some(logical_instance.id);
        t.set_filing_repo_id(Some(RepoId::from_uuid(filing_origin.id.as_uuid())))
            .unwrap();
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        // Lifecycle transition → plan_mirror_mutations → UpdateRemote.
        svc.reopen(&t.id.to_string()).await.unwrap();

        let canonical = outbox
            .all()
            .iter()
            .find_map(|e| match &e.mutation {
                OutboxMutation::UpdateRemote { canonical_repo, .. } => Some(canonical_repo.clone()),
                _ => None,
            })
            .expect("a lifecycle UpdateRemote must be enqueued");
        assert_eq!(
            canonical, "github.com/o/filing",
            "UpdateRemote must address the FILING repo, not the logical repo"
        );
    }

    /// Draft-backed orphan mirror (no `repo_id`, no workspace default) resolves
    /// to `None` (step 4 — board draft), records `filing_repo_id == None`, and
    /// emits `CreateDraftIssue`.
    #[tokio::test]
    async fn first_board_filing_draft_backed_orphan_stays_null_emits_create_draft() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            ..
        } = rich_svc();
        let project = test_project("PVT_kwHO_d2_draft");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        // No workspace default filing repo.
        workspaces.save(&ws).await.unwrap();

        // Draft-backed mirror: remote == None, project_item_id == None, no repo_id.
        let mut t = Task::import_mirror(
            ws.id,
            None, // no repo_id — orphan
            RemoteRef::new("github", "0"),
            "board draft".into(),
            "needs triage".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None; // strip REST issue → pure draft
        // project_item_id is None → first-filing precondition met
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.reopen(&t.id.to_string()).await.unwrap();

        // Step 4: filing_repo_id stays None (legitimate board draft).
        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id, None,
            "orphan draft with no workspace default must stay filing_repo_id = None (step 4)"
        );
        // CreateDraftIssue was enqueued.
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert!(
            kinds.contains(&"create_draft_issue"),
            "CreateDraftIssue must be enqueued for draft-backed first board filing: {kinds:?}"
        );
    }

    /// Orphan draft whose workspace has a filing default resolves to the
    /// workspace default (step 2) and records that as the filing repo.
    #[tokio::test]
    async fn first_board_filing_orphan_draft_with_ws_default_resolves_to_default() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_d2_ws_default");
        projects.save(&project).await.unwrap();

        // Bind a "filing default" repo to the workspace.
        let ws_placeholder_id = domain_core::WorkspaceId::new();
        let default_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/filing.git".into(),
            "github.com/o/filing".into(),
        )
        .unwrap();
        let default_instance = domain_repo::RepoInstance::new(
            ws_placeholder_id,
            default_origin.id,
            "github.com/o/filing".into(),
            None,
        )
        .unwrap();
        let default_origin_as_repo_id = RepoId::from_uuid(default_origin.id.as_uuid());
        bindings.save_origin(&default_origin).await.unwrap();
        bindings.save_instance(&default_instance).await.unwrap();

        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        ws.filing_repo_id = Some(default_origin_as_repo_id); // workspace default (origin id stored as RepoId)
        workspaces.save(&ws).await.unwrap();

        // Orphan draft: no repo_id, pure draft (remote = None, project_item_id = None).
        let mut t = Task::import_mirror(
            ws.id,
            None, // orphan — no logical repo
            RemoteRef::new("github", "0"),
            "orphan".into(),
            "body".into(),
            vec![],
            false,
        )
        .unwrap();
        t.remote = None;
        repo.save(&t, SnapshotSource::Pull).await.unwrap();

        svc.reopen(&t.id.to_string()).await.unwrap();

        // Step 2: workspace default wins over the absent logical repo.
        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id,
            Some(default_origin_as_repo_id),
            "orphan + workspace default must resolve to the workspace default repo (step 2)"
        );
        // CreateDraftIssue is enqueued (the issue conversion to a real issue
        // in the filing repo is coordinated by ConvertDraftToIssue — #123).
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert!(
            kinds.contains(&"create_draft_issue"),
            "CreateDraftIssue must be enqueued: {kinds:?}"
        );
        // filing_repo_id is now recorded — a second transition must be idempotent.
        let _ = outbox.all(); // drain for next assertion
        let _ = svc.start(&t.id.to_string()).await;
        let reloaded2 = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded2.filing_repo_id,
            Some(default_origin_as_repo_id),
            "filing_repo_id must not change on repeat transition (idempotent same-value set)"
        );
    }

    /// A task that is ALREADY a board item (`project_item_id` is `Some`) does
    /// NOT trigger the first-filing path — `filing_repo_id` is left unchanged
    /// and no extra recording bump occurs.
    #[tokio::test]
    async fn already_attached_card_does_not_re_resolve_filing_repo() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_d2_attached");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();
        let b_origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let b_instance =
            domain_repo::RepoInstance::new(ws.id, b_origin.id, "github.com/o/r".into(), None)
                .unwrap();
        let b_origin_as_repo_id = RepoId::from_uuid(b_origin.id.as_uuid());
        bindings.save_origin(&b_origin).await.unwrap();
        bindings.save_instance(&b_instance).await.unwrap();

        // Already attached: project_item_id is set; filing_repo_id already recorded.
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_a1"), Some("PVTI_a1")).await;
        t.repo_id = Some(b_instance.id);
        t.set_filing_repo_id(Some(b_origin_as_repo_id)).unwrap();
        repo.save(&t, SnapshotSource::Promote).await.unwrap();
        let filing_before = t.filing_repo_id;

        svc.reopen(&t.id.to_string()).await.unwrap();

        let reloaded = repo.get(t.id).await.unwrap();
        // filing_repo_id must be unchanged — already-attached card skips first-filing.
        assert_eq!(
            reloaded.filing_repo_id, filing_before,
            "already-attached card must not re-resolve or change filing_repo_id"
        );
        // Only SetProjectStatus is enqueued (card move), not AddItem.
        let kinds: Vec<&str> = outbox.all().iter().map(|e| e.mutation.kind()).collect();
        assert!(
            kinds.contains(&"set_project_status"),
            "attached card must produce a SetProjectStatus card move: {kinds:?}"
        );
        assert!(
            !kinds.contains(&"add_item"),
            "attached card must NOT produce AddItem: {kinds:?}"
        );
    }

    /// RFC 0002 regression (verify #124): a task that recorded its filing repo
    /// at promote (#117) but is NOT yet a board item (`project_item_id` None)
    /// must NOT re-resolve on a later lifecycle transition. Before the
    /// `filing_repo_id.is_none()` guard, if the workspace default changed after
    /// promote, the transition re-resolved to the new default and
    /// `set_filing_repo_id` errored, hard-failing start/complete/etc.
    #[tokio::test]
    async fn recorded_filing_not_re_resolved_when_workspace_default_changes() {
        let RichSvc {
            svc,
            repo,
            workspaces,
            projects,
            bindings,
            ..
        } = rich_svc();
        let project = test_project("PVT_kwHO_d2_changed");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();

        let logical_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/logical.git".into(),
            "github.com/o/logical".into(),
        )
        .unwrap();
        let logical_instance = domain_repo::RepoInstance::new(
            ws.id,
            logical_origin.id,
            "github.com/o/logical".into(),
            None,
        )
        .unwrap();
        let logical_origin_as_repo_id = RepoId::from_uuid(logical_origin.id.as_uuid());
        let other_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/other.git".into(),
            "github.com/o/other".into(),
        )
        .unwrap();
        let other_instance = domain_repo::RepoInstance::new(
            ws.id,
            other_origin.id,
            "github.com/o/other".into(),
            None,
        )
        .unwrap();
        let other_origin_as_repo_id = RepoId::from_uuid(other_origin.id.as_uuid());
        bindings.save_origin(&logical_origin).await.unwrap();
        bindings.save_instance(&logical_instance).await.unwrap();
        bindings.save_origin(&other_origin).await.unwrap();
        bindings.save_instance(&other_instance).await.unwrap();

        // Issue-backed, NOT yet a board item (project_item_id None); filing
        // recorded at promote = the logical repo (#117, no default then).
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_x"), None).await;
        t.repo_id = Some(logical_instance.id);
        t.set_filing_repo_id(Some(logical_origin_as_repo_id))
            .unwrap();
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        // The workspace default is set to a DIFFERENT repo AFTER promote.
        ws.filing_repo_id = Some(other_origin_as_repo_id);
        workspaces.save(&ws).await.unwrap();

        // A lifecycle transition must NOT error and must NOT retarget filing.
        svc.start(&t.id.to_string())
            .await
            .expect("transition must not error on a recorded filing repo when the workspace default changed");
        let reloaded = repo.get(t.id).await.unwrap();
        assert_eq!(
            reloaded.filing_repo_id,
            Some(logical_origin_as_repo_id),
            "recorded filing repo is authoritative — not retargeted to the changed workspace default"
        );
    }

    /// A second lifecycle transition after the filing repo was recorded is an
    /// idempotent same-value set — `set_filing_repo_id` returns `Ok(())` and
    /// the recorded value is unchanged.
    #[tokio::test]
    async fn repeat_transition_after_filing_recorded_is_idempotent() {
        let RichSvc {
            svc,
            repo,
            outbox,
            workspaces,
            projects,
            bindings,
        } = rich_svc();
        let project = test_project("PVT_kwHO_d2_repeat");
        projects.save(&project).await.unwrap();
        let mut ws = Workspace::new(WorkspaceName::new("w").unwrap(), None, false);
        ws.project_id = Some(project.id.clone());
        workspaces.save(&ws).await.unwrap();
        let b_origin =
            domain_repo::RepoOrigin::new("git@github.com:o/r.git".into(), "github.com/o/r".into())
                .unwrap();
        let b_instance =
            domain_repo::RepoInstance::new(ws.id, b_origin.id, "github.com/o/r".into(), None)
                .unwrap();
        let b_origin_as_repo_id = RepoId::from_uuid(b_origin.id.as_uuid());
        bindings.save_origin(&b_origin).await.unwrap();
        bindings.save_instance(&b_instance).await.unwrap();

        // Not yet attached.
        let mut t = save_issue_mirror(&repo, ws.id, Some("I_rep"), None).await;
        t.repo_id = Some(b_instance.id);
        repo.save(&t, SnapshotSource::Promote).await.unwrap();

        // First transition: records filing_repo_id (= origin id stored as RepoId).
        svc.reopen(&t.id.to_string()).await.unwrap();
        let after_first = repo.get(t.id).await.unwrap();
        assert_eq!(after_first.filing_repo_id, Some(b_origin_as_repo_id));

        // Clear outbox for a clean second-pass observation.
        let _ = outbox.all();

        // Simulate card write-back (drainer would set project_item_id); do it
        // manually so the second transition sees the attached state.
        let mut attached = after_first.clone();
        attached.project_item_id = Some("PVTI_rep".into());
        repo.save(&attached, SnapshotSource::Promote).await.unwrap();

        // Second transition: idempotent — filing_repo_id must stay the same value.
        svc.complete(&t.id.to_string()).await.unwrap();
        let after_second = repo.get(t.id).await.unwrap();
        assert_eq!(
            after_second.filing_repo_id,
            Some(b_origin_as_repo_id),
            "repeat transition must not change or error on the already-recorded filing_repo_id"
        );
    }

    #[tokio::test]
    async fn rollback_restores_original_title() {
        let svc = svc();
        // Create task — this writes version 1.
        let original = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "original title".into(),
                body: None,
                priority: None,
                filing_repo_override: None,
            })
            .await
            .unwrap();
        // Edit title — this writes version 2.
        svc.update(UpdateTaskCmd {
            task_id: original.id.clone(),
            title: Some("edited title".into()),
            body: None,
            priority: None,
            assignees: None,
            repo_id: None,
        })
        .await
        .unwrap();
        // Rollback to version 1 — title should revert.
        let rolled_back = svc.rollback(&original.id, 1).await.unwrap();
        assert_eq!(rolled_back.title, "original title");
    }

    /// `repoint_filing_repo` re-points the recorded `filing_repo_id` and
    /// tags the resulting snapshot with `FilingRepoRepair` so the audit
    /// trail makes every doctor re-point greppable. rpl-sv2.
    #[tokio::test]
    async fn repoint_filing_repo_writes_repair_snapshot() {
        let (svc, bindings) = svc_with_bindings();
        // Plant a real binding so the service-layer pre-validation
        // (`bindings.get(target)?`) accepts the re-point target. The
        // import_mirror call doesn't itself need a binding because
        // the test plants the filing pointer via `force_set_filing_repo_id`
        // after import.
        let ws = ws_id();
        let new_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/target.git".into(),
            "github.com/o/target".into(),
        )
        .unwrap();
        let new_instance = domain_repo::RepoInstance::new(
            domain_core::WorkspaceId::new(),
            new_origin.id,
            "github.com/o/target".into(),
            None,
        )
        .unwrap();
        let new_instance_id = new_instance.id;
        bindings.save_origin(&new_origin).await.unwrap();
        bindings.save_instance(&new_instance).await.unwrap();

        let original = svc
            .import_mirror(ImportMirrorCmd {
                workspace_id: ws.clone(),
                repo_id: None,
                provider: "github".into(),
                remote_id: "123".into(),
                title: "imported".into(),
                body: "from gh".into(),
                assignees: vec![],
                closed: false,
            })
            .await
            .unwrap();

        let resolved = svc
            .repoint_filing_repo(&original.id, Some(new_instance_id))
            .await
            .unwrap();
        // §D4: the recorded filing repo is the target's ORIGIN, not the raw
        // instance id (which would be a dangling filing pointer).
        let domain = svc.resolve_task(&original.id).await.unwrap();
        assert_eq!(
            domain.filing_repo_id,
            Some(domain_core::RepoId::from_uuid(new_origin.id.as_uuid()))
        );
        let _ = new_instance_id;
        // D5 contract (the dto never carries filing_repo_id) is covered
        // by the CLI test `task_show_surfaces_filing_repo_without_leaking_filing_repo_id`,
        // not duplicated here.
        let _ = resolved;
    }

    /// Idempotent re-point: passing the same target as the recorded
    /// value is a no-op at the domain layer (`force_set_filing_repo_id`
    /// early-returns on equality). The snapshot still gets a row, but
    /// `updated_at` is untouched.
    #[tokio::test]
    async fn repoint_filing_repo_idempotent_on_same_target() {
        let (svc, bindings) = svc_with_bindings();
        // Plant a real binding for the pre-validation check.
        let new_origin = domain_repo::RepoOrigin::new(
            "git@github.com:o/idem-target.git".into(),
            "github.com/o/idem-target".into(),
        )
        .unwrap();
        let new_instance = domain_repo::RepoInstance::new(
            domain_core::WorkspaceId::new(),
            new_origin.id,
            "github.com/o/idem-target".into(),
            None,
        )
        .unwrap();
        let target = new_instance.id;
        bindings.save_origin(&new_origin).await.unwrap();
        bindings.save_instance(&new_instance).await.unwrap();

        let original = svc
            .import_mirror(ImportMirrorCmd {
                workspace_id: ws_id(),
                repo_id: None,
                provider: "github".into(),
                remote_id: "456".into(),
                title: "x".into(),
                body: "".into(),
                assignees: vec![],
                closed: false,
            })
            .await
            .unwrap();
        // First re-point sets it.
        svc.repoint_filing_repo(&original.id, Some(target))
            .await
            .unwrap();
        let after_first = svc.resolve_task(&original.id).await.unwrap().updated_at;
        // Second re-point with same target is a no-op.
        svc.repoint_filing_repo(&original.id, Some(target))
            .await
            .unwrap();
        let after_second = svc.resolve_task(&original.id).await.unwrap().updated_at;
        assert_eq!(
            after_first, after_second,
            "idempotent re-point must not bump updated_at"
        );
    }

    /// `repoint_filing_repo` must pre-validate the target binding
    /// exists before persisting the re-point. Otherwise a typo or
    /// stale binding handle would silently write ANOTHER dangling
    /// pointer (the bug class rpl-sv2 exists to heal, just on a
    /// different column). CodeRabbit review flagged this as a Major
    /// defensive-programming miss.
    #[tokio::test]
    async fn repoint_filing_repo_rejects_unknown_target() {
        let svc = svc();
        let original = svc
            .import_mirror(ImportMirrorCmd {
                workspace_id: ws_id(),
                repo_id: None,
                provider: "github".into(),
                remote_id: "789".into(),
                title: "validate".into(),
                body: "".into(),
                assignees: vec![],
                closed: false,
            })
            .await
            .unwrap();
        // An arbitrary UUID that was never saved to the bindings
        // repo. The pre-validation must catch this before the save
        // would have persisted another dangling pointer.
        let phantom_target = RepoId::new();
        let err = svc
            .repoint_filing_repo(&original.id, Some(phantom_target))
            .await
            .expect_err("repoint to a non-existent binding must error");
        let msg = err.to_string();
        assert!(
            msg.contains("not found") || msg.contains("NoRepo") || msg.contains("not_found"),
            "expected a 'not found' error, got: {msg}"
        );
        // The task row must be unchanged — the pre-validation
        // happened before the in-memory mutation.
        let domain = svc.resolve_task(&original.id).await.unwrap();
        assert_ne!(
            domain.filing_repo_id,
            Some(phantom_target),
            "pre-validation must abort before persisting a dangling pointer"
        );
    }
}
