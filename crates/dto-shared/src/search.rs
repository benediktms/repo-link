//! dto-shared — RFC 0007 task-search + search-index DTO contract.
//!
//! The JSON surface for `rl task search` and `rl task search-index`
//! (RFC 0007 D10). Availability and reason fields follow a strict machine
//! contract: a lane-availability field is present exactly when its lane
//! succeeded, and the paired reason enum is present exactly when it did not.
//! Lane scores are ranking signals, not calibrated probabilities.

use serde::{Deserialize, Serialize};

// ---------- Query mode ---------------------------------------------------

/// The deterministic token classifier's verdict (RFC 0007 D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryModeDto {
    Exact,
    Identifier,
    Natural,
}

// ---------- Degradation reasons ------------------------------------------

/// Why the lexical (FTS5) lane is unavailable. Present on the wire exactly
/// when `lexical_available` is false (RFC 0007 D8/D10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalUnavailableReasonDto {
    SidecarUnavailable,
    PermissionDenied,
    SchemaMismatch,
    IndexCorrupt,
    StorageLimit,
    ReconciliationFailed,
}

/// Why the semantic lane is unavailable. Present on the wire exactly when
/// `semantic_available` is false (RFC 0007 D8/D10). When several
/// prerequisites fail, the emitted reason is the first in this declaration
/// order (RFC 0007 D10 "first in this order").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticSkippedReasonDto {
    LexicalIndexUnavailable,
    /// `--exact` selected exact mode, which is a literal-match guarantee and
    /// never consults the semantic lane or loads a model (RFC 0007 D4).
    ExactMode,
    ModelNotPrepared,
    ProfileMismatch,
    ModelCacheMissing,
    QueryTooLong,
    StorageLimit,
    EmbeddingFailed,
}

// ---------- Result rows --------------------------------------------------

/// Which source kind produced the winning match evidence (RFC 0007 D3/D10).
/// `Core` = the task's title/body chunk; `Comment` = a comment chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedSourceKindDto {
    Core,
    Comment,
}

/// Per-lane contribution to one result. Optional fields are omitted (not
/// null) when that lane did not contribute (RFC 0007 D10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchMatchDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_rank: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_rank: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fused_score: Option<f32>,
}

/// Where the winning match was found, with a bounded excerpt windowed around
/// the raw match span (RFC 0007 D4/D10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchedSourceDto {
    pub kind: MatchedSourceKindDto,
    /// `remote_comment_id` present only when the winning chunk is a comment
    /// whose current row is unambiguously identified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_comment_id: Option<String>,
    pub excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResultDto {
    pub rank: u64,
    /// Composite display id (`prefix-hash`), e.g. `"rpl-abc"`.
    pub id: String,
    /// Canonical task uuid, serialized as a string.
    pub task_id: String,
    pub workspace_id: String,
    pub workspace_name: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<SearchMatchDto>,
    pub matched_source: MatchedSourceDto,
}

// ---------- Top-level search response ------------------------------------

/// Subtopic — the full `rl task search` JSON response (RFC 0007 D10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskSearchResponseDto {
    pub query: String,
    pub query_mode: QueryModeDto,
    pub lexical_available: bool,
    /// Required iff `lexical_available` is false; omitted when it is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_unavailable_reason: Option<LexicalUnavailableReasonDto>,
    pub semantic_available: bool,
    /// Required iff `semantic_available` is false; omitted when it is true
    /// — even when the successful lane returned no candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_skipped_reason: Option<SemanticSkippedReasonDto>,
    pub results: Vec<SearchResultDto>,
}

// ---------- search-index subcommand responses ----------------------------

/// Summary of one search-index maintenance command (RFC 0007 D10).
///
/// Flat result structs below serialize their meaningful-in-this-context
/// fields; zero-valued fields are omitted to keep the JSON tight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchIndexStatusDto {
    pub lexical_available: bool,
    pub semantic_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_unavailable_reason: Option<LexicalUnavailableReasonDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_skipped_reason: Option<SemanticSkippedReasonDto>,
    pub chunk_count: u64,
    pub vector_count: u64,
    /// Whether the sidecar FTS5 index passed `integrity-check`. `null` means
    /// the check did not run: the command writes, so a read-only sidecar
    /// cannot answer it.
    pub fts_integrity_ok: Option<bool>,
    /// Sidecar database file size in bytes (main DB after checkpoint).
    pub sidecar_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_available: Option<bool>,
}

/// Structured response for an explicit `search-index rebuild` or `clear`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchIndexMaintenanceDto {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks_written: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Structured response for `search-index prepare-model` (Stage 2/3 surface;
/// PR1 reports the not-yet-prepared state).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchModelStatusDto {
    pub prepared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
}
