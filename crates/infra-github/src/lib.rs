//! infra-github — GitHub adapter implementing [`ports::RemoteTaskProvider`].
//!
//! Issues are the underlying remote task object. Promotion creates an issue;
//! push updates it; pull fetches its current state. The REST surface is
//! driven by `octocrab`.
//!
//! # Internals
//!
//! Protocol-specific code is split into submodules so REST and GraphQL stay
//! distinct:
//!
//! - [`rest`] — issue CRUD via `octocrab`'s REST handlers (today's full
//!   implementation). Owns the issue-model mapping, URL parsing, and the
//!   `state_reason` enum mapping.
//! - `graphql` *(future)* — sibling module for capabilities only exposed
//!   via GraphQL (Projects v2 status fields, sub-issue parents, custom
//!   fields). The wrapper struct below will compose both clients.

use async_trait::async_trait;
use ports::{
    PortResult, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskSnapshot, RemoteTaskUpdate,
};

mod rest;

/// Single public face of the GitHub adapter. Today this is a thin wrapper
/// around [`rest::RestClient`]; when the GraphQL adapter lands, it will
/// also hold a GraphQL client and route capability-specific methods to
/// whichever one supports them.
pub struct GithubTaskProvider {
    rest: rest::RestClient,
}

impl GithubTaskProvider {
    /// Default constructor — talks to `api.github.com`. Fallible because
    /// building the underlying `octocrab` client can fail (bad base URI).
    pub fn new(token: impl Into<String>) -> PortResult<Self> {
        Self::with_base_url(token, rest::DEFAULT_BASE_URL)
    }

    /// `base_url` exists for tests: point it at a `wiremock::MockServer::uri()`
    /// to exercise the HTTP path without hitting api.github.com.
    pub fn with_base_url(
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> PortResult<Self> {
        Ok(Self {
            rest: rest::RestClient::new(token, base_url)?,
        })
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
}

#[cfg(test)]
mod tests {
    //! Integration-style wiremock tests — exercise the public
    //! `GithubTaskProvider` end-to-end through the trait surface. REST-
    //! internal unit tests (URL parsing, state_reason mapping) live next to
    //! their code in `rest.rs`.

    use super::*;
    use ports::RemoteStateReason;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A full GitHub `Author` object. octocrab's typed `Author` model has 18
    /// required fields (mostly `Url`s), so a minimal stub won't deserialize.
    fn user(login: &str) -> serde_json::Value {
        serde_json::json!({
            "login": login,
            "id": 1,
            "node_id": "U_kgDOAAAAAA",
            "avatar_url": "https://avatars.githubusercontent.com/u/1",
            "gravatar_id": "",
            "url": "https://api.github.com/users/{login}",
            "html_url": "https://github.com/{login}",
            "followers_url": "https://api.github.com/users/{login}/followers",
            "following_url": "https://api.github.com/users/{login}/following",
            "gists_url": "https://api.github.com/users/{login}/gists",
            "starred_url": "https://api.github.com/users/{login}/starred",
            "subscriptions_url": "https://api.github.com/users/{login}/subscriptions",
            "organizations_url": "https://api.github.com/users/{login}/orgs",
            "repos_url": "https://api.github.com/users/{login}/repos",
            "events_url": "https://api.github.com/users/{login}/events",
            "received_events_url": "https://api.github.com/users/{login}/received_events",
            "type": "User",
            "site_admin": false
        })
    }

    /// A full GitHub `Label` object (octocrab requires id/node_id/url/name/color/default).
    fn label(name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "node_id": "LA_kgDOAAAAAA",
            "url": "https://api.github.com/repos/o/r/labels/bug",
            "name": name,
            "color": "d73a4a",
            "default": true
        })
    }

    /// A full GitHub issue JSON body. octocrab's typed `Issue` demands ~19
    /// required fields (the original 7-field stub no longer deserializes).
    fn issue_payload(number: u64, title: &str, body: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "id": number,
            "node_id": "I_kwDOAAAAAA",
            "url": format!("https://api.github.com/repos/o/r/issues/{number}"),
            "repository_url": "https://api.github.com/repos/o/r",
            "labels_url": format!("https://api.github.com/repos/o/r/issues/{number}/labels"),
            "comments_url": format!("https://api.github.com/repos/o/r/issues/{number}/comments"),
            "events_url": format!("https://api.github.com/repos/o/r/issues/{number}/events"),
            "html_url": format!("https://github.com/o/r/issues/{number}"),
            "number": number,
            "state": state,
            "title": title,
            "body": body,
            "user": user("octocat"),
            "labels": [label("bug")],
            "assignees": [user("alice")],
            "locked": false,
            "comments": 0,
            "created_at": "2026-05-20T12:00:00Z",
            "updated_at": "2026-05-20T12:00:00Z",
            "author_association": "OWNER"
        })
    }

    #[tokio::test]
    async fn create_issue_returns_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues"))
            .and(header("authorization", "Bearer t0k"))
            .and(body_partial_json(serde_json::json!({"title": "ship it"})))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(issue_payload(42, "ship it", "soon", "open")),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let snap = provider
            .create_remote(RemoteTaskCreate {
                canonical_repo: "github.com/o/r",
                title: "ship it",
                body: "soon",
                assignees: &[],
                labels: &[],
            })
            .await
            .unwrap();
        assert_eq!(snap.remote_id, "42");
        assert_eq!(snap.title, "ship it");
        assert!(!snap.closed);
        assert_eq!(snap.assignees, vec!["alice".to_string()]);
        assert_eq!(snap.labels, vec!["bug".to_string()]);
    }

    #[tokio::test]
    async fn update_issue_patches_only_provided_fields() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/42"))
            .and(body_partial_json(serde_json::json!({"state": "closed"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(issue_payload(42, "x", "y", "closed")),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let snap = provider
            .update_remote(RemoteTaskUpdate {
                canonical_repo: "github.com/o/r",
                remote_id: "42",
                title: None,
                body: None,
                closed: Some(true),
                state_reason: None,
            })
            .await
            .unwrap();
        assert!(snap.closed);
    }

    #[tokio::test]
    async fn update_issue_sends_state_reason_when_closing() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/42"))
            .and(body_partial_json(
                serde_json::json!({"state": "closed", "state_reason": "not_planned"}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(issue_payload(42, "x", "y", "closed")),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        provider
            .update_remote(RemoteTaskUpdate {
                canonical_repo: "github.com/o/r",
                remote_id: "42",
                title: None,
                body: None,
                closed: Some(true),
                state_reason: Some(RemoteStateReason::NotPlanned),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fetch_issue_404_maps_to_not_found() {
        let server = MockServer::start().await;
        // octocrab decodes error bodies into its typed `GitHubError`, so the
        // fixture must be a JSON error object (not a bare string) for the
        // status code to surface as `Error::GitHub`.
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/99"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"message": "Not Found"})),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let err = provider
            .fetch_remote("github.com/o/r", "99")
            .await
            .unwrap_err();
        assert!(matches!(err, ports::PortError::NotFound(_)));
    }
}
