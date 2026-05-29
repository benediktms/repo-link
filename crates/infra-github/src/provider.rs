//! The public [`GithubAdapter`] wrapper and its two port implementations:
//! [`ports::RemoteTaskProvider`] (issues) and [`ports::RemoteProjectProvider`]
//! (Projects v2). It composes a REST client ([`crate::rest::RestClient`]) with
//! a GraphQL client ([`crate::graphql::GraphqlClient`]), routing each port
//! method to whichever protocol GitHub exposes the capability on.

use async_trait::async_trait;
use domain_core::Timestamp;
use ports::{
    PollPage, PortResult, RemoteChildIssue, RemoteComment, RemoteProjectProvider,
    RemoteProjectSnapshot, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskSnapshot,
    RemoteTaskUpdate,
};

use crate::graphql::GraphqlClient;
use crate::rest::{DEFAULT_BASE_URL, RestClient};

/// Single public face of the GitHub adapter. Holds one REST client (issues,
/// comments, sub-issues) and one GraphQL client (Projects v2), both bound to
/// the same token and base URL. Issue lifecycle goes through REST; project
/// status, drafts, and membership go through GraphQL.
pub struct GithubAdapter {
    rest: RestClient,
    graphql: GraphqlClient,
}

impl GithubAdapter {
    /// Default constructor — talks to `api.github.com`. Fallible because
    /// building the underlying `octocrab` clients can fail (bad base URI).
    pub fn new(token: impl Into<String>) -> PortResult<Self> {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// `base_url` exists for tests: point it at a `wiremock::MockServer::uri()`
    /// to exercise the HTTP path without hitting api.github.com. Both the REST
    /// and GraphQL clients share it (`/repos/…` vs `/graphql` off the same
    /// host).
    pub fn with_base_url(
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> PortResult<Self> {
        let token = token.into();
        let base_url = base_url.into();
        Ok(Self {
            rest: RestClient::new(token.clone(), base_url.clone())?,
            graphql: GraphqlClient::new(token, base_url)?,
        })
    }

    /// Construct from a token plus an optional base-URL override. `Some(url)`
    /// honours `REPO_LINK_GITHUB_API_BASE_URL` (GitHub Enterprise / a wiremock
    /// in tests); `None` falls back to `api.github.com`. This is the single
    /// shared entry point both `app-cli` and `app-daemon` build the adapter
    /// through, so the base-URL override is honoured identically everywhere
    /// (fixes #100 — the daemon previously called `new`, dropping the override).
    pub fn from_env_parts(token: impl Into<String>, base_url: Option<&str>) -> PortResult<Self> {
        match base_url {
            Some(url) => Self::with_base_url(token, url),
            None => Self::new(token),
        }
    }

    /// Resolve the GitHub login of the token's owner via `GET /user`. Used by
    /// `rl gh auth` to cache the login alongside the token; not on the
    /// `RemoteTaskProvider` trait because only the auth flow needs it.
    pub async fn current_user_login(&self) -> PortResult<String> {
        self.rest.current_user_login().await
    }
}

#[async_trait]
impl RemoteTaskProvider for GithubAdapter {
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

#[async_trait]
impl RemoteProjectProvider for GithubAdapter {
    async fn fetch_project(&self, owner: &str, number: u64) -> PortResult<RemoteProjectSnapshot> {
        self.graphql.fetch_project(owner, number).await
    }

    async fn add_item(&self, project_node_id: &str, issue_node_id: &str) -> PortResult<String> {
        self.graphql.add_item(project_node_id, issue_node_id).await
    }

    async fn create_draft_issue(
        &self,
        project_node_id: &str,
        title: &str,
        body: &str,
    ) -> PortResult<String> {
        self.graphql
            .create_draft_issue(project_node_id, title, body)
            .await
    }

    async fn update_draft_issue(
        &self,
        item_node_id: &str,
        title: Option<&str>,
        body: Option<&str>,
    ) -> PortResult<()> {
        self.graphql
            .update_draft_issue(item_node_id, title, body)
            .await
    }

    async fn convert_draft_to_issue(
        &self,
        item_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<(String, u64)> {
        self.graphql
            .convert_draft_to_issue(item_node_id, repo_node_id)
            .await
    }

    async fn set_status(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        status_field_id: &str,
        option_id: &str,
    ) -> PortResult<()> {
        self.graphql
            .set_status(project_node_id, item_node_id, status_field_id, option_id)
            .await
    }

    async fn poll_project_items(
        &self,
        project_node_id: &str,
        status_field_id: &str,
        since: Timestamp,
        query: &str,
    ) -> PortResult<PollPage> {
        self.graphql
            .poll_project_items(project_node_id, status_field_id, since, query)
            .await
    }
}
