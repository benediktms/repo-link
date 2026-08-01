//! `rl org` dispatch — org-level GitHub resources. Currently just the
//! native issue-type registry (RFC 0006 D5): `rl project link` only ever
//! refreshed this for the *project* owner (#226); `rl org fetch-issue-types`
//! is a standalone trigger for any owner, e.g. the *filing repo* owner
//! #228's Type sync resolves against.

use anyhow::{Result, anyhow};
use infra_config::RepoLinkConfig;

use crate::cli::OrgCmd;
use crate::commands::{discover_canonical_or_none, github_owner_from_canonical};
use crate::services::{
    Services, build_github_provider, refresh_org_issue_types, require_github_token,
};

pub(crate) async fn org_dispatch(cmd: OrgCmd, svc: &Services, cfg: &RepoLinkConfig) -> Result<()> {
    match cmd {
        OrgCmd::FetchIssueTypes { owner } => {
            let owner = match owner {
                Some(owner) => owner,
                None => cwd_github_owner()?,
            };

            let token = require_github_token(cfg, "org fetch-issue-types")?;
            let provider = build_github_provider(&token, cfg).map_err(|e| anyhow!("{e}"))?;
            let refreshed = refresh_org_issue_types(svc, &provider, &owner).await?;

            println!(
                "{}",
                serde_json::json!({
                    "owner": owner,
                    "available": refreshed.available,
                    "types": refreshed.names,
                })
            );
            if !refreshed.available {
                eprintln!(
                    "no native issue types for {owner} — user account or the org feature is disabled"
                );
            }
        }
    }
    Ok(())
}

/// Derive `{owner}` from the current directory's GitHub git origin, for the
/// `rl org fetch-issue-types` no-`<owner>` form. Mirrors how other cwd-aware
/// commands (`rl here`, `rl agents docs`) resolve context.
fn cwd_github_owner() -> Result<String> {
    let cwd =
        std::env::current_dir().map_err(|e| anyhow!("failed to read current directory: {e}"))?;
    let abs = dunce::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let canonical = discover_canonical_or_none(&abs)?;
    canonical
        .as_deref()
        .and_then(github_owner_from_canonical)
        .ok_or_else(|| {
            anyhow!(
                "org fetch-issue-types requires <owner> — the current directory isn't a \
                 GitHub checkout with a resolvable origin"
            )
        })
}
