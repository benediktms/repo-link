//! ports — async trait contracts between application and infrastructure.

use std::path::Path;

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, Timestamp, WorkspaceId};
use domain_repo::RepoBinding;
use domain_task::{SnapshotSource, SyncState, Task, TaskSnapshot, TaskStatus};
use domain_workspace::Workspace;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PortError {
    #[error("not found: {0}")]
    NotFound(String),

    /// A uniqueness violation. `target` names the logical constraint the
    /// backend reported (e.g. `"tasks.hash"`, `"repos.prefix"`) when it
    /// can — adapters translate their native error into this structured
    /// form so the application layer can drive retry logic off the
    /// target instead of substring-matching backend-specific message
    /// text. `None` when the backend gives no usable target.
    #[error("conflict{}: {message}", .target.as_deref().map(|t| format!(" on {t}")).unwrap_or_default())]
    Conflict {
        target: Option<String>,
        message: String,
    },

    #[error("backend failure: {0}")]
    Backend(String),

    #[error("network failure: {0}")]
    Network(String),

    /// The remote issue at `from_canonical#from_remote_id` was administratively
    /// transferred to `to_canonical#to_remote_id` (GitHub returned 301 with a
    /// `Location` header). Adapters surface this *typed* error instead of a
    /// raw network failure so callers can offer a verified re-link rather than
    /// asking the user to diagnose an opaque HTTP code.
    #[error(
        "remote issue {from_canonical}#{from_remote_id} moved to {to_canonical}#{to_remote_id}"
    )]
    IssueMoved {
        from_canonical: String,
        from_remote_id: String,
        to_canonical: String,
        to_remote_id: String,
    },
}

impl PortError {
    /// The logical target of a uniqueness [`PortError::Conflict`]
    /// (e.g. `"tasks.hash"`, `"repos.prefix"`), if the backend reported
    /// one. Returns `None` for non-conflict errors or conflicts without
    /// a target.
    pub fn conflict_target(&self) -> Option<&str> {
        match self {
            PortError::Conflict {
                target: Some(t), ..
            } => Some(t.as_str()),
            _ => None,
        }
    }
}

pub type PortResult<T> = std::result::Result<T, PortError>;

// ---------- Workspace repository -----------------------------------------

#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    async fn save(&self, workspace: &Workspace) -> PortResult<()>;
    async fn get(&self, id: WorkspaceId) -> PortResult<Workspace>;
    async fn find_by_name(&self, name: &str) -> PortResult<Option<Workspace>>;
    async fn list(&self, include_archived: bool) -> PortResult<Vec<Workspace>>;
    async fn delete(&self, id: WorkspaceId) -> PortResult<()>;
}

// ---------- Repo binding repository --------------------------------------

#[async_trait]
pub trait RepoBindingRepository: Send + Sync {
    async fn save(&self, binding: &RepoBinding) -> PortResult<()>;
    async fn get(&self, id: RepoId) -> PortResult<RepoBinding>;
    async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> PortResult<Vec<RepoBinding>>;
    async fn find_by_canonical_url(
        &self,
        workspace_id: WorkspaceId,
        canonical_url: &str,
    ) -> PortResult<Option<RepoBinding>>;
    /// Look up a binding by its globally-unique `prefix`. Used by the
    /// repo locator path so callers can pass `--repo rpl` (or use
    /// `rpl-ak7` for tasks and reuse the prefix half here) instead of a
    /// UUID.
    async fn find_by_prefix(&self, prefix: &str) -> PortResult<Option<RepoBinding>>;
    async fn delete(&self, id: RepoId) -> PortResult<()>;
}

// ---------- Task repository -----------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub workspace_id: Option<WorkspaceId>,
    pub repo_id: Option<RepoId>,
    /// Filter by lifecycle status. When `None`, callers usually want
    /// non-archived rows only — see `include_archived`.
    pub status: Option<TaskStatus>,
    /// Filter by sync state.
    pub sync_state: Option<SyncState>,
    /// When `status` is `None`, include `Archived` rows. Ignored if
    /// `status` is set explicitly.
    pub include_archived: bool,
}

#[async_trait]
pub trait TaskRepository: Send + Sync {
    /// Persist `task` and append a new row to its snapshot history,
    /// tagged with `source`. The adapter assigns the next monotonic
    /// `version`. Both writes are committed in a single transaction.
    async fn save(&self, task: &Task, source: SnapshotSource) -> PortResult<()>;
    async fn get(&self, id: TaskId) -> PortResult<Task>;
    async fn list(&self, filter: TaskFilter) -> PortResult<Vec<Task>>;
    /// Look up a task by its globally-unique `hash`. Used by the
    /// friendly-ID resolver so callers can pass a bare hash (`ak7`) or
    /// the prefix half of a composite (`rlk-ak7`) instead of a UUID.
    async fn find_by_hash(&self, hash: &str) -> PortResult<Option<Task>>;
    /// Look up the task mirroring a given remote issue within a repo
    /// (`repo_id` + `provider` + `remote_id`). Scoped by repo because remote
    /// issue numbers are only unique per repo (GitHub `repoA#123` ≠
    /// `repoB#123`). Used by `sync import` to skip already-tracked issues.
    async fn find_by_remote(
        &self,
        repo_id: RepoId,
        provider: &str,
        remote_id: &str,
    ) -> PortResult<Option<Task>>;
    /// Replace the task's *synced* comments with `comments` (always
    /// remote-backed — taking [`RemoteComment`] rather than `TaskComment`
    /// makes pending input unrepresentable), leaving any pending local-only
    /// comments untouched. Writes only the `task_comments` table — never a
    /// snapshot — so mirroring remote comments doesn't perturb sync state.
    async fn replace_comments(
        &self,
        task_id: TaskId,
        comments: &[RemoteComment],
    ) -> PortResult<()>;
    /// Append a single pending (local-only) comment, stored with the empty
    /// `remote_comment_id` sentinel. Writes only the `task_comments` table —
    /// never a snapshot — so adding a comment never perturbs sync state
    /// (pending comments are a separate outbound axis from title/body drift).
    async fn add_pending_comment(
        &self,
        task_id: TaskId,
        author: &str,
        body: &str,
        created_at: Timestamp,
    ) -> PortResult<()>;
    /// Promote a task's pending comments to synced after a successful remote
    /// push: deletes the rows in `drained_local_ids` and inserts `pushed` as
    /// synced rows. Writes only `task_comments`, never a snapshot.
    ///
    /// Identity-aware so the drain can't race-delete a pending comment that
    /// was added between the caller reading the task and this call: only the
    /// rows whose surrogate id was actually pushed are removed.
    async fn mark_comments_pushed(
        &self,
        task_id: TaskId,
        drained_local_ids: &[String],
        pushed: &[RemoteComment],
    ) -> PortResult<()>;
    /// Count pending (local-only) comments per task across a workspace, so
    /// `query unsynced` can surface comment-only outbound work without loading
    /// every task's comments (`list` deliberately skips them). Returns only
    /// tasks with at least one pending comment.
    async fn pending_comment_counts(
        &self,
        workspace_id: WorkspaceId,
    ) -> PortResult<std::collections::HashMap<TaskId, usize>>;
    async fn delete(&self, id: TaskId) -> PortResult<()>;
}

/// History queries over [`TaskSnapshot`] rows. Reads only — appends are
/// the side-effect of [`TaskRepository::save`] (so the snapshot table and
/// the task projection can't drift apart).
#[async_trait]
pub trait TaskSnapshotRepository: Send + Sync {
    /// All snapshots for a task, oldest version first.
    async fn list(&self, task_id: TaskId) -> PortResult<Vec<TaskSnapshot>>;

    /// Fetch a specific version. Returns `NotFound` if the version
    /// doesn't exist.
    async fn get(&self, task_id: TaskId, version: u64) -> PortResult<TaskSnapshot>;
}

// ---------- Remote task provider (GitHub etc.) ---------------------------

#[derive(Clone, Debug)]
pub struct RemoteTaskCreate<'a> {
    pub canonical_repo: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub assignees: &'a [String],
    pub labels: &'a [String],
}

/// Why a remote task is changing state. Providers that don't model this
/// (GitLab, custom backends) can silently drop it. Names mirror GitHub's
/// `state_reason` vocab because that's the most expressive enumeration
/// currently in the wild; adapters map to their wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteStateReason {
    /// Work finished as planned.
    Completed,
    /// Won't be done — dropped, abandoned, deferred indefinitely.
    NotPlanned,
    /// Closed because it's a duplicate of another task.
    Duplicate,
    /// Closed → open transition.
    Reopened,
}

#[derive(Clone, Debug)]
pub struct RemoteTaskUpdate<'a> {
    pub canonical_repo: &'a str,
    pub remote_id: &'a str,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub closed: Option<bool>,
    /// Annotates a state transition. Meaningful with `closed = Some(true)`
    /// (Completed / NotPlanned / Duplicate) or when reopening
    /// (`Reopened`). Adapters ignore the field if their backend has no
    /// equivalent concept.
    pub state_reason: Option<RemoteStateReason>,
}

#[derive(Clone, Debug)]
pub struct RemoteTaskSnapshot {
    pub remote_id: String,
    pub title: String,
    pub body: String,
    pub closed: bool,
    pub updated_at: Timestamp,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
}

/// One sub-issue returned by [`RemoteTaskProvider::fetch_sub_issues`], paired
/// with the canonical repo it actually lives in. A sub-issue can belong to a
/// different repo than its parent, so the canonical is carried here (rather
/// than widening [`RemoteTaskSnapshot`]) to let the import orchestrator detect
/// and skip cross-repo children.
#[derive(Clone, Debug)]
pub struct RemoteChildIssue {
    pub canonical_repo: String,
    pub snapshot: RemoteTaskSnapshot,
}

/// A comment fetched from a remote issue. Always carries a remote id (the
/// provider assigns one on create); the local-only / pending case is
/// represented by [`domain_task::TaskComment::remote_id`] being `None`.
#[derive(Clone, Debug)]
pub struct RemoteComment {
    pub remote_id: String,
    pub author: String,
    pub body: String,
    pub created_at: Timestamp,
}

#[async_trait]
pub trait RemoteTaskProvider: Send + Sync {
    async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot>;
    async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot>;
    async fn fetch_remote(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<RemoteTaskSnapshot>;

    /// List the direct (one level) sub-issues of a remote task. Providers
    /// without a sub-issue concept inherit the default empty result, so only
    /// adapters that support it (GitHub) need to override. Recursion into
    /// grandchildren is the caller's job.
    async fn fetch_sub_issues(
        &self,
        _canonical_repo: &str,
        _remote_id: &str,
    ) -> PortResult<Vec<RemoteChildIssue>> {
        Ok(Vec::new())
    }

    /// List the comments on a remote task, oldest first. Providers without a
    /// comment concept inherit the default empty result; GitHub overrides.
    async fn fetch_comments(
        &self,
        _canonical_repo: &str,
        _remote_id: &str,
    ) -> PortResult<Vec<RemoteComment>> {
        Ok(Vec::new())
    }

    /// Create a comment on a remote task and return it with its provider-
    /// assigned id/author/timestamp. Required (no default): a write has no
    /// sensible no-op fallback, so each provider must implement it explicitly.
    async fn create_comment(
        &self,
        canonical_repo: &str,
        remote_id: &str,
        body: &str,
    ) -> PortResult<RemoteComment>;

    /// Probe the remote for a transferred-issue redirect. Returns
    /// `Some((to_canonical_repo, to_remote_id))` if the provider reports the
    /// task at `(canonical_repo, remote_id)` has been moved, `None` if the
    /// task is still at the supplied address. Used by `rl task link --relink`
    /// to verify a user-supplied URL is GitHub's actual redirect target
    /// before rewriting the task's remote identity. Providers without a
    /// transfer concept inherit the default `Ok(None)`.
    async fn discover_move_target(
        &self,
        _canonical_repo: &str,
        _remote_id: &str,
    ) -> PortResult<Option<(String, String)>> {
        Ok(None)
    }
}

// ---------- Filesystem probe ---------------------------------------------

#[async_trait]
pub trait FilesystemProbe: Send + Sync {
    async fn path_exists(&self, path: &Path) -> PortResult<bool>;
    async fn is_git_worktree(&self, path: &Path) -> PortResult<bool>;
}

// ---------- Clock --------------------------------------------------------

/// Injected to keep reconciliation deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

// ---------- Event sink ---------------------------------------------------

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn record(&self, envelope: dto_events::EventEnvelope) -> PortResult<()>;
}
