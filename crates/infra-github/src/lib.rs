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

    /// Mount the parent-issue stub that `fetch_sub_issues` / `fetch_comments`
    /// pre-flight to detect a moved issue. Without this, those tests would
    /// fail because the pre-flight GET hits an unmocked path.
    async fn mount_parent_issue_ok(server: &MockServer, number: u64) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/issues/{number}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(issue_payload(number, "parent", "", "open")),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_comments_maps_and_paginates() {
        let server = MockServer::start().await;
        mount_parent_issue_ok(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                comment_payload(10, "alice", "first"),
                comment_payload(11, "bob", "second"),
            ]))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let comments = provider
            .fetch_comments("github.com/o/r", "1")
            .await
            .unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].remote_id, "10");
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[0].body, "first");
        assert_eq!(comments[1].author, "bob");
    }

    #[tokio::test]
    async fn fetch_comments_paginates_past_one_page() {
        let server = MockServer::start().await;
        mount_parent_issue_ok(&server, 1).await;
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
                ResponseTemplate::new(200)
                    .set_body_json(vec![comment_payload(2000, "bob", "last")]),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let comments = provider
            .fetch_comments("github.com/o/r", "1")
            .await
            .unwrap();
        assert_eq!(comments.len(), 101); // 100 + 1 across two pages
    }

    #[tokio::test]
    async fn create_comment_posts_and_maps_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/1/comments"))
            .and(header("authorization", "Bearer t0k"))
            .and(body_partial_json(serde_json::json!({"body": "looks good"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(comment_payload(
                42,
                "alice",
                "looks good",
            )))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let c = provider
            .create_comment("github.com/o/r", "1", "looks good")
            .await
            .unwrap();
        assert_eq!(c.remote_id, "42");
        assert_eq!(c.author, "alice");
        assert_eq!(c.body, "looks good");
    }

    #[tokio::test]
    async fn fetch_sub_issues_maps_children_with_canonical_repo() {
        let server = MockServer::start().await;
        mount_parent_issue_ok(&server, 1).await;
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
        mount_parent_issue_ok(&server, 1).await;
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
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(vec![issue_payload(2000, "last", "b", "open")]),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let children = provider
            .fetch_sub_issues("github.com/o/r", "1")
            .await
            .unwrap();
        assert_eq!(children.len(), 101); // 100 + 1 across two pages
    }

    /// Build an Issue payload that pretends to live in a non-default repo —
    /// the post-follow state octocrab sees after a GitHub transfer (which the
    /// tower-http FollowRedirect layer silently resolves on GETs).
    fn issue_payload_in_repo(
        number: u64,
        title: &str,
        body: &str,
        state: &str,
        owner: &str,
        repo: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": number,
            "node_id": "I_kwDOAAAAAA",
            "url": format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}"),
            "repository_url": format!("https://api.github.com/repos/{owner}/{repo}"),
            "labels_url": format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/labels"),
            "comments_url": format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/comments"),
            "events_url": format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/events"),
            "html_url": format!("https://github.com/{owner}/{repo}/issues/{number}"),
            "number": number,
            "state": state,
            "title": title,
            "body": body,
            "user": user("octocat"),
            "labels": [],
            "assignee": null,
            "assignees": [],
            "milestone": null,
            "locked": false,
            "active_lock_reason": null,
            "comments": 0,
            "pull_request": null,
            "closed_at": null,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "author_association": "OWNER"
        })
    }

    #[tokio::test]
    async fn discover_move_target_compares_repository_url_after_followed_redirect() {
        // octocrab follows the 301 on a safe GET, so by the time we see the
        // response the body's `repository_url` + `number` are the new repo's.
        // We simulate the post-follow state here: mock GET on the old URL
        // returns 200 with a body that names the new repo.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/5788"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(issue_payload_in_repo(
                    1506,
                    "transferred",
                    "moved",
                    "open",
                    "o2",
                    "r2",
                )),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let target = provider
            .discover_move_target("github.com/o/r", "5788")
            .await
            .unwrap();
        assert_eq!(target, Some(("github.com/o2/r2".into(), "1506".into())));
    }

    #[tokio::test]
    async fn discover_move_target_returns_none_when_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/42"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(issue_payload(42, "x", "y", "open")),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let target = provider
            .discover_move_target("github.com/o/r", "42")
            .await
            .unwrap();
        assert_eq!(target, None);
    }

    #[tokio::test]
    async fn fetch_remote_on_transferred_issue_surfaces_issue_moved() {
        // Same simulation: octocrab follows the 301 silently and lands on a
        // response describing the *new* repo. fetch_remote post-checks
        // `repository_url` and surfaces a typed IssueMoved instead of
        // silently returning the wrong issue's data.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/5788"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(issue_payload_in_repo(
                    1506,
                    "transferred",
                    "moved",
                    "open",
                    "o2",
                    "r2",
                )),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let err = provider
            .fetch_remote("github.com/o/r", "5788")
            .await
            .unwrap_err();
        match err {
            ports::PortError::IssueMoved {
                from_canonical,
                from_remote_id,
                to_canonical,
                to_remote_id,
            } => {
                assert_eq!(from_canonical, "github.com/o/r");
                assert_eq!(from_remote_id, "5788");
                assert_eq!(to_canonical, "github.com/o2/r2");
                assert_eq!(to_remote_id, "1506");
            }
            other => panic!("expected IssueMoved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_comments_on_transferred_parent_surfaces_issue_moved() {
        // GitHub's `/comments` payload doesn't name the parent's repo, so a
        // silent redirect-follow would return the new repo's comments without
        // the caller knowing. The pre-flight `ensure_not_moved` catches this.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/5788"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(issue_payload_in_repo(
                    1506, "moved", "x", "open", "o2", "r2",
                )),
            )
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let err = provider
            .fetch_comments("github.com/o/r", "5788")
            .await
            .unwrap_err();
        assert!(matches!(err, ports::PortError::IssueMoved { .. }));
    }

    #[tokio::test]
    async fn current_user_login_returns_token_owner() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", "Bearer t0k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(user("benediktms")))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri()).unwrap();
        let login = provider.current_user_login().await.unwrap();
        assert_eq!(login, "benediktms");
    }

    #[tokio::test]
    async fn current_user_login_maps_401_to_network_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "message": "Bad credentials"
            })))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("bad", server.uri()).unwrap();
        let err = provider.current_user_login().await.unwrap_err();
        assert!(
            matches!(err, ports::PortError::Network(_)),
            "expected Network for 401, got {err:?}"
        );
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
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"message": "Not Found"})),
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
