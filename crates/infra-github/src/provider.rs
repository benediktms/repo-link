//! The public [`GithubTaskProvider`] wrapper and its [`ports::RemoteTaskProvider`]
//! implementation. Today this is a thin facade over [`crate::rest::RestClient`].

use async_trait::async_trait;
use ports::{
    PortResult, RemoteChildIssue, RemoteComment, RemoteTaskCreate, RemoteTaskProvider,
    RemoteTaskSnapshot, RemoteTaskUpdate,
};

use crate::rest::{DEFAULT_BASE_URL, RestClient};

/// Single public face of the GitHub adapter. Today this is a thin wrapper
/// around [`rest::RestClient`]; when the GraphQL adapter lands, it will
/// also hold a GraphQL client and route capability-specific methods to
/// whichever one supports them.
pub struct GithubTaskProvider {
    rest: RestClient,
}

impl GithubTaskProvider {
    /// Default constructor — talks to `api.github.com`. Fallible because
    /// building the underlying `octocrab` client can fail (bad base URI).
    pub fn new(token: impl Into<String>) -> PortResult<Self> {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// `base_url` exists for tests: point it at a `wiremock::MockServer::uri()`
    /// to exercise the HTTP path without hitting api.github.com.
    pub fn with_base_url(
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> PortResult<Self> {
        Ok(Self {
            rest: RestClient::new(token, base_url)?,
        })
    }

    /// Resolve the GitHub login of the token's owner via `GET /user`. Used by
    /// `rl gh auth` to cache the login alongside the token; not on the
    /// `RemoteTaskProvider` trait because only the auth flow needs it.
    pub async fn current_user_login(&self) -> PortResult<String> {
        self.rest.current_user_login().await
    }
}

#[async_trait]
impl RemoteTaskProvider for GithubTaskProvider {
    async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
        self.rest.create_issue(cmd).await
    }

    async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot> {
        self.rest.update_issue(cmd).await
    }

    async fn fetch_remote(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<RemoteTaskSnapshot> {
        self.rest.fetch_issue(canonical_repo, remote_id).await
    }

    async fn fetch_sub_issues(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<Vec<RemoteChildIssue>> {
        self.rest.fetch_sub_issues(canonical_repo, remote_id).await
    }

    async fn fetch_comments(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<Vec<RemoteComment>> {
        self.rest.fetch_comments(canonical_repo, remote_id).await
    }

    async fn create_comment(
        &self,
        canonical_repo: &str,
        remote_id: &str,
        body: &str,
    ) -> PortResult<RemoteComment> {
        self.rest
            .create_comment(canonical_repo, remote_id, body)
            .await
    }

    async fn discover_move_target(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<Option<(String, String)>> {
        self.rest
            .discover_move_target(canonical_repo, remote_id)
            .await
    }
}
