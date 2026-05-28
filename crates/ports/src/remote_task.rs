//! Remote task provider port and its DTOs.

use async_trait::async_trait;
use domain_core::Timestamp;

use crate::error::PortResult;

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
}
