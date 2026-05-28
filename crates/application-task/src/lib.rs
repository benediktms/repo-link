//! application-task — Task CRUD + lifecycle orchestration.

use std::sync::Arc;

use domain_core::{IdParseError, RepoId, TaskId, Timestamp, WorkspaceId};
use domain_task::{
    Priority, RelationKind, RemoteRef, SnapshotSource, SyncState, Task, TaskStatus,
};
use dto_shared::{
    AddTaskRelationCmd, CreateTaskCmd, ImportMirrorCmd, ListTasksQuery, RemoteRefDto,
    TaskCommentDto, TaskDto, TaskRelationDto, UpdateTaskCmd,
};
use ports::{
    PortError, RepoBindingRepository, TaskFilter, TaskRepository, TaskSnapshotRepository,
};
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Domain(#[from] domain_core::DomainError),
    #[error("invalid id: {0}")]
    BadId(String),
    #[error("invalid enum value for {field}: {value}")]
    BadEnum { field: &'static str, value: String },
    /// Composite ID input named one prefix but the task's repo carries
    /// a different one. The bare hash is unique, so we *could* resolve
    /// it silently; the spec explicitly rejects that path because the
    /// mismatch usually indicates a stale copy-paste from another
    /// repo's context.
    #[error(
        "prefix mismatch: input '{input_prefix}-{hash}' but task {hash} lives under prefix '{actual_prefix}'"
    )]
    PrefixMismatch {
        input_prefix: String,
        actual_prefix: String,
        hash: String,
    },
}

impl From<IdParseError> for ServiceError {
    fn from(e: IdParseError) -> Self {
        Self::BadId(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

// ---------- TaskService ---------------------------------------------------

pub struct TaskService {
    repo: Arc<dyn TaskRepository>,
    snapshots: Arc<dyn TaskSnapshotRepository>,
    /// Used by the friendly-ID resolver to validate the prefix half of
    /// a composite `prefix-hash` input against the task's repo binding.
    bindings: Arc<dyn RepoBindingRepository>,
}

impl TaskService {
    pub fn new(
        repo: Arc<dyn TaskRepository>,
        snapshots: Arc<dyn TaskSnapshotRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
    ) -> Self {
        Self {
            repo,
            snapshots,
            bindings,
        }
    }

    pub fn snapshots_repo(&self) -> &Arc<dyn TaskSnapshotRepository> {
        &self.snapshots
    }

    /// Resolve a friendly task ID and list its snapshot history.
    /// Exists so the CLI can stay friendly-ID-aware without having to
    /// reach into [`snapshots_repo`] and parse a UUID itself.
    pub async fn list_snapshots(
        &self,
        query: &str,
    ) -> Result<Vec<domain_task::TaskSnapshot>> {
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
        Ok(Some(self.bindings.get(repo_id).await?.prefix))
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
        Ok(dto)
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
                Some(repo_id) => self.bindings.get(repo_id).await?.prefix,
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
            RemoteRef {
                provider: cmd.provider,
                remote_id: cmd.remote_id,
            },
            cmd.title,
            cmd.body,
            cmd.assignees,
            cmd.closed,
        )?;
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
            t.set_repo_id(Some(repo_id.parse::<RepoId>()?))?;
        }
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
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
            status: query
                .status
                .as_deref()
                .map(|s| parse_enum::<TaskStatus>("status", s))
                .transpose()?,
            sync_state: query
                .sync_state
                .as_deref()
                .map(|s| parse_enum::<SyncState>("sync_state", s))
                .transpose()?,
            include_archived: query.include_archived,
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
        self.transition(id, |t| t.stage_for_sync()).await
    }

    // ---------- Lifecycle transitions ------------------------------------

    pub async fn start(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.start()).await
    }

    pub async fn complete(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.complete()).await
    }

    pub async fn reopen(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.reopen()).await
    }

    pub async fn mark_blocked(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.mark_blocked()).await
    }

    pub async fn unblock(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.unblock()).await
    }

    pub async fn archive(&self, id: &str) -> Result<TaskDto> {
        self.transition(id, |t| t.archive()).await
    }

    pub async fn add_relation(&self, cmd: AddTaskRelationCmd) -> Result<TaskDto> {
        let kind = parse_enum::<RelationKind>("kind", &cmd.kind)?;
        let mut t = self.resolve_task(&cmd.task_id).await?;
        // The other side of a relation is a friendly ID too.
        let other_task = self.resolve_task(&cmd.other).await?;
        t.add_relation(kind, other_task.id);
        self.repo.save(&t, SnapshotSource::LocalEdit).await?;
        self.task_dto(&t).await
    }

    pub async fn rollback(&self, id: &str, to_version: u64) -> Result<TaskDto> {
        let mut task = self.resolve_task(id).await?;
        let snapshot = self.snapshots.get(task.id, to_version).await?;
        task.title = snapshot.title;
        task.body = snapshot.body;
        task.status = snapshot.status;
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
}

// ---------- Mapping ------------------------------------------------------

fn enum_str<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn parse_enum<T: DeserializeOwned>(field: &'static str, value: &str) -> Result<T> {
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
fn assemble_task_display_id(t: &Task, prefix: Option<&str>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ports::TaskSnapshotRepository;
    use testing_fixtures::{
        InMemoryRepoBindingRepository, InMemoryTaskRepository, InMemoryTaskSnapshotRepository,
    };

    fn svc() -> TaskService {
        let repo = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        TaskService::new(repo, snaps, bindings)
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
        assert_eq!(dto.status, "open");
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
            })
            .await
            .unwrap();
        assert_eq!(dto.status, "open");
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
    async fn invalid_priority_returns_typed_error() {
        let svc = svc();
        let err = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: Some("p99".into()),
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
            })
            .await
            .unwrap();
        let started = svc.start(&t.id).await.unwrap();
        assert_eq!(started.status, "in_progress");
        let done = svc.complete(&t.id).await.unwrap();
        assert_eq!(done.status, "done");
        let archived = svc.archive(&t.id).await.unwrap();
        assert_eq!(archived.status, "archived");
    }

    #[tokio::test]
    async fn block_and_unblock() {
        let svc = svc();
        let t = svc
            .create(CreateTaskCmd {
                workspace_id: ws_id(),
                repo_id: None,
                title: "t".into(),
                body: None,
                priority: None,
            })
            .await
            .unwrap();
        let blocked = svc.mark_blocked(&t.id).await.unwrap();
        assert_eq!(blocked.status, "blocked");
        let unblocked = svc.unblock(&t.id).await.unwrap();
        assert_eq!(unblocked.status, "open");
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
        use domain_repo::RepoBinding;
        use ports::RepoBindingRepository;

        let repo = Arc::new(InMemoryTaskRepository::new());
        let snaps: Arc<dyn TaskSnapshotRepository> =
            Arc::new(InMemoryTaskSnapshotRepository::linked_to(&repo));
        let bindings = Arc::new(InMemoryRepoBindingRepository::new());

        // Seed a binding with a known prefix so the created task's id is a
        // real `prefix-hash` composite (not the bare-hash fallback).
        let ws = WorkspaceId::new();
        let mut binding = RepoBinding::new(
            ws,
            "git@github.com:o/widget.git".into(),
            "github.com/o/widget".into(),
        )
        .unwrap();
        binding.set_prefix("wid".into()).unwrap();
        let repo_id = binding.id;
        bindings.save(&binding).await.unwrap();

        let svc = TaskService::new(repo, snaps, bindings);
        let dto = svc
            .create(CreateTaskCmd {
                workspace_id: ws.to_string(),
                repo_id: Some(repo_id.to_string()),
                title: "bound task".into(),
                body: None,
                priority: None,
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
        let err = svc
            .resolve_task(&format!("nope-{hash}"))
            .await
            .unwrap_err();
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
        let svc = TaskService::new(repo.clone(), snaps, bindings_repo.clone());

        let workspace_id = WorkspaceId::new();
        // Stand up a real binding for A so the post-rollback `task_dto`
        // prefix lookup succeeds. We don't need one for B — the test never
        // renders a DTO while pointed at B.
        let binding_a = domain_repo::RepoBinding::new(
            workspace_id,
            "git@github.com:o/a.git".into(),
            "github.com/o/a".into(),
        )
        .unwrap();
        let repo_a = binding_a.id;
        bindings_repo.save(&binding_a).await.unwrap();
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
        let svc = TaskService::new(repo.clone(), snaps, bindings_repo);

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
}
