//! TaskSearchService — RFC 0007 Stage 1 orchestration.
//!
//! Every search: (1) derives the lexical-availability decision from sidecar
//! metadata (D8), (2) runs the full-corpus reconcile (D6), (3) opens one
//! authoritative result snapshot (D4), (4) runs the raw-text literal lane and
//! the FTS5 lexical lane, (5) collapses + reranks + RRF-fuses (D4), and
//! (6) assembles immutable response DTOs inside the same snapshot.

use std::collections::HashMap;

use domain_core::TaskId;
use dto_shared::{
    LexicalUnavailableReasonDto, MatchedSourceDto, MatchedSourceKindDto, QueryModeDto,
    SearchMatchDto, SearchResultDto, SemanticSkippedReasonDto, TaskSearchResponseDto,
};

use crate::chunker::chunk_task;
use crate::fold::{excerpt, find_literal_spans};
use crate::lane::{order_fused, rrf_fuse};
use crate::query_mode::{QueryMode, classify, identifier_tokens};
use ports::{
    ChunkTarget, LexicalRank, ReconcileFailure, SearchScope, TaskSearchIndex,
    TaskSearchResultSnapshot, TaskSearchSourceRepository, TaskTextRow,
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

/// The RFC 0007 D6 reconcile + D4 retrieval orchestrator.
pub struct TaskSearchService<S: TaskSearchSourceRepository, I: TaskSearchIndex> {
    source: S,
    index: I,
}

impl<S: TaskSearchSourceRepository, I: TaskSearchIndex> TaskSearchService<S, I> {
    pub fn new(source: S, index: I) -> Self {
        Self { source, index }
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

        // ---- lexical availability + reconcile (D8, D6) -------------------
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
            if let Some(s) = session {
                match s.diff_chunks(&targets, projected).await {
                    Ok(_) => {
                        if s.commit(&fingerprint).await.is_err() {
                            lexical_available = false;
                            lexical_reason =
                                Some(LexicalUnavailableReasonDto::ReconciliationFailed);
                        }
                    }
                    Err(ReconcileFailure { reason }) => {
                        lexical_available = false;
                        lexical_reason = Some(reason);
                    }
                }
            }
        }

        // ---- result snapshot (one authoritative read) --------------------
        let snapshot = self.source.begin_result_snapshot(&req.scope).await?;
        let rows = snapshot.eligible_rows().await?;

        // ---- literal lane (raw-text fold scan, D4) -----------------------
        let literal = literal_lane(&rows, query, mode);

        // ---- lexical lane (FTS5, complete ranking) -----------------------
        let mut lexical: Vec<LexicalRank> = Vec::new();
        if lexical_available {
            let eligible: Vec<TaskId> = rows.iter().map(|r| r.task_id).collect();
            let expr = build_match_expr(query);
            lexical = self.index.search_lexical(&expr, &eligible).await?;
        }

        self.assemble(
            snapshot.as_ref(),
            query,
            mode,
            lexical_available,
            lexical_reason,
            &literal,
            &lexical,
            req.limit,
        )
        .await
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
        literal: &HashMap<TaskId, TaskLiteral>,
        lexical: &[LexicalRank],
        limit: usize,
    ) -> Result<TaskSearchResponseDto, SearchError> {
        let lexical_map: HashMap<TaskId, usize> =
            lexical.iter().map(|r| (r.task_id, r.rank)).collect();
        let lexical_by_task: HashMap<TaskId, &LexicalRank> =
            lexical.iter().map(|r| (r.task_id, r)).collect();

        // Fuse lexical (when present) + the natural-mode literal occurrence
        // lane (RFC 0007 D4 third lane).
        let mut fused = HashMap::new();
        if !lexical_map.is_empty() {
            fused = rrf_fuse(&[&lexical_map]);
        }
        if mode == QueryMode::Natural {
            let occ = literal_occurrence_ranking(literal);
            let fused2 = rrf_fuse(&[&lexical_map, &occ]);
            if !fused2.is_empty() {
                fused = fused2;
            }
        }
        let mut ordered = order_fused(&fused);

        // Mode-specific hard sort-ahead (RFC 0007 D4).
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
                literal: lit.map(|l| l.contributed),
                lexical_rank: lex.map(|r| r.rank as u64),
                semantic_rank: None,
                semantic_score: None,
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
            semantic_available: false,
            semantic_skipped_reason: Some(SemanticSkippedReasonDto::ModelNotPrepared),
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
