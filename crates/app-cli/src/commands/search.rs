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

/// Build the pinned embedding provider when its prepared profile is present
/// in the model cache (RFC 0007 D7); None when the model is not prepared, so
/// the search reports `model_not_prepared` instead of failing.
fn maybe_build_embedder() -> Result<Option<std::sync::Arc<dyn ports::EmbeddingProvider>>> {
    use std::sync::Arc;
    let manifest = infra_embed::profiles::profile();
    let cache_root = infra_config::default_model_cache_root()
        .map_err(|e| anyhow!("resolve model cache root: {e}"))?;
    let dir = cache_root.join(&manifest.profile_id);
    if !dir.exists() {
        return Ok(None);
    }
    let config = infra_embed::model::EmbedConfig {
        pooling: infra_embed::model::Pooling::Mean,
        corpus_prefix: None,
        query_prefix: None,
        dims: 384,
        max_input_tokens: 512,
    };
    let provider = infra_embed::provider::CandleEmbeddingProvider::new(&manifest.profile_id, &dir, config)
        .map_err(|e| anyhow!("load model: {e}"))?;
    Ok(Some(Arc::new(provider)))
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
    let mut service = TaskSearchService::new(svc.search_source.clone(), index);
    if let Some(embedder) = maybe_build_embedder()? {
        service = service.with_embedder(embedder);
    }
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
            // Report an unavailable sidecar rather than failing: status is the
            // diagnostic surface for exactly that condition (RFC 0007 D8).
            let stats = index.stats().await.ok();
            let sidecar_available = stats.as_ref().map(|s| s.sidecar_available).unwrap_or(false);
            let (lex_available, lex_reason) = match stats {
                Some(_) => {
                    let meta = index.metadata().await.ok();
                    match (
                        meta.as_ref().and_then(|m| m.available.as_ref()),
                        meta.as_ref().and_then(|m| m.schema_mismatch.as_ref()),
                    ) {
                        (None, _) => (false, Some(LexicalUnavailableReasonDto::SidecarUnavailable)),
                        (Some(_), Some(m)) if m.incompatible => {
                            (false, Some(LexicalUnavailableReasonDto::SchemaMismatch))
                        }
                        _ => (true, None),
                    }
                }
                None => (false, Some(LexicalUnavailableReasonDto::SidecarUnavailable)),
            };
            let model_prepared = maybe_build_embedder()?.is_some();
            let (sem_available, sem_reason) = if !lex_available {
                (false, Some(SemanticSkippedReasonDto::LexicalIndexUnavailable))
            } else if !model_prepared {
                (false, Some(SemanticSkippedReasonDto::ModelNotPrepared))
            } else {
                (true, None)
            };
            render::search_index_status(&SearchIndexStatusDto {
                lexical_available: lex_available,
                semantic_available: sem_available,
                lexical_unavailable_reason: lex_reason,
                semantic_skipped_reason: sem_reason,
                chunk_count: stats.as_ref().map(|s| s.chunk_count).unwrap_or(0),
                vector_count: stats.as_ref().map(|s| s.vector_count).unwrap_or(0),
                fts_integrity_ok: stats.as_ref().map(|s| s.fts_integrity_ok).unwrap_or(false),
                sidecar_size_bytes: stats.as_ref().map(|s| s.sidecar_size_bytes).unwrap_or(0),
                sidecar_available: Some(sidecar_available),
            });
        }
        SearchIndexCmd::Rebuild { .. } => {
            let mut service = TaskSearchService::new(svc.search_source.clone(), index);
            if let Some(embedder) = maybe_build_embedder()? {
                service = service.with_embedder(embedder);
            }
            let written = service.rebuild().await?;
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
            let manifest = infra_embed::profiles::profile();
            let cache_root = infra_config::default_model_cache_root()
                .map_err(|e| anyhow!("resolve model cache root: {e}"))?;
            let dir = match infra_embed::prepare::prepare(&manifest, &cache_root) {
                Ok(dir) => dir,
                Err(infra_embed::prepare::PrepareError::AlreadyPrepared { path, .. }) => path,
                Err(e) => return Err(anyhow!("prepare-model: {e}")),
            };
            render::search_model_status(&SearchModelStatusDto {
                prepared: true,
                profile_id: Some(manifest.profile_id.clone()),
                dimensions: Some(384),
            });
            eprintln!("model cache: {}", dir.display());
        }
    }
    Ok(())
}
