//! Per-area command dispatch. Each submodule owns the handler(s) for one
//! `Cmd` branch; `dispatch.rs` fans out to them. Shared helpers used by more
//! than one submodule live here at the module root.

pub(crate) mod agents;
pub(crate) mod gh;
pub(crate) mod here;
pub(crate) mod org;
pub(crate) mod project;
pub(crate) mod query;
pub(crate) mod repo;
pub(crate) mod search;
pub(crate) mod sync;
pub(crate) mod task;
pub(crate) mod workspace;

/// Discover the canonical URL for a checkout path. Only "not a git repo"
/// (or "git repo with no origin") maps to `None` — those are legitimate
/// no-matches. Any other error (git binary missing, I/O failure, permission
/// denied) propagates so callers can distinguish broken tooling from an
/// unmapped path.
pub(crate) fn discover_canonical_or_none(abs: &std::path::Path) -> anyhow::Result<Option<String>> {
    match infra_git::discover_canonical(abs) {
        Err(infra_git::GitError::NotARepo(_)) | Ok(None) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{e}")),
        Ok(Some(c)) => Ok(Some(c)),
    }
}

/// Parse the GitHub `{owner}` out of a canonical repo URL of the form
/// `github.com/{owner}/{repo}` (see `infra_git::parse_canonical`). Returns
/// `None` for anything else — a non-GitHub host, or a malformed value —
/// so callers can treat "not GitHub" as a silent no-op rather than an
/// error. Shared by the `rl repo attach` org issue-type auto-populate and
/// `rl org fetch-issue-types`'s cwd-derived owner fallback.
pub(crate) fn github_owner_from_canonical(canonical: &str) -> Option<String> {
    let mut parts = canonical.splitn(3, '/');
    let host = parts.next()?;
    if host != "github.com" {
        return None;
    }
    let owner = parts.next()?;
    if owner.is_empty() {
        return None;
    }
    Some(owner.to_string())
}

/// Print a JSON ambiguous-handle error to stderr and exit with code 2.
/// Used by any resolver command when `ServiceError::AmbiguousHandle` fires.
pub(crate) fn handle_ambiguous(
    query: String,
    candidates: Vec<application_workspace::AmbiguousCandidate>,
) -> ! {
    let body = serde_json::json!({
        "error": "ambiguous",
        "query": query,
        "candidates": candidates,
    });
    eprintln!("{body}");
    std::process::exit(2);
}
