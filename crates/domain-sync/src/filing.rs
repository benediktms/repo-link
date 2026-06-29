//! The RFC 0002 **D2 filing-repo resolution chain** — the single, shared rule
//! for deciding *where a task's backing GitHub issue is filed*.
//!
//! It is a pure function on three already-fetched inputs; the I/O of reading
//! the workspace default or the per-task override stays with the caller. Both
//! `application-sync` (promote / REST create) and `application-task`
//! (enqueue / draft conversion) call this same function, so the step-4 NULL
//! semantics live in exactly one place and cannot drift between call sites.

use domain_core::RepoOriginId;

/// Resolve the filing repo by RFC 0002 §D2 precedence:
///
/// 1. an **explicit per-task override**, else
/// 2. the **workspace default origin id** (`workspaces.filing_repo_id`), else
/// 3. the task's **logical origin id** if set, else
/// 4. **`None`** — no filing repo, i.e. a project-board draft.
///
/// This is `explicit_override.or(workspace_default).or(logical_origin)`, but it
/// is named and documented because the chain is called from more than one
/// crate and the two NULL edge cases are easy to get subtly wrong:
///
/// - **Orphan + workspace default** (`logical_origin == None` but
///   `workspace_default == Some`) resolves at *step 2* to a real repo — the
///   issue is filed in the workspace default, NOT left a board draft. `.or`
///   short-circuits on the first `Some`, so this falls out naturally.
/// - **NULL fall-through**: step 4 (`None`) is reached only when all of 1–3
///   are absent. A `None` logical origin id is a *failing* step-3 condition
///   (distinct from a present-but-unused value), so it correctly continues to
///   step 4 rather than masking a higher-precedence input.
///
/// The result is recorded on `tasks.filing_repo_id` at the first filing
/// transition (promote / first board filing / draft conversion); once
/// recorded it is authoritative and never re-resolved from the workspace
/// default.
pub fn resolve_filing_repo(
    explicit_override: Option<RepoOriginId>,
    workspace_default: Option<RepoOriginId>,
    logical_origin: Option<RepoOriginId>,
) -> Option<RepoOriginId> {
    explicit_override.or(workspace_default).or(logical_origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step1_explicit_override_wins_over_everything() {
        let over = RepoOriginId::new();
        let ws = RepoOriginId::new();
        let logical = RepoOriginId::new();
        assert_eq!(
            resolve_filing_repo(Some(over), Some(ws), Some(logical)),
            Some(over)
        );
    }

    #[test]
    fn step2_workspace_default_when_no_override() {
        let ws = RepoOriginId::new();
        let logical = RepoOriginId::new();
        assert_eq!(resolve_filing_repo(None, Some(ws), Some(logical)), Some(ws));
    }

    #[test]
    fn step3_logical_when_no_override_or_workspace_default() {
        let logical = RepoOriginId::new();
        assert_eq!(
            resolve_filing_repo(None, None, Some(logical)),
            Some(logical)
        );
    }

    #[test]
    fn step4_none_when_all_absent_is_a_board_draft() {
        assert_eq!(resolve_filing_repo(None, None, None), None);
    }

    #[test]
    fn edge_orphan_plus_workspace_default_files_in_workspace_default_not_a_draft() {
        // logical (repo_id) is None — an orphan — but the workspace has a
        // default filing repo, so step 2 produces a real repo. This must NOT
        // collapse to the step-4 board-draft case.
        let ws = RepoOriginId::new();
        assert_eq!(resolve_filing_repo(None, Some(ws), None), Some(ws));
    }

    #[test]
    fn edge_override_rescues_an_orphan_with_no_workspace_default() {
        let over = RepoOriginId::new();
        assert_eq!(resolve_filing_repo(Some(over), None, None), Some(over));
    }
}
