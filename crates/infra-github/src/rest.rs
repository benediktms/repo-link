//! GitHub REST adapter internals.
//!
//! Everything in this module is REST-specific: the HTTP client, the JSON
//! wire types, the URL parsing, the `state_reason` mapping. A future
//! `graphql` sibling will live next to this one (for Projects v2 mutations
//! and other capabilities not on the REST surface); the top-level
//! `GithubTaskProvider` in `lib.rs` will compose both.

use chrono::{DateTime, Utc};
use domain_core::Timestamp;
use ports::{
    PortError, PortResult, RemoteStateReason, RemoteTaskCreate, RemoteTaskSnapshot,
    RemoteTaskUpdate,
};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.github.com";
const USER_AGENT: &str = "repo-link";

/// REST client. Stateless beyond the HTTP client + auth token; the actual
/// `GithubTaskProvider` wraps this and dispatches the [`ports::RemoteTaskProvider`]
/// methods through it.
pub(crate) struct RestClient {
    http: HttpClient,
    base_url: String,
    token: String,
}

impl RestClient {
    pub(crate) fn new(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: HttpClient::new(),
            base_url: base_url.into(),
            token: token.into(),
        }
    }

    pub(crate) async fn create_issue(
        &self,
        cmd: RemoteTaskCreate<'_>,
    ) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(cmd.canonical_repo)?;
        let body = CreateIssueBody {
            title: cmd.title,
            body: cmd.body,
            assignees: cmd.assignees,
            labels: cmd.labels,
        };
        let resp = self
            .request(
                reqwest::Method::POST,
                &format!("/repos/{owner}/{repo}/issues"),
            )
            .json(&body)
            .send()
            .await
            .map_err(net)?;
        decode_issue(resp).await
    }

    pub(crate) async fn update_issue(
        &self,
        cmd: RemoteTaskUpdate<'_>,
    ) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(cmd.canonical_repo)?;
        let body = UpdateIssueBody {
            title: cmd.title,
            body: cmd.body,
            state: cmd.closed.map(|c| if c { "closed" } else { "open" }),
            state_reason: cmd.state_reason.map(state_reason_str),
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

    pub(crate) async fn fetch_issue(
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

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }
}

// ---------- Wire types (REST JSON shapes) --------------------------------

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
    #[serde(skip_serializing_if = "Option::is_none")]
    state_reason: Option<&'static str>,
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

// ---------- Mapping (provider-agnostic enums → REST strings) -------------

/// REST-specific mapping from the typed [`RemoteStateReason`] to GitHub's
/// `state_reason` wire format. Lives in this module because the strings
/// (`"completed"`, `"not_planned"`, …) are REST-API-shaped; GraphQL uses a
/// different enum vocabulary (`StateReason::COMPLETED` etc.) and will have
/// its own mapping when that adapter lands.
pub(crate) fn state_reason_str(reason: RemoteStateReason) -> &'static str {
    match reason {
        RemoteStateReason::Completed => "completed",
        RemoteStateReason::NotPlanned => "not_planned",
        RemoteStateReason::Duplicate => "duplicate",
        RemoteStateReason::Reopened => "reopened",
    }
}

pub(crate) fn split_owner_repo(canonical: &str) -> PortResult<(String, String)> {
    let stripped = canonical
        .strip_prefix("github.com/")
        .ok_or_else(|| PortError::Backend(format!("not a github canonical url: {canonical}")))?;
    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
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

    #[test]
    fn rejects_non_github_canonical() {
        assert!(split_owner_repo("gitlab.com/o/r").is_err());
        assert!(split_owner_repo("github.com/o").is_err());
        assert!(split_owner_repo("github.com/o/r/extra").is_err());
        assert!(split_owner_repo("github.com/o/r/extra/segments").is_err());
        assert_eq!(
            split_owner_repo("github.com/o/r").unwrap(),
            ("o".into(), "r".into())
        );
    }

    #[test]
    fn state_reason_strings_match_github_wire_format() {
        assert_eq!(state_reason_str(RemoteStateReason::Completed), "completed");
        assert_eq!(
            state_reason_str(RemoteStateReason::NotPlanned),
            "not_planned"
        );
        assert_eq!(state_reason_str(RemoteStateReason::Duplicate), "duplicate");
        assert_eq!(state_reason_str(RemoteStateReason::Reopened), "reopened");
    }
}
