//! ports — RFC 0007 task-search contracts (application ↔ infrastructure).
//!
//! Three capabilities (RFC 0007 D9): the authoritative task/comment source
//! (`TaskSearchSourceRepository`), the disposable sidecar index
//! (`TaskSearchIndex`), and — in later stages — the embedding provider
//! (`EmbeddingProvider`). Only the first two are exercised by PR1.
//!
//! Cross-boundary orchestration types that tie a response together
//! (`TaskSearchResultSnapshot`) live here, not in `application-search`,
//! because the snapshot is created by the adapter and consumed by the
//! application layer.

use async_trait::async_trait;
use domain_core::{RepoId, TaskId, WorkspaceId};
use dto_shared::{LexicalUnavailableReasonDto, MatchedSourceKindDto};

use crate::error::PortResult;

/// Sidecar derived-schema version (RFC 0007 D5). Bumped when the D5 schema
/// (tables, columns, triggers) changes. v2: FTS5 declared as external-content
/// over `task_search_chunks` so `delete-all`/`integrity-check` and the
/// trigger pattern are legal.
pub const SEARCH_SCHEMA_VERSION: i64 = 2;
/// Lexical chunk-format version (RFC 0007 D2). Bumped only when chunk
/// formatting/boundary rules change; independent of model/profile. Persisted
/// in `task_search_meta.chunk_format_version`.
pub const SEARCH_CHUNK_FORMAT_VERSION: i64 = 1;

// ---------- Shared value types -------------------------------------------

/// One comment's current text (RFC 0007 D1/D3). Author/timestamp are
/// metadata, not search content, and are deliberately absent.
#[derive(Clone, Debug)]
pub struct CommentTextRow {
    /// GitHub comment id; `None` for a pending local comment.
    pub remote_comment_id: Option<String>,
    pub body: String,
}

/// One task's current search content from the authoritative database, in one
/// read snapshot (RFC 0007 D6 step 3). Never reads snapshots or audit data.
#[derive(Clone, Debug)]
pub struct TaskTextRow {
    pub task_id: TaskId,
    pub workspace_id: WorkspaceId,
    pub repo_id: Option<RepoId>,
    /// Lifecycle open bit; used for query-time eligibility filters.
    pub is_open: bool,
    pub title: String,
    pub body: String,
    /// Current comments (synced + pending), oldest first.
    pub comments: Vec<CommentTextRow>,
}

/// Query-time eligibility scope (RFC 0007 D4/D10). An omitted field means no
/// filter on that axis; `--status` defaults to all.
#[derive(Clone, Debug, Default)]
pub struct SearchScope {
    pub workspace_id: Option<WorkspaceId>,
    pub repo_id: Option<RepoId>,
    pub is_open: Option<bool>,
}

/// Which source kind a search chunk came from (RFC 0007 D3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkKind {
    Core,
    Comment,
}

/// A desired index row computed by `application-search` (RFC 0007 D6
/// step 4–5): the formatted lexical chunk plus its SHA-256 content hash,
/// keyed by `(task_id, content_hash)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkTarget {
    pub task_id: TaskId,
    pub kind: ChunkKind,
    /// SHA-256 of the formatted UTF-8 text, 32 bytes.
    pub content_hash: [u8; 32],
    /// Formatted lexical chunk text (`Title:`-anchored, RFC 0007 D2/D3).
    pub text: String,
}

/// Result of reconciling the desired set against the sidecar (RFC 0007 D6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileDiff {
    pub inserted: usize,
    pub deleted: usize,
    pub desired_total: usize,
}

/// A reconcile that could not write a consistent index. Carries the stable
/// D8 degradation reason (`storage_limit`, `index_corrupt`, or
/// `reconciliation_failed`) so the service degrades to literal-only without
/// guessing from an opaque backend error.
#[derive(Debug, thiserror::Error)]
#[error("task-search reconcile failed: {reason:?}")]
pub struct ReconcileFailure {
    pub reason: LexicalUnavailableReasonDto,
}

/// A single raw substring hit on authoritative task text (literal lane).
#[derive(Clone, Debug)]
pub struct LiteralHit {
    /// The task that matched.
    pub task_id: TaskId,
    /// Which column produced the match (title/body → Core, comment → Comment).
    pub kind: MatchedSourceKindDto,
    /// The `remote_comment_id` of the matched comment, when kind is Comment.
    pub remote_comment_id: Option<String>,
    /// Raw UTF-8 byte span in the source column that encloses the match.
    pub span: std::ops::Range<usize>,
}

// ---------- Authoritative source repository ------------------------------

/// Current task/comment search content from `repo-link.db`.
#[async_trait]
pub trait TaskSearchSourceRepository: Send + Sync {
    /// Load every current task + comment text in one read snapshot for
    /// reconciliation (RFC 0007 D6 step 3).
    async fn load_reconcile_snapshot(&self) -> PortResult<Vec<TaskTextRow>>;

    /// Open a result snapshot over the eligible tasks for `scope`. The
    /// snapshot is one consistent read: eligibility, raw text, literal
    /// matches, and every DTO field come from the same authoritative view
    /// (RFC 0007 D4 "one authoritative read snapshot").
    async fn begin_result_snapshot(
        &self,
        scope: &SearchScope,
    ) -> PortResult<Box<dyn TaskSearchResultSnapshot>>;
}

/// A consistent authoritative read for result assembly (RFC 0007 D4).
/// Lives until the response DTOs are fully built, then is dropped; no
/// further database reads happen after serialization begins.
#[async_trait]
pub trait TaskSearchResultSnapshot: Send + Sync {
    /// All eligible task rows as raw text, for the literal lane's
    /// Unicode-folded substring scan (RFC 0007 D4 raw-text scan).
    async fn eligible_rows(&self) -> PortResult<Vec<TaskTextRow>>;

    /// Confirm which of `task_ids` are still present and eligible in this
    /// snapshot — used to drop stale sidecar candidates before reranking
    /// (RFC 0007 D4 "current-source verification").
    async fn verify_sources(&self, task_ids: &[TaskId]) -> PortResult<Vec<TaskId>>;

    /// Resolve a task's display identity within this snapshot.
    async fn task_identity(&self, task_id: &TaskId) -> PortResult<TaskIdentity>;
}

/// Identity fields needed to render a result row (RFC 0007 D10).
#[derive(Clone, Debug)]
pub struct TaskIdentity {
    /// Composite display id (`prefix-hash`), e.g. `"rpl-abc"`.
    pub display_id: String,
    pub workspace_id: WorkspaceId,
    pub workspace_name: String,
    pub title: String,
}

// ---------- Disposable sidecar index -------------------------------------

/// Write/read access to the task-search sidecar
/// (`<authoritative>.task-search.db`, RFC 0007 D5/D6).
#[async_trait]
pub trait TaskSearchIndex: Send + Sync {
    /// Begin a serialized reconcile session (sidecar `BEGIN IMMEDIATE`).
    async fn begin_reconcile(&self) -> PortResult<Box<dyn ReconcileSession>>;

    /// Read index metadata (schema/format/profile identity + storage facts).
    async fn metadata(&self) -> PortResult<IndexMetadata>;

    /// Complete lexical task ranking for `match_expr` over `eligible`
    /// task ids. Returns one best rank per task, ascending (1 = best).
    async fn search_lexical(
        &self,
        match_expr: &str,
        eligible: &[TaskId],
    ) -> PortResult<Vec<LexicalRank>>;

    /// Structural status (chunk/text/vector/FTS facts) for `search-index`
    /// and `status`.
    async fn stats(&self) -> PortResult<IndexStats>;

    /// Explicit `clear`: delete all derived rows + FTS delete-all +
    /// auto-vacuum + truncating checkpoint under the exclusive lifecycle
    /// lock.
    async fn clear(&self) -> PortResult<()>;

    /// Explicit `rebuild`: replace derived schema + rows for the current
    /// chunk format under the exclusive lifecycle lock.
    async fn rebuild(&self, targets: &[ChunkTarget]) -> PortResult<u64>;

    /// Compare-and-set the sidecar's embedding profile: succeeds only when
    /// `embedding_profile_id` is currently NULL; claims it as `expected`
    /// (RFC 0007 D8 "Unclaimed profile"). Returns Ok(true) on claim, Ok(false)
    /// when another profile already owns the sidecar.
    async fn claim_empty_profile(&self, expected: &str) -> PortResult<bool>;

    /// List up to `limit` lexical chunks with no stored vectors, oldest
    /// first — the input set for guarded semantic embedding (RFC 0007 D6).
    ///
    /// Discovery is chunk-granular: a chunk with any stored vector is
    /// complete, which holds because [`TaskSearchIndex::store_vectors_guarded`]
    /// refuses to leave partial coverage behind.
    async fn missing_semantic_inputs(&self, limit: u32) -> PortResult<Vec<MissingSemanticInput>>;

    /// Insert one guarded vector batch (RFC 0007 D6 guards: profile, chunk
    /// identity, and input hash must all still match). Rows whose guards
    /// fail are discarded.
    ///
    /// Returns how many of `rows` have their vector in place afterwards,
    /// counting rows a concurrent writer already stored — only a guard
    /// rejection is no progress, so a caller that stops at zero does not
    /// mistake a completed race for a failure.
    ///
    /// A batch that would leave a chunk covering only part of `0..n` fails
    /// and stores nothing: callers must supply every segment of a chunk whose
    /// vectors they write.
    async fn store_vectors_guarded(&self, rows: &[GuardedVectorRow]) -> PortResult<usize>;

    /// Complete semantic ranking of `eligible` tasks against `query_vector`
    /// by exact cosine (best per-task vector), descending score (RFC 0007 D4).
    async fn search_semantic(
        &self,
        query_vector: &[f32],
        eligible: &[TaskId],
    ) -> PortResult<Vec<SemanticRank>>;
}

/// A serialized sidecar reconcile session (RFC 0007 D6 ordering: the sidecar
/// writer transaction is acquired before the authoritative snapshot, so a
/// slow reconciler cannot regress a newer index).
#[async_trait]
pub trait ReconcileSession: Send {
    /// Diff the desired set against the sidecar and write the delta.
    /// `projected_bytes` is the RFC 0007 D5 storage preflight (adapter
    /// refuses before growth). Returns counts, or a `ReconcileFailure`
    /// carrying the stable degradation reason.
    async fn diff_chunks(
        &mut self,
        desired: &[ChunkTarget],
        projected_bytes: u64,
    ) -> Result<ReconcileDiff, ReconcileFailure>;

    /// Commit the reconcile and persist a validated-integrity marker bound
    /// to the written content fingerprint (RFC 0007 D6).
    async fn commit(&mut self, content_fingerprint: &[u8; 32]) -> PortResult<()>;

    /// Roll back without persisting the marker.
    async fn rollback(&mut self) -> PortResult<()>;
}

/// A complete lexical ranking entry: one row per task, with the matched
/// chunk's source evidence (RFC 0007 D4/D10) so a lexical-only winner still
/// gets an explanatory excerpt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexicalRank {
    pub task_id: TaskId,
    /// 1-based best rank for this task.
    pub rank: usize,
    /// Source kind of the winning chunk.
    pub kind: MatchedSourceKindDto,
    /// `Some` when the winning chunk is a comment whose current row is
    /// unambiguously identified.
    pub remote_comment_id: Option<String>,
    /// Bounded excerpt of the winning chunk's formatted text.
    pub excerpt: String,
}

/// Sidecar metadata used for degradation decisions (RFC 0007 D8).
#[derive(Clone, Debug)]
pub struct IndexMetadata {
    /// None means the sidecar is absent/unopenable.
    pub available: Option<SidecarInfo>,
    /// Present when the sidecar exists but the schema/chunk format is
    /// incompatible with this binary.
    pub schema_mismatch: Option<SchemaMismatch>,
    /// The sidecar opened for reading only, because the environment denies a
    /// write to it. Every maintenance operation refuses with
    /// `PermissionDenied`, and a read serves the last reconciled content.
    pub read_only: bool,
}

#[derive(Clone, Debug)]
pub struct SidecarInfo {
    pub schema_version: i64,
    pub chunk_format_version: i64,
    pub embedding_profile_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchemaMismatch {
    pub incompatible: bool,
}

/// Structural stats for `search-index status` (RFC 0007 D10).
#[derive(Clone, Debug, Default)]
pub struct IndexStats {
    pub chunk_count: u64,
    pub vector_count: u64,
    /// None means the check did not run. The FTS5 `integrity-check` command is
    /// an `INSERT` statement, so a read-only sidecar cannot run it.
    pub fts_integrity_ok: Option<bool>,
    pub sidecar_size_bytes: u64,
    pub sidecar_available: bool,
}

// ---------- Embedding provider (Stage 2/3; declared for PR1) -------------

/// Deterministic local embedding (RFC 0007 D7). PR1 never constructs one; a
/// `testing-fixtures` fake exists so `application-search` unit tests stay
/// offline and deterministic.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn profile_id(&self) -> String;
    fn dimensions(&self) -> usize;

    /// Effective token budget for a single semantic input, including
    /// instruction prefixes and model special tokens (RFC 0007 D2).
    fn input_limit(&self) -> usize;

    /// Split one lexical chunk into complete, tokenizer-bounded semantic
    /// inputs: every byte of the chunk is covered by at least one input, and
    /// each input fits under [`EmbeddingProvider::input_limit`] including
    /// the profile's instruction prefix (RFC 0007 D2/D7). The adapter never
    /// relies on runtime truncation.
    fn plan_semantic_inputs(&self, chunk_text: &str) -> PortResult<Vec<String>>;

    async fn embed_query(&self, query: &str) -> PortResult<Vec<f32>>;
    async fn embed_inputs(&self, texts: &[String]) -> PortResult<Vec<Vec<f32>>>;
}

/// One semantic input waiting for a vector: a lexical chunk (D2/D3 text)
/// whose tokenizer-bounded inputs are not yet stored (RFC 0007 D6 "missing
/// semantic inputs are embedded in batches").
#[derive(Clone, Debug)]
pub struct MissingSemanticInput {
    /// Sidecar row id of the lexical chunk.
    pub search_chunk_id: i64,
    pub task_id: TaskId,
    pub kind: ChunkKind,
    /// SHA-256 of the formatted chunk text (guard identity, RFC 0007 D6).
    pub content_hash: [u8; 32],
    /// Formatted lexical chunk text (title-anchored).
    pub text: String,
}

/// A guarded vector batch row (RFC 0007 D6 guards): the insert succeeds only
/// while sidecar metadata still names the same profile, the chunk identity
/// (id/task/hash) is unchanged, and the embedding-input hash still matches
/// the tokenizer-derived input.
#[derive(Clone, Debug)]
pub struct GuardedVectorRow {
    pub search_chunk_id: i64,
    pub task_id: TaskId,
    pub content_hash: [u8; 32],
    pub segment_index: u32,
    /// SHA-256 of the semantic input text.
    pub embedding_input_hash: [u8; 32],
    /// Normalized f32 vector of `dimensions()` floats.
    pub vector: Vec<f32>,
}

/// A complete semantic ranking entry: best cosine per eligible task
/// (RFC 0007 D4 exact scan, memory O(eligible), not O(vectors)).
#[derive(Clone, Debug)]
pub struct SemanticRank {
    pub task_id: TaskId,
    /// Cosine similarity, higher is better.
    pub score: f32,
}
