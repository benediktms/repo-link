//! `rl here` dispatch — resolves the cwd's full working context (repo
//! binding, every workspace membership, filing repo, sibling roster) in
//! one shot. See `crate::cli::Cmd::Here`.

use anyhow::{Result, anyhow};
use domain_core::RepoOriginId;
use dto_shared::{HereMatchDto, HereRepoSummaryDto, HereResponseDto};
use infra_git::{GitError, discover_canonical};

use crate::render;
use crate::services::Services;

pub(crate) async fn here_dispatch(svc: &Services) -> Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow!("failed to determine current directory: {e}"))?;
    let abs = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let query_path = abs.display().to_string();

    // Only "not a git repo" (or "git repo with no origin") maps to null —
    // legitimate no-matches. Any other error (git binary missing, I/O
    // failure, permission denied) propagates so it isn't mistaken for one.
    let canonical_url = match discover_canonical(&abs) {
        Err(GitError::NotARepo(_)) | Ok(None) => None,
        Err(e) => return Err(anyhow!("{e}")),
        Ok(Some(c)) => Some(c),
    };

    let mut matches = Vec::new();
    if let Some(canonical) = canonical_url.as_deref() {
        // Archived workspaces are always excluded — `rl here` is a
        // session-start command, not `rl repo locate`'s opt-in search.
        let memberships = svc
            .bindings
            .memberships_for_canonical_url(canonical, false)
            .await?;
        for membership in memberships {
            let roster = svc
                .bindings
                .list(&membership.workspace.id)
                .await?
                .iter()
                .filter(|b| b.id != membership.binding.id)
                .map(HereRepoSummaryDto::from)
                .collect();

            // A filing origin can outlive its detached instance, so resolve
            // it via the service rather than by scanning the roster above.
            let filing_repo = match membership.workspace.filing_repo_id.as_deref() {
                Some(id) => {
                    let origin_id: RepoOriginId = id
                        .parse()
                        .map_err(|e| anyhow!("invalid filing_repo_id {id:?}: {e}"))?;
                    svc.bindings.resolve_filing_origin(origin_id).await?
                }
                None => None,
            };

            matches.push(HereMatchDto {
                workspace: membership.workspace,
                repo: HereRepoSummaryDto::from(&membership.binding),
                filing_repo,
                roster,
            });
        }
    }

    render::here(&HereResponseDto {
        query_path,
        canonical_url,
        matches,
    });
    Ok(())
}
