//! GitHub REST adapter internals.
//!
//! Everything in this module is REST-specific: the `octocrab` client, the
//! issue-model mapping, the URL parsing, the `state_reason` mapping. A future
//! `graphql` sibling will live next to this one (for Projects v2 mutations
//! and other capabilities not on the REST surface); the top-level
//! `GithubTaskProvider` in `lib.rs` will compose both.

use domain_core::Timestamp;
use octocrab::Octocrab;
use octocrab::models::IssueState;
use octocrab::models::issues::{Issue, IssueStateReason};
use ports::{
    PortError, PortResult, RemoteChildIssue, RemoteComment, RemoteStateReason, RemoteTaskCreate,
    RemoteTaskSnapshot, RemoteTaskUpdate,
};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.github.com";

/// REST client. A thin wrapper around an `octocrab` instance bound to one
/// token; the actual `GithubTaskProvider` wraps this and dispatches the
/// [`ports::RemoteTaskProvider`] methods through it.
pub(crate) struct RestClient {
    http: Octocrab,
}

impl RestClient {
    pub(crate) fn new(token: impl Into<String>, base_url: impl Into<String>) -> PortResult<Self> {
        let http = Octocrab::builder()
            .personal_token(token.into())
            .base_uri(base_url.into())
            .map_err(|e| PortError::Backend(format!("github base_uri: {e}")))?
            .build()
            .map_err(|e| PortError::Backend(format!("github client build: {e}")))?;
        Ok(Self { http })
    }

    pub(crate) async fn create_issue(
        &self,
        cmd: RemoteTaskCreate<'_>,
    ) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(cmd.canonical_repo)?;
        let issue = self
            .http
            .issues(owner, repo)
            .create(cmd.title)
            .body(cmd.body)
            .assignees(cmd.assignees.to_vec())
            .labels(cmd.labels.to_vec())
            .send()
            .await
            .map_err(map_err)?;
        Ok(map_issue(issue))
    }

    pub(crate) async fn update_issue(
        &self,
        cmd: RemoteTaskUpdate<'_>,
    ) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(cmd.canonical_repo)?;
        let number = parse_issue_number(cmd.remote_id)?;
        let handler = self.http.issues(owner, repo);
        // Builders consume `self`, so partial updates reassign as we go —
        // each field is only set when the caller supplied it.
        let mut builder = handler.update(number);
        if let Some(title) = cmd.title {
            builder = builder.title(title);
        }
        if let Some(body) = cmd.body {
            builder = builder.body(body);
        }
        if let Some(closed) = cmd.closed {
            builder = builder.state(if closed {
                IssueState::Closed
            } else {
                IssueState::Open
            });
            // `state_reason` only annotates a state transition, so it rides
            // along with `state` — never on a title/body-only patch.
            if let Some(reason) = cmd.state_reason {
                builder = builder.state_reason(map_state_reason(reason));
            }
        }
        let issue = builder.send().await.map_err(map_err)?;
        Ok(map_issue(issue))
    }

    pub(crate) async fn fetch_issue(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<RemoteTaskSnapshot> {
        let (owner, repo) = split_owner_repo(canonical_repo)?;
        let number = parse_issue_number(remote_id)?;
        let issue = self
            .http
            .issues(owner, repo)
            .get(number)
            .await
            .map_err(map_err)?;
        Ok(map_issue(issue))
    }

    /// List the direct sub-issues of an issue. octocrab has no typed handler
    /// for `/sub_issues`, so we hit it via the generic `get` and decode into
    /// the typed `Issue` model. Each child carries its own canonical repo
    /// (derived from `repository_url`) since sub-issues can live in another
    /// repo. One level only — the caller recurses.
    pub(crate) async fn fetch_sub_issues(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<Vec<RemoteChildIssue>> {
        let (owner, repo) = split_owner_repo(canonical_repo)?;
        let number = parse_issue_number(remote_id)?;
        // Page through until a short page (or the safety cap), so issues with
        // more than one page of direct sub-issues aren't silently truncated.
        const PER_PAGE: usize = 100;
        const MAX_PAGES: u32 = 50;
        let mut issues: Vec<Issue> = Vec::new();
        let mut cap_page_full = false;
        for page in 1..=MAX_PAGES {
            let route = format!(
                "/repos/{owner}/{repo}/issues/{number}/sub_issues?per_page={PER_PAGE}&page={page}"
            );
            let batch: Vec<Issue> = self.http.get(route, None::<&()>).await.map_err(map_err)?;
            let full = batch.len() == PER_PAGE;
            issues.extend(batch);
            if !full {
                break;
            }
            if page == MAX_PAGES {
                cap_page_full = true;
            }
        }
        // A full final page isn't proof of overflow (could be exactly
        // MAX_PAGES * PER_PAGE). Probe one more lightweight page to tell an
        // exact boundary from genuine truncation.
        if cap_page_full {
            let probe_route =
                format!("/repos/{owner}/{repo}/issues/{number}/sub_issues?per_page=1&page={}", MAX_PAGES + 1);
            let probe: Vec<Issue> = self.http.get(probe_route, None::<&()>).await.map_err(map_err)?;
            if !probe.is_empty() {
                return Err(PortError::Backend(format!(
                    "issue {number} in {canonical_repo} has more than {} sub-issues; \
                     refusing to import a truncated tree",
                    MAX_PAGES as usize * PER_PAGE
                )));
            }
        }
        Ok(issues
            .into_iter()
            .map(|issue| {
                let child_canonical = canonical_from_repository_url(issue.repository_url.as_str())
                    .unwrap_or_else(|| canonical_repo.to_string());
                RemoteChildIssue {
                    canonical_repo: child_canonical,
                    snapshot: map_issue(issue),
                }
            })
            .collect())
    }

    /// List an issue's comments, oldest first, paging through the typed
    /// `list_comments` handler. Caps pages like `fetch_sub_issues` and
    /// surfaces a cap-hit rather than silently truncating.
    pub(crate) async fn fetch_comments(
        &self,
        canonical_repo: &str,
        remote_id: &str,
    ) -> PortResult<Vec<RemoteComment>> {
        let (owner, repo) = split_owner_repo(canonical_repo)?;
        let number = parse_issue_number(remote_id)?;
        const PER_PAGE: u8 = 100;
        const MAX_PAGES: u32 = 50;
        let mut out: Vec<RemoteComment> = Vec::new();
        let mut cap_page_full = false;
        for page in 1..=MAX_PAGES {
            let batch = self
                .http
                .issues(owner.as_str(), repo.as_str())
                .list_comments(number)
                .per_page(PER_PAGE)
                .page(page)
                .send()
                .await
                .map_err(map_err)?;
            let full = batch.items.len() == PER_PAGE as usize;
            for c in batch.items {
                out.push(RemoteComment {
                    remote_id: c.id.to_string(),
                    author: c.user.login,
                    body: c.body.unwrap_or_default(),
                    created_at: Timestamp::from_utc(c.created_at),
                });
            }
            if !full {
                break;
            }
            if page == MAX_PAGES {
                cap_page_full = true;
            }
        }
        // A full final page isn't proof of overflow (it could be exactly
        // MAX_PAGES * PER_PAGE). Probe one more lightweight page to tell an
        // exact boundary from genuine truncation.
        if cap_page_full
            && !self
                .http
                .issues(owner.as_str(), repo.as_str())
                .list_comments(number)
                .per_page(1)
                .page(MAX_PAGES + 1)
                .send()
                .await
                .map_err(map_err)?
                .items
                .is_empty()
        {
            return Err(PortError::Backend(format!(
                "issue {number} in {canonical_repo} has more than {} comments; \
                 refusing to mirror a truncated set",
                MAX_PAGES as usize * PER_PAGE as usize
            )));
        }
        Ok(out)
    }
}

// ---------- Mapping (octocrab models ↔ port types) ----------------------

fn map_issue(issue: Issue) -> RemoteTaskSnapshot {
    RemoteTaskSnapshot {
        remote_id: issue.number.to_string(),
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        closed: matches!(issue.state, IssueState::Closed),
        updated_at: Timestamp::from_utc(issue.updated_at),
        assignees: issue.assignees.into_iter().map(|u| u.login).collect(),
        labels: issue.labels.into_iter().map(|l| l.name).collect(),
    }
}

/// Map the provider-agnostic [`RemoteStateReason`] to octocrab's typed
/// `IssueStateReason`. Lives here because it's REST-API-shaped; the GraphQL
/// adapter uses a different enum vocabulary and will have its own mapping.
fn map_state_reason(reason: RemoteStateReason) -> IssueStateReason {
    match reason {
        RemoteStateReason::Completed => IssueStateReason::Completed,
        RemoteStateReason::NotPlanned => IssueStateReason::NotPlanned,
        RemoteStateReason::Duplicate => IssueStateReason::Duplicate,
        RemoteStateReason::Reopened => IssueStateReason::Reopened,
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

/// Derive the canonical `github.com/<owner>/<repo>` form from a GitHub API
/// `repository_url` (e.g. `https://api.github.com/repos/o/r`). Returns `None`
/// for shapes that don't contain a `/repos/<owner>/<repo>` segment.
fn canonical_from_repository_url(url: &str) -> Option<String> {
    let rest = url.split("/repos/").nth(1)?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("github.com/{owner}/{repo}"))
}

fn parse_issue_number(remote_id: &str) -> PortResult<u64> {
    remote_id
        .parse::<u64>()
        .map_err(|_| PortError::Backend(format!("invalid github issue number: {remote_id}")))
}

/// Translate an `octocrab::Error` into a [`PortError`], preserving the
/// status-code → variant mapping the application layer relies on. A GitHub
/// API error carries an HTTP status; everything else (transport, decode) is
/// a network-class failure.
fn map_err(e: octocrab::Error) -> PortError {
    match e {
        octocrab::Error::GitHub { source, .. } => {
            let message = source.message.clone();
            match source.status_code.as_u16() {
                404 => PortError::NotFound(message),
                409 | 422 => PortError::Conflict {
                    target: None,
                    message,
                },
                code => PortError::Network(format!("github {code}: {message}")),
            }
        }
        other => PortError::Network(other.to_string()),
    }
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
    fn canonical_from_repository_url_extracts_owner_repo() {
        assert_eq!(
            canonical_from_repository_url("https://api.github.com/repos/o/r").as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(
            canonical_from_repository_url("https://api.github.com/repos/acme/backend").as_deref(),
            Some("github.com/acme/backend")
        );
        assert_eq!(canonical_from_repository_url("https://example.com/x"), None);
    }

    #[test]
    fn maps_state_reason_to_octocrab_enum() {
        assert!(matches!(
            map_state_reason(RemoteStateReason::Completed),
            IssueStateReason::Completed
        ));
        assert!(matches!(
            map_state_reason(RemoteStateReason::NotPlanned),
            IssueStateReason::NotPlanned
        ));
        assert!(matches!(
            map_state_reason(RemoteStateReason::Duplicate),
            IssueStateReason::Duplicate
        ));
        assert!(matches!(
            map_state_reason(RemoteStateReason::Reopened),
            IssueStateReason::Reopened
        ));
    }
}
