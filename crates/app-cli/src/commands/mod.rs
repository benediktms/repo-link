//! Per-area command dispatch. Each submodule owns the handler(s) for one
//! `Cmd` branch; `dispatch.rs` fans out to them. Shared helpers used by more
//! than one submodule live here at the module root.

pub(crate) mod agents;
pub(crate) mod gh;
pub(crate) mod here;
pub(crate) mod project;
pub(crate) mod query;
pub(crate) mod repo;
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
