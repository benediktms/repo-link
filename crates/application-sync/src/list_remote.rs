//! `rl sync list-remote` result types (#3).
//!
//! Read-only discovery of GitHub issues that have no local task. The service
//! lists issues across the workspace's relevant repo(s) and marks each row
//! `tracked` (a local task already mirrors it) or `untracked` (an import
//! candidate). Repo selection + grouping live in [`crate::SyncService`]; the
//! CLI owns the cwd / `--repo` precedence and passes the resolved override in.

use domain_core::Timestamp;
use serde::Serialize;

/// Why a repo appears in a `list-remote` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListRemoteRole {
    /// The workspace filing default (where issues land by default).
    Filing,
    /// A bound canonical/logical code repo.
    Canonical,
    /// An explicit single-repo query (`--repo` or cwd fallback) — no grouping.
    Repo,
}

/// One remote issue and whether a local task already mirrors it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteIssueRow {
    pub remote_id: String,
    pub title: String,
    pub closed: bool,
    pub updated_at: Timestamp,
    /// True iff a local task already mirrors this issue (filing repo +
    /// provider + `remote_id` match).
    pub tracked: bool,
}

/// One repo's worth of remote issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListRemoteGroup {
    /// Human handle for the repo (the origin's `name`).
    pub repo: String,
    /// Canonical `github.com/<owner>/<repo>` the issues were listed from.
    pub canonical_url: String,
    pub role: ListRemoteRole,
    pub issues: Vec<RemoteIssueRow>,
}

/// The full `list-remote` result: one group per queried repo, filing default
/// first in the workspace-wide case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListRemoteDto {
    pub groups: Vec<ListRemoteGroup>,
}
