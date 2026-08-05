//! `rl task search` + `rl task search-index` handlers (RFC 0007 D10).

use anyhow::{Result, anyhow};
use application_search::TaskSearchService;
use domain_core::{RepoId, WorkspaceId};
use dto_shared::{
    LexicalUnavailableReasonDto, SearchIndexMaintenanceDto, SearchIndexStatusDto,
    SearchModelStatusDto, SemanticSkippedReasonDto,
};
use infra_config::RepoLinkConfig;
use infra_sqlite::SqliteTaskSearchIndex;
use ports::{TaskSearchIndex, TaskSearchSourceRepository};
use std::str::FromStr;

use crate::cli::{SearchIndexCmd, TaskCmd};
use crate::commands::repo::resolve_repo_handle_required;
use crate::render;
use crate::services::Services;

fn parse_workspace(s: Option<String>) -> Result<Option<WorkspaceId>> {
    match s {
        None => Ok(None),
        Some(s) => Ok(Some(
            WorkspaceId::from_str(&s).map_err(|e| anyhow!("invalid --workspace id: {e}"))?,
        )),
    }
}

fn status_is_open(s: &str) -> Result<Option<bool>> {
    match s {
        "all" => Ok(None),
        "open" => Ok(Some(true)),
        "closed" => Ok(Some(false)),
        other => Err(anyhow!(
            "invalid --status {other:?}: expected open | closed | all"
        )),
    }
}

pub(crate) async fn task_search_dispatch(
    cmd: TaskCmd,
    svc: &Services,
    cfg: &RepoLinkConfig,
) -> Result<()> {
    let TaskCmd::Search { args } = cmd else {
        unreachable!("task_search_dispatch only receives TaskCmd::Search");
    };

    let workspace_id = parse_workspace(args.ws.workspace)?;
    let repo_id: Option<RepoId> = match args.repo {
        Some(h) => {
            let id = resolve_repo_handle_required(svc, &h).await?;
            Some(RepoId::from_str(&id).map_err(|e| anyhow!("repo resolved to invalid id: {e}"))?)
        }
        None => None,
    };

    let index = SqliteTaskSearchIndex::new(&cfg.database_path);
    let service = TaskSearchService::new(svc.search_source.clone(), index);
    let resp = service
        .search(&application_search::SearchRequest {
            query: args.query,
            scope: ports::SearchScope {
                workspace_id,
                repo_id,
                is_open: args
                    .status
                    .as_deref()
                    .map(status_is_open)
                    .transpose()?
                    .flatten(),
            },
            exact: args.exact,
            limit: args.limit.unwrap_or(10),
        })
        .await?;
    render::search(&resp);
    Ok(())
}

pub(crate) async fn search_index_dispatch(
    cmd: SearchIndexCmd,
    svc: &Services,
    cfg: &RepoLinkConfig,
) -> Result<()> {
    let index = SqliteTaskSearchIndex::new(&cfg.database_path);
    match cmd {
        SearchIndexCmd::Status { .. } => {
            let stats = index.stats().await?;
            let meta = index.metadata().await?;
            let (lex_available, lex_reason) = match (&meta.available, &meta.schema_mismatch) {
                (None, _) => (false, Some(LexicalUnavailableReasonDto::SidecarUnavailable)),
                (Some(_), Some(m)) if m.incompatible => {
                    (false, Some(LexicalUnavailableReasonDto::SchemaMismatch))
                }
                _ => (true, None),
            };
            render::search_index_status(&SearchIndexStatusDto {
                lexical_available: lex_available,
                semantic_available: false,
                lexical_unavailable_reason: lex_reason,
                semantic_skipped_reason: Some(SemanticSkippedReasonDto::ModelNotPrepared),
                chunk_count: stats.chunk_count,
                vector_count: stats.vector_count,
                fts_integrity_ok: stats.fts_integrity_ok,
                sidecar_size_bytes: stats.sidecar_size_bytes,
                sidecar_available: Some(stats.sidecar_available),
            });
        }
        SearchIndexCmd::Rebuild { .. } => {
            let rows = svc.search_source.load_reconcile_snapshot().await?;
            let targets: Vec<ports::ChunkTarget> = rows
                .iter()
                .flat_map(application_search::chunk_task)
                .collect();
            let written = index.rebuild(&targets).await?;
            render::search_index_maintenance(&SearchIndexMaintenanceDto {
                command: "rebuild".into(),
                chunks_written: Some(written),
                error: None,
            });
        }
        SearchIndexCmd::Clear { .. } => {
            index.clear().await?;
            render::search_index_maintenance(&SearchIndexMaintenanceDto {
                command: "clear".into(),
                chunks_written: None,
                error: None,
            });
        }
        SearchIndexCmd::PrepareModel { .. } => {
            // PR1 ships with no model; report the not-prepared state (D8).
            render::search_model_status(&SearchModelStatusDto {
                prepared: false,
                profile_id: None,
                dimensions: None,
            });
        }
    }
    Ok(())
}
