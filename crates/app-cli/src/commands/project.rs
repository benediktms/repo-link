//! `rl project` dispatch — GitHub Projects v2 management (local-only schema
//! in Stage 4).

use anyhow::{Result, anyhow};
use dto_shared::{LinkProjectCmd, MapStatusCmd, StatusMappingDto, StatusOptionDto};

use crate::cli::ProjectCmd;
use crate::services::Services;

pub(crate) async fn project_dispatch(cmd: ProjectCmd, svc: &Services) -> Result<()> {
    match cmd {
        ProjectCmd::Link {
            node_id,
            owner,
            number,
            title,
            status_field_id,
            options,
            mappings,
        } => {
            let status_options: Vec<StatusOptionDto> = options
                .into_iter()
                .enumerate()
                .map(|(i, (id, name))| StatusOptionDto {
                    option_id: id,
                    name,
                    // The CLI surface uses positional order as the user's
                    // intended display order; same as we'd get from the
                    // GraphQL field response in Stage 5.
                    ordinal: u32::try_from(i).unwrap_or(u32::MAX),
                    default_for: None,
                })
                .collect();
            let initial_mappings: Vec<StatusMappingDto> = mappings
                .into_iter()
                .map(|(status, option_id)| StatusMappingDto { status, option_id })
                .collect();
            let dto = svc
                .projects
                .link(LinkProjectCmd {
                    node_id,
                    owner_login: owner,
                    number,
                    title,
                    status_field_id,
                    status_options,
                    initial_mappings,
                })
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
