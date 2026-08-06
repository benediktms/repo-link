//! application-search — RFC 0007 task-search orchestration.
//!
//! Owns chunk construction and hashing (D2/D3), the reconcile policy (D6),
//! Unicode fold-to-raw-span mapping and the raw-text literal lane (D4),
//! query-mode classification (D4), complete lane roll-up + post-verification
//! reranking + RRF fusion (D4), excerpt selection, and response assembly.
//! Uses the `ports` source/ index / embedder contracts; keeps pure logic
//! offline-testable.

mod chunker;
mod fold;
mod lane;
mod query_mode;
mod service;

pub use chunker::{CHUNK_FORMAT_VERSION, chunk_task};
pub use query_mode::{QueryMode, classify, identifier_tokens};
pub use service::{SearchError, SearchRequest, TaskSearchService};
