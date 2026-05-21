//! infra-github — GitHub REST adapter implementing [`ports::RemoteTaskProvider`].
//!
//! Issues are the underlying remote task object. Promotion creates an issue;
//! push updates it; pull fetches its current state. The adapter is stateless
//! beyond the HTTP client + auth token; sync logic lives in
//! `application-sync`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain_core::Timestamp;
use ports::{
    PortError, PortResult, RemoteTaskCreate, RemoteTaskProvider, RemoteTaskSnapshot,
    RemoteTaskUpdate,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const USER_AGENT: &str = "repo-link";

pub struct GithubTaskProvider {
    client: Client,
    base_url: String,
    token: String,
}

impl GithubTaskProvider {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// `base_url` exists for tests: point it at a `wiremock::MockServer::uri()`
    /// to exercise the HTTP path without hitting api.github.com.
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            token: token.into(),
        }
    }
}

#[async_trait]
impl RemoteTaskProvider for GithubTaskProvider {
    async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(cmd.canonical_repo)?;
        let body = CreateIssueBody {
            title: cmd.title,
            body: cmd.body,
            assignees: cmd.assignees,
            labels: cmd.labels,
        };
        let resp = self
            .request(reqwest::Method::POST, &format!("/repos/{owner}/{repo}/issues"))
            .json(&body)
            .send()
            .await
            .map_err(net)?;
        decode_issue(resp).await
    }

    async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(cmd.canonical_repo)?;
        let body = UpdateIssueBody {
            title: cmd.title,
            body: cmd.body,
            state: cmd.closed.map(|c| if c { "closed" } else { "open" }),
        };
        let resp = self
            .request(
                reqwest::Method::PATCH,
                &format!("/repos/{owner}/{repo}/issues/{}", cmd.remote_id),
            )
            .json(&body)
            .send()
            .await
            .map_err(net)?;
        decode_issue(resp).await
    }

    async fn fetch_remote(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(canonical_repo)?;
        let resp = self
            .request(
                reqwest::Method::GET,
                &format!("/repos/{owner}/{repo}/issues/{remote_id}"),
            )
            .send()
            .await
            .map_err(net)?;
        decode_issue(resp).await
    }
}

impl GithubTaskProvider {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }
}

// ---------- Wire types ---------------------------------------------------

#[derive(Serialize)]
struct CreateIssueBody<'a> {
    title: &'a str,
    body: &'a str,
    assignees: &'a [String],
    labels: &'a [String],
}

#[derive(Serialize)]
struct UpdateIssueBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
}

#[derive(Deserialize)]
struct IssueResponse {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    assignees: Vec<UserRef>,
    #[serde(default)]
    labels: Vec<LabelRef>,
}

#[derive(Deserialize)]
struct UserRef {
    login: String,
}

#[derive(Deserialize)]
struct LabelRef {
    name: String,
}

// ---------- Helpers ------------------------------------------------------

fn split_owner_repo(canonical: &str) -> PortResult<(String, String)> {
    let stripped = canonical
        .strip_prefix("github.com/")
        .ok_or_else(|| PortError::Backend(format!("not a github canonical url: {canonical}")))?;
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(PortError::Backend(format!(
            "expected github.com/<owner>/<repo>, got {canonical}"
        )));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

async fn decode_issue(resp: reqwest::Response) -> PortResult<RemoteTaskSnapshot> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(match status.as_u16() {
            404 => PortError::NotFound(body),
            409 | 422 => PortError::Conflict(body),
            _ => PortError::Network(format!("github {status}: {body}")),
        });
    }
    let issue: IssueResponse = resp.json().await.map_err(net)?;
    Ok(RemoteTaskSnapshot {
        remote_id: issue.number.to_string(),
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        closed: issue.state == "closed",
        updated_at: Timestamp::from_utc(issue.updated_at),
        assignees: issue.assignees.into_iter().map(|u| u.login).collect(),
        labels: issue.labels.into_iter().map(|l| l.name).collect(),
    })
}

fn net(e: reqwest::Error) -> PortError {
    PortError::Network(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn issue_payload(number: u64, title: &str, body: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": title,
            "body": body,
            "state": state,
            "updated_at": "2026-05-20T12:00:00Z",
            "assignees": [{"login": "alice"}],
            "labels": [{"name": "bug"}]
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

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri());
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

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri());
        let snap = provider
            .update_remote(RemoteTaskUpdate {
                canonical_repo: "github.com/o/r",
                remote_id: "42",
                title: None,
                body: None,
                closed: Some(true),
            })
            .await
            .unwrap();
        assert!(snap.closed);
    }

    #[tokio::test]
    async fn fetch_issue_404_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/99"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;

        let provider = GithubTaskProvider::with_base_url("t0k", server.uri());
        let err = provider
            .fetch_remote("github.com/o/r", "99")
            .await
            .unwrap_err();
        assert!(matches!(err, PortError::NotFound(_)));
    }

    #[test]
    fn rejects_non_github_canonical() {
        assert!(split_owner_repo("gitlab.com/o/r").is_err());
        assert!(split_owner_repo("github.com/o").is_err());
        assert_eq!(
            split_owner_repo("github.com/o/r").unwrap(),
            ("o".into(), "r".into())
        );
    }
}
