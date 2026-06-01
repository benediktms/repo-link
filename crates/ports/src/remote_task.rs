//! Remote task provider port and its DTOs.

use async_trait::async_trait;
use domain_core::Timestamp;

use crate::error::{PortError, PortResult};

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
    /// Provider-native opaque node ID (e.g. GitHub `I_kwHO…`). GitHub's REST
    /// issue payload carries it alongside `number`, so every create / fetch
    /// path can surface it here. Maps onto [`domain_task::RemoteRef::node_id`];
    /// `None` for providers (or REST responses) that don't expose one. Required
    /// by GraphQL mutations such as `addProjectV2ItemById`, so capturing it on
    /// the REST paths is what makes a task board-eligible (RFC 0001 §9 / §D1).
    pub node_id: Option<String>,
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

    /// List the issues that the given remote task is **blocked by** — the
    /// inbound read counterpart of [`add_blocked_by`](Self::add_blocked_by),
    /// backing relation reconcile on pull. Each entry carries the blocker's
    /// canonical repo (a dependency may be cross-repo) so the caller can map it
    /// to a local task. Providers without an issue-dependency concept inherit
    /// the default empty result; GitHub overrides.
    async fn fetch_blocked_by(
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

    /// List issues in `canonical_repo` whose `updatedAt` is at or after
    /// `since`. Backs the REST polling fallback used by binding-only
    /// (projectless) workspaces — see RFC 0001 §3 D4. Providers without
    /// a since-filter inherit the default empty result, so only adapters
    /// that support it (GitHub) need to override.
    async fn list_changed_since(
        &self,
        _canonical_repo: &str,
        _since: Timestamp,
    ) -> PortResult<Vec<RemoteTaskSnapshot>> {
        Ok(Vec::new())
    }

    /// Link `(child_canonical, child_remote_id)` as a sub-issue of
    /// `(parent_canonical, parent_remote_id)` — the outbound projection of a
    /// `parent_of` / `child_of` relation. The adapter is responsible for
    /// resolving the child's provider-native id form its API needs (GitHub's
    /// `sub_issues` body wants the child's integer **database id**, not its
    /// `#number`). Idempotent: re-linking an existing sub-issue must succeed.
    /// Providers without a sub-issue concept inherit the `Unsupported` default.
    async fn add_sub_issue(
        &self,
        _parent_canonical: &str,
        _parent_remote_id: &str,
        _child_canonical: &str,
        _child_remote_id: &str,
    ) -> PortResult<()> {
        Err(PortError::Backend(
            "sub-issue relations not supported by this provider".into(),
        ))
    }

    /// Unlink the sub-issue relationship created by [`add_sub_issue`]. Idempotent:
    /// removing an absent link must succeed.
    ///
    /// [`add_sub_issue`]: Self::add_sub_issue
    async fn remove_sub_issue(
        &self,
        _parent_canonical: &str,
        _parent_remote_id: &str,
        _child_canonical: &str,
        _child_remote_id: &str,
    ) -> PortResult<()> {
        Err(PortError::Backend(
            "sub-issue relations not supported by this provider".into(),
        ))
    }

    /// Record that `(blocked_canonical, blocked_remote_id)` is blocked by
    /// `(blocker_canonical, blocker_remote_id)` — the outbound projection of a
    /// `blocked_by` / `blocks` relation onto GitHub issue dependencies. The
    /// adapter resolves the blocker's native id (GitHub wants the blocker's
    /// integer **database id** in the `issue_id` body). Idempotent.
    /// Providers without a dependency concept inherit the `Unsupported` default.
    async fn add_blocked_by(
        &self,
        _blocked_canonical: &str,
        _blocked_remote_id: &str,
        _blocker_canonical: &str,
        _blocker_remote_id: &str,
    ) -> PortResult<()> {
        Err(PortError::Backend(
            "issue dependencies not supported by this provider".into(),
        ))
    }

    /// Drop the dependency created by [`add_blocked_by`]. Idempotent.
    ///
    /// [`add_blocked_by`]: Self::add_blocked_by
    async fn remove_blocked_by(
        &self,
        _blocked_canonical: &str,
        _blocked_remote_id: &str,
        _blocker_canonical: &str,
        _blocker_remote_id: &str,
    ) -> PortResult<()> {
        Err(PortError::Backend(
            "issue dependencies not supported by this provider".into(),
        ))
    }
}
