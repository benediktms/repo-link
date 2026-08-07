//! TaskSearchService — RFC 0007 Stage 1 orchestration.
//!
//! Every search: (1) derives the lexical-availability decision from sidecar
//! metadata (D8), (2) runs the full-corpus reconcile (D6), (3) opens one
//! authoritative result snapshot (D4), (4) runs the raw-text literal lane and
//! the FTS5 lexical lane, (5) collapses + reranks + RRF-fuses (D4), and
//! (6) assembles immutable response DTOs inside the same snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use domain_core::TaskId;
use dto_shared::{
    LexicalUnavailableReasonDto, MatchedSourceDto, MatchedSourceKindDto, QueryModeDto,
    SearchMatchDto, SearchResultDto, SemanticSkippedReasonDto, TaskSearchResponseDto,
};
use sha2::{Digest, Sha256};

use crate::chunker::chunk_task;
use crate::fold::{excerpt, find_literal_spans};
use crate::lane::{order_fused, rrf_fuse};
use crate::query_mode::{QueryMode, classify, identifier_tokens};
use ports::{
    ChunkTarget, EmbeddingProvider, GuardedVectorRow, LexicalRank, ReconcileFailure, SearchScope,
    SemanticRank, TaskSearchIndex, TaskSearchResultSnapshot, TaskSearchSourceRepository,
    TaskTextRow,
};

/// Search parameters from the CLI (RFC 0007 D10).
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub scope: SearchScope,
    pub exact: bool,
    pub limit: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("empty query")]
    EmptyQuery,
    #[error("--limit must be > 0")]
    ZeroLimit,
    #[error("search backend: {0}")]
    Source(#[from] ports::PortError),
}

/// Result of a vector-fill pass (RFC 0007 D6/D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillOutcome {
    /// Every chunk has a vector.
    Complete,
    /// The batch cap was reached with work still outstanding.
    Incomplete,
    /// An embed or store failure; the lane must degrade (D8).
    Failed,
}

/// The RFC 0007 D6 reconcile + D4 retrieval orchestrator.
pub struct TaskSearchService<S: TaskSearchSourceRepository, I: TaskSearchIndex> {
    source: S,
    index: I,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

impl<S: TaskSearchSourceRepository, I: TaskSearchIndex> TaskSearchService<S, I> {
    pub fn new(source: S, index: I) -> Self {
        Self {
            source,
            index,
            embedder: None,
        }
    }

    /// Attach a prepared embedding provider (RFC 0007 D7). Without one, the
    /// semantic lane reports `model_not_prepared` (PR1 behaviour).
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Explicit `rebuild`: re-derive the sidecar lexical rows, then — when an
    /// embedder is attached — fill every chunk's vectors in guarded batches
    /// as part of the same command (RFC 0007 D10: rebuild "fills guarded
    /// vectors in batches"). Returns (chunks written, vectors fully filled).
    /// A fill failure degrades the semantic lane but leaves lexical search
    /// intact, and is surfaced to the caller rather than swallowed.
    pub async fn rebuild(&self) -> Result<(u64, bool), SearchError> {
        let rows = self.source.load_reconcile_snapshot().await?;
        let targets: Vec<ChunkTarget> = rows.iter().flat_map(chunk_task).collect();
        let written = self.index.rebuild(&targets).await?;
        let filled = match &self.embedder {
            Some(embedder) => {
                matches!(
                    self.fill_semantic_vectors(&self.index, embedder.as_ref(), usize::MAX)
                        .await,
                    FillOutcome::Complete
                )
            }
            None => false,
        };
        Ok((written, filled))
    }

    pub async fn search(&self, req: &SearchRequest) -> Result<TaskSearchResponseDto, SearchError> {
        let query = req.query.trim();
        if query.is_empty() {
            return Err(SearchError::EmptyQuery);
        }
        if req.limit == 0 {
            return Err(SearchError::ZeroLimit);
        }
        let mode = classify(query, req.exact);

        let mut lexical_available = true;
        let mut lexical_reason: Option<LexicalUnavailableReasonDto> = None;

        let meta = match self.index.metadata().await {
            Ok(m) => Some(m),
            Err(_) => {
                lexical_available = false;
                lexical_reason = Some(LexicalUnavailableReasonDto::SidecarUnavailable);
                None
            }
        };
        if let Some(m) = &meta
            && m.schema_mismatch.as_ref().is_some_and(|s| s.incompatible)
        {
            lexical_available = false;
            lexical_reason = Some(LexicalUnavailableReasonDto::SchemaMismatch);
        }

        // Begin reconcile only when the sidecar is usable. A source read
        // failure (authoritative DB) is a hard error and propagates.
        if lexical_available {
            let src_rows = self.source.load_reconcile_snapshot().await?;
            let targets: Vec<ChunkTarget> = src_rows.iter().flat_map(chunk_task).collect();
            let projected = projected_bytes(&targets);
            let fingerprint = content_fingerprint(&targets);
            let session = match self.index.begin_reconcile().await {
                Ok(s) => Some(s),
                Err(_) => {
                    lexical_available = false;
                    lexical_reason = Some(LexicalUnavailableReasonDto::SidecarUnavailable);
                    None
                }
            };
            if let Some(mut s) = session {
                let outcome = match s.diff_chunks(&targets, projected).await {
                    Ok(_) => match s.commit(&fingerprint).await {
                        Ok(()) => Ok(()),
                        Err(_) => Err(Some(LexicalUnavailableReasonDto::ReconciliationFailed)),
                    },
                    Err(ReconcileFailure { reason }) => Err(Some(reason)),
                };
                if let Err(reason) = outcome {
                    // Always release the raw `BEGIN IMMEDIATE` transaction
                    // before the session's pool connection is returned; a
                    // live open transaction on the single-connection pool
                    // would poison every later acquire.
                    let _ = s.rollback().await;
                    lexical_available = false;
                    lexical_reason = reason;
                }
            }
        }

        let mut semantic_available = false;
        let mut semantic_reason: Option<SemanticSkippedReasonDto> = None;
        if let Some(embedder) = &self.embedder {
            if !lexical_available {
                semantic_reason = Some(SemanticSkippedReasonDto::LexicalIndexUnavailable);
            } else {
                let profile_id = embedder.profile_id();
                let sidecar_profile = meta
                    .as_ref()
                    .and_then(|m| m.available.as_ref())
                    .and_then(|s| s.embedding_profile_id.clone());
                match sidecar_profile {
                    Some(p) if p == profile_id => semantic_available = true,
                    Some(_) => semantic_reason = Some(SemanticSkippedReasonDto::ProfileMismatch),
                    None => match self.index.claim_empty_profile(&profile_id).await {
                        Ok(true) => semantic_available = true,
                        _ => semantic_reason = Some(SemanticSkippedReasonDto::ProfileMismatch),
                    },
                }
            }
        } else {
            semantic_reason = Some(if !lexical_available {
                SemanticSkippedReasonDto::LexicalIndexUnavailable
            } else {
                SemanticSkippedReasonDto::ModelNotPrepared
            });
        }

        if semantic_available {
            let embedder = self.embedder.as_ref().expect("checked above");
            // Bound the interactive path: at most one batch per search so a
            // rebuild-free query never stalls on a large missing set. The
            // lane runs only when coverage is complete (D8).
            match self
                .fill_semantic_vectors(&self.index, embedder.as_ref(), 1)
                .await
            {
                FillOutcome::Complete => {}
                // Incomplete (bounded batch left work) or Failed: never rank
                // against partial vector coverage (RFC 0007 D8).
                FillOutcome::Incomplete | FillOutcome::Failed => {
                    semantic_available = false;
                    semantic_reason = Some(SemanticSkippedReasonDto::EmbeddingFailed);
                }
            }
        }

        let snapshot = self.source.begin_result_snapshot(&req.scope).await?;
        let rows = snapshot.eligible_rows().await?;

        let literal = literal_lane(&rows, query, mode);

        let mut lexical: Vec<LexicalRank> = Vec::new();
        if lexical_available {
            let eligible: Vec<TaskId> = rows.iter().map(|r| r.task_id).collect();
            let expr = build_match_expr(query);
            lexical = self.index.search_lexical(&expr, &eligible).await?;
        }

        let mut semantic: Vec<SemanticRank> = Vec::new();
        if semantic_available {
            let embedder = self.embedder.as_ref().expect("checked above");
            if embedder
                .plan_semantic_inputs(query)
                .map(|inputs| inputs.len() > 1)
                .unwrap_or(true)
            {
                semantic_available = false;
                semantic_reason = Some(SemanticSkippedReasonDto::QueryTooLong);
            } else {
                match embedder.embed_query(query).await {
                    Ok(qv) => {
                        let eligible: Vec<TaskId> = rows.iter().map(|r| r.task_id).collect();
                        semantic = match self.index.search_semantic(&qv, &eligible).await {
                            Ok(s) => s,
                            Err(_) => {
                                semantic_available = false;
                                semantic_reason = Some(SemanticSkippedReasonDto::EmbeddingFailed);
                                Vec::new()
                            }
                        };
                    }
                    Err(_) => {
                        semantic_available = false;
                        semantic_reason = Some(SemanticSkippedReasonDto::EmbeddingFailed);
                    }
                }
            }
        }

        self.assemble(
            snapshot.as_ref(),
            query,
            mode,
            lexical_available,
            lexical_reason,
            semantic_available,
            semantic_reason,
            &literal,
            &lexical,
            &semantic,
            req.limit,
        )
        .await
    }

    /// Fill every chunk missing a vector through the embedder, in guarded
    /// batches, stopping after `max_batches` batches. Already-stored vectors
    /// stay untouched (D8). A batch whose guards reject every row, an embed
    /// result shorter than its input, or any store failure yields
    /// [`FillOutcome::Failed`]; running out of batches first yields
    /// [`FillOutcome::Incomplete`].
    async fn fill_semantic_vectors(
        &self,
        index: &I,
        embedder: &dyn EmbeddingProvider,
        max_batches: usize,
    ) -> FillOutcome {
        const BATCH: u32 = 64;
        let mut batches = 0usize;
        loop {
            let missing = match index.missing_semantic_inputs(BATCH).await {
                Ok(m) => m,
                Err(_) => return FillOutcome::Failed,
            };
            if missing.is_empty() {
                return FillOutcome::Complete;
            }
            if batches >= max_batches {
                return FillOutcome::Incomplete;
            }
            batches += 1;
            let mut texts: Vec<String> = Vec::new();
            let mut metas: Vec<(i64, TaskId, [u8; 32], u32)> = Vec::new();
            for m in &missing {
                let inputs = match embedder.plan_semantic_inputs(&m.text) {
                    Ok(i) => i,
                    Err(_) => return FillOutcome::Failed,
                };
                for (seg, input) in inputs.iter().enumerate() {
                    texts.push(input.clone());
                    metas.push((m.search_chunk_id, m.task_id, m.content_hash, seg as u32));
                }
            }
            let vectors = match embedder.embed_inputs(&texts).await {
                Ok(v) => v,
                Err(_) => return FillOutcome::Failed,
            };
            if vectors.len() != texts.len() {
                // A short result would silently drop trailing segments and
                // leave them unvectorized forever (missing-input discovery is
                // chunk-granular). Reject the batch instead.
                return FillOutcome::Failed;
            }
            let rows: Vec<GuardedVectorRow> = texts
                .iter()
                .zip(vectors)
                .zip(metas)
                .map(
                    |((text, vector), (chunk_id, task_id, content_hash, seg))| GuardedVectorRow {
                        search_chunk_id: chunk_id,
                        task_id,
                        content_hash,
                        segment_index: seg,
                        embedding_input_hash: Sha256::digest(text.as_bytes()).into(),
                        vector,
                    },
                )
                .collect();
            let stored = match index.store_vectors_guarded(&rows).await {
                Ok(n) => n,
                Err(_) => return FillOutcome::Failed,
            };
            if stored == 0 {
                return FillOutcome::Failed;
            }
        }
    }

    /// Assemble the response DTOs. A single internal orchestrator with many
    /// inputs is clearer than an ad-hoc struct here.
    #[allow(clippy::too_many_arguments)]
    async fn assemble(
        &self,
        snapshot: &dyn TaskSearchResultSnapshot,
        query: &str,
        mode: QueryMode,
        lexical_available: bool,
        lexical_reason: Option<LexicalUnavailableReasonDto>,
        semantic_available: bool,
        semantic_reason: Option<SemanticSkippedReasonDto>,
        literal: &HashMap<TaskId, TaskLiteral>,
        lexical: &[LexicalRank],
        semantic: &[SemanticRank],
        limit: usize,
    ) -> Result<TaskSearchResponseDto, SearchError> {
        let lexical_map: HashMap<TaskId, usize> =
            lexical.iter().map(|r| (r.task_id, r.rank)).collect();
        let lexical_by_task: HashMap<TaskId, &LexicalRank> =
            lexical.iter().map(|r| (r.task_id, r)).collect();
        let semantic_rank: HashMap<TaskId, usize> = semantic
            .iter()
            .enumerate()
            .map(|(i, r)| (r.task_id, i + 1))
            .collect();
        let semantic_score: HashMap<TaskId, f32> =
            semantic.iter().map(|r| (r.task_id, r.score)).collect();

        let occ = literal_occurrence_ranking(literal);
        let mut lanes: Vec<&HashMap<TaskId, usize>> = Vec::new();
        if !lexical_map.is_empty() {
            lanes.push(&lexical_map);
        }
        if !occ.is_empty() {
            lanes.push(&occ);
        }
        if !semantic_rank.is_empty() {
            lanes.push(&semantic_rank);
        }
        let fused = if lanes.is_empty() {
            HashMap::new()
        } else {
            rrf_fuse(&lanes)
        };
        let mut ordered = order_fused(&fused);

        if mode == QueryMode::Exact || mode == QueryMode::Identifier {
            let (ahead, rest): (Vec<TaskId>, Vec<TaskId>) = ordered.into_iter().partition(|t| {
                let Some(l) = literal.get(t) else {
                    return false;
                };
                match mode {
                    QueryMode::Exact => l.full_present,
                    QueryMode::Identifier => l.full_present || l.all_tokens_present,
                    _ => false,
                }
            });
            ordered = ahead;
            ordered.extend(rest);
        }
        ordered.truncate(limit);

        let mut results = Vec::with_capacity(ordered.len());
        for (idx, tid) in ordered.iter().enumerate() {
            let identity = snapshot.task_identity(tid).await?;
            let lit = literal.get(tid);
            let lex = lexical_by_task.get(tid).copied();

            let (kind, comment_id, excerpt_text) = evidence(lit, lex, &identity.title);
            let matched = SearchMatchDto {
                literal: lit.filter(|l| l.contributed).map(|_| true),
                lexical_rank: lex.map(|r| r.rank as u64),
                semantic_rank: semantic_rank.get(tid).map(|r| *r as u64),
                semantic_score: semantic_score.get(tid).copied(),
                fused_score: fused.get(tid).map(|s| *s as f32),
            };
            results.push(SearchResultDto {
                rank: (idx + 1) as u64,
                id: identity.display_id,
                task_id: tid.to_string(),
                workspace_id: identity.workspace_id.to_string(),
                workspace_name: identity.workspace_name,
                title: identity.title,
                matched: Some(matched),
                matched_source: MatchedSourceDto {
                    kind,
                    remote_comment_id: comment_id,
                    excerpt: excerpt_text,
                },
            });
        }

        Ok(TaskSearchResponseDto {
            query: query.to_string(),
            query_mode: mode.into(),
            lexical_available,
            lexical_unavailable_reason: if lexical_available {
                None
            } else {
                lexical_reason
            },
            semantic_available,
            semantic_skipped_reason: if semantic_available {
                None
            } else {
                semantic_reason
            },
            results,
        })
    }
}

/// Per-task literal-lane aggregate (raw-text fold scan, RFC 0007 D4).
struct TaskLiteral {
    contributed: bool,
    full_present: bool,
    all_tokens_present: bool,
    occurrence: usize,
    kind: MatchedSourceKindDto,
    remote_comment_id: Option<String>,
    excerpt: String,
}

const EXCERPT_LIMIT: usize = 200;

fn literal_lane(
    rows: &[TaskTextRow],
    query: &str,
    mode: QueryMode,
) -> HashMap<TaskId, TaskLiteral> {
    let mut out = HashMap::new();
    let id_tokens = identifier_tokens(query);
    for row in rows {
        let mut occurrence = 0usize;
        let mut full_present = false;
        let mut best: Option<(MatchedSourceKindDto, Option<String>, String)> = None;
        for (text, kind, comment_id) in columns(row) {
            if text.is_empty() {
                continue;
            }
            for span in find_literal_spans(text, query) {
                full_present = true;
                occurrence += 1;
                if best.is_none() {
                    best = Some((
                        kind,
                        comment_id.clone(),
                        excerpt(text, &span, EXCERPT_LIMIT),
                    ));
                }
            }
        }
        let all_tokens_present = if mode == QueryMode::Identifier {
            !id_tokens.is_empty()
                && id_tokens.iter().all(|t| {
                    columns(row).into_iter().any(|(text, _, _)| {
                        !text.is_empty() && !find_literal_spans(text, t).is_empty()
                    })
                })
        } else {
            false
        };
        let contributed = match mode {
            QueryMode::Exact => full_present,
            QueryMode::Identifier => full_present || all_tokens_present,
            QueryMode::Natural => occurrence > 0,
        };
        let (kind, remote_comment_id, excerpt) =
            best.unwrap_or((MatchedSourceKindDto::Core, None, row.title.clone()));
        out.insert(
            row.task_id,
            TaskLiteral {
                contributed,
                full_present,
                all_tokens_present,
                occurrence,
                kind,
                remote_comment_id,
                excerpt,
            },
        );
    }
    out
}

/// Iterate a task's literal-lane columns: title/body → Core, each non-empty
/// comment → Comment (RFC 0007 D4 raw columns).
fn columns(row: &TaskTextRow) -> Vec<(&str, MatchedSourceKindDto, Option<String>)> {
    let mut v = Vec::new();
    v.push((row.title.as_str(), MatchedSourceKindDto::Core, None));
    v.push((row.body.as_str(), MatchedSourceKindDto::Core, None));
    for c in &row.comments {
        v.push((
            c.body.as_str(),
            MatchedSourceKindDto::Comment,
            c.remote_comment_id.clone(),
        ));
    }
    v
}

/// Natural-mode literal lane: rank tasks by full-query occurrence count
/// (1 = most occurrences), ties by task id (RFC 0007 D4 third lane).
fn literal_occurrence_ranking(literal: &HashMap<TaskId, TaskLiteral>) -> HashMap<TaskId, usize> {
    let mut tasks: Vec<&TaskId> = literal
        .iter()
        .filter(|(_, l)| l.occurrence > 0)
        .map(|(t, _)| t)
        .collect();
    tasks.sort_by(|a, b| {
        let oa = literal.get(a).map(|l| l.occurrence).unwrap_or(0);
        let ob = literal.get(b).map(|l| l.occurrence).unwrap_or(0);
        ob.cmp(&oa).then_with(|| a.as_uuid().cmp(&b.as_uuid()))
    });
    tasks
        .into_iter()
        .enumerate()
        .map(|(i, t)| (*t, i + 1))
        .collect()
}

/// RFC 0007 D4 FTS MATCH expression: each whitespace term double-quoted with
/// internal double quotes doubled, joined with ` OR `. Bound as a parameter
/// so no user character can become FTS syntax or SQL.
pub fn build_match_expr(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Choose result evidence: literal hit, else lexical chunk, else title.
fn evidence(
    lit: Option<&TaskLiteral>,
    lex: Option<&LexicalRank>,
    title: &str,
) -> (MatchedSourceKindDto, Option<String>, String) {
    if let Some(l) = lit
        && l.contributed
    {
        return (l.kind, l.remote_comment_id.clone(), l.excerpt.clone());
    }
    if let Some(l) = lex {
        return (l.kind, l.remote_comment_id.clone(), l.excerpt.clone());
    }
    (MatchedSourceKindDto::Core, None, title.to_string())
}

/// Coarse projected sidecar growth for the D5 storage preflight (bytes).
/// The adapter enforces the real page/`max_page_count` budgets; this is the
/// projection handed to it before any write.
fn projected_bytes(targets: &[ChunkTarget]) -> u64 {
    // PR1 has no vectors; text+FTS dominates. ~1.2 KiB per chunk is a safe
    // over-estimate for ~900-byte formatted text + FTS overhead.
    let per = 1200u64;
    (targets.len() as u64).saturating_mul(per)
}

/// SHA-256 of the desired-set signature (RFC 0007 D6 validated-integrity
/// marker). Sorted (task_id, kind, content_hash) so the fingerprint is
/// canonical regardless of row order.
fn content_fingerprint(targets: &[ChunkTarget]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<_> = targets
        .iter()
        .map(|t| (t.task_id.as_uuid(), chunk_kind_byte(t.kind), t.content_hash))
        .collect();
    entries.sort();
    let mut h = Sha256::new();
    for (uuid, kind, hash) in &entries {
        h.update(uuid.as_bytes());
        h.update([*kind]);
        h.update(hash);
    }
    h.finalize().into()
}

fn chunk_kind_byte(kind: ports::ChunkKind) -> u8 {
    match kind {
        ports::ChunkKind::Core => 0,
        ports::ChunkKind::Comment => 1,
    }
}

impl From<QueryMode> for QueryModeDto {
    fn from(m: QueryMode) -> Self {
        match m {
            QueryMode::Exact => QueryModeDto::Exact,
            QueryMode::Identifier => QueryModeDto::Identifier,
            QueryMode::Natural => QueryModeDto::Natural,
        }
    }
}
