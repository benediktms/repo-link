//! `rl project` dispatch — GitHub Projects v2 management. `link` fetches a
//! project's schema from GitHub over GraphQL (Stage 5); the other verbs are
//! local-only reads/edits of the mirrored project.

use anyhow::{Result, anyhow};
use dto_shared::MapStatusCmd;
use infra_config::RepoLinkConfig;
use ports::RemoteProjectProvider;

use crate::cli::ProjectCmd;
use crate::services::{Services, build_github_provider, require_github_token};

pub(crate) async fn project_dispatch(
    cmd: ProjectCmd,
    svc: &Services,
    cfg: &RepoLinkConfig,
) -> Result<()> {
    match cmd {
        ProjectCmd::Link { target } => {
            let (owner, number) = parse_owner_number(&target)?;
            // Fetch the live schema over GraphQL, then let the service
            // auto-derive the status mapping and persist.
            let token = require_github_token(cfg, "project link")?;
            let provider = build_github_provider(&token, cfg).map_err(|e| anyhow!("{e}"))?;
            let snapshot = provider
                .fetch_project(&owner, number)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            let dto = svc
                .projects
                .link_from_snapshot(snapshot)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dto)?);
        }
        ProjectCmd::List => {
            let dtos = svc.projects.list().await.map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dtos)?);
        }
        ProjectCmd::Show { spec } => {
            let dto = svc.projects.get(&spec).await.map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dto)?);
        }
        ProjectCmd::Map {
            spec,
            local,
            option_id,
        } => {
            let dto = svc
                .projects
                .map_status(MapStatusCmd {
                    project_spec: spec,
                    status: local,
                    option_id,
                })
                .await
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::to_string_pretty(&dto)?);
        }
        ProjectCmd::Unlink { spec } => {
            svc.projects
                .unlink(&spec)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            println!("{}", serde_json::json!({ "unlinked": spec }));
        }
    }
    Ok(())
}

/// Parse a `<owner>/<number>` project target (e.g. `benediktms/3`).
fn parse_owner_number(target: &str) -> Result<(String, u64)> {
    let (owner, number_str) = target.split_once('/').ok_or_else(|| {
        anyhow!("expected `<owner>/<number>` (e.g. benediktms/3), got {target:?}")
    })?;
    if owner.is_empty() {
        return Err(anyhow!("project owner must be non-empty, got {target:?}"));
    }
    let number: u64 = number_str.parse().map_err(|_| {
        anyhow!("invalid project number in {target:?}: expected a positive integer")
    })?;
    Ok((owner.to_string(), number))
}
