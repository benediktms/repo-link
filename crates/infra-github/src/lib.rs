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
    PortResult, RemoteChildIssue, RemoteComment, RemoteTaskCreate, RemoteTaskProvider,
    RemoteTaskSnapshot, RemoteTaskUpdate,
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
}

#[cfg(test)]
mod tests {
    //! Integration-style wiremock tests — exercise the public
    //! `GithubTaskProvider` end-to-end through the trait surface. REST-
    //! internal unit tests (URL parsing, state_reason mapping) live next to
    //! their code in `rest.rs`.

    use super::*;
    use ports::RemoteStateReason;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
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
            "url": format!("https://api.github.com/users/{login}"),
            "html_url": format!("https://github.com/{login}"),
            "followers_url": format!("https://api.github.com/users/{login}/followers"),
            "following_url": format!("https://api.github.com/users/{login}/following"),
            "gists_url": format!("https://api.github.com/users/{login}/gists"),
            "starred_url": format!("https://api.github.com/users/{login}/starred"),
            "subscriptions_url": format!("https://api.github.com/users/{login}/subscriptions"),
            "organizations_url": format!("https://api.github.com/users/{login}/orgs"),
            "repos_url": format!("https://api.github.com/users/{login}/repos"),
            "events_url": format!("https://api.github.com/users/{login}/events"),
            "received_events_url": format!("https://api.github.com/users/{login}/received_events"),
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

    fn comment_payload(id: u64, login: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "node_id": "IC_kwDOAAAAAA",
            "url": format!("https://api.github.com/repos/o/r/issues/comments/{id}"),
            "html_url": format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            "body": body,
            "user": user(login),
            "created_at": "2026-05-21T09:00:00Z",
            "updated_at": "2026-05-21T09:00:00Z",
            "author_association": "OWNER"
        })
    }

    #[tokio::test]
    async fn fetch_comments_maps_and_paginates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                comment_payload(10, "alice", "first"),
                comment_payload(11, "bob", "second"),
            ]))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let comments = provider.fetch_comments("github.com/o/r", "1").await.unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].remote_id, "10");
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[0].body, "first");
        assert_eq!(comments[1].author, "bob");
    }

    #[tokio::test]
    async fn fetch_comments_paginates_past_one_page() {
        let server = MockServer::start().await;
        // Page 1: a full page of 100 → the client must request page 2.
        let page1: Vec<serde_json::Value> = (0..100)
            .map(|i| comment_payload(1000 + i, "alice", "c"))
            .collect();
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/comments"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/comments"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(vec![comment_payload(2000, "bob", "last")]),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let comments = provider.fetch_comments("github.com/o/r", "1").await.unwrap();
        assert_eq!(comments.len(), 101); // 100 + 1 across two pages
    }

    #[tokio::test]
    async fn fetch_sub_issues_maps_children_with_canonical_repo() {
        let server = MockServer::start().await;
        // GitHub returns a flat array of full issue objects (one level).
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/sub_issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                issue_payload(2, "child a", "body a", "open"),
                issue_payload(3, "child b", "body b", "closed"),
            ])))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let children = provider
            .fetch_sub_issues("github.com/o/r", "1")
            .await
            .unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].snapshot.remote_id, "2");
        assert_eq!(children[0].snapshot.title, "child a");
        assert!(!children[0].snapshot.closed);
        // issue_payload sets repository_url to .../repos/o/r → canonical github.com/o/r.
        assert_eq!(children[0].canonical_repo, "github.com/o/r");
        assert_eq!(children[1].snapshot.remote_id, "3");
        assert!(children[1].snapshot.closed);
    }

    #[tokio::test]
    async fn fetch_sub_issues_paginates_past_one_page() {
        let server = MockServer::start().await;
        // Page 1: a full page of 100 → the client must request page 2.
        let page1: Vec<serde_json::Value> = (0..100)
            .map(|i| issue_payload(1000 + i, "child", "b", "open"))
            .collect();
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/sub_issues"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;
        // Page 2: a short page → stop.
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/sub_issues"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue_payload(
                2000, "last", "b", "open",
            )]))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let children = provider
            .fetch_sub_issues("github.com/o/r", "1")
            .await
            .unwrap();
        assert_eq!(children.len(), 101); // 100 + 1 across two pages
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
