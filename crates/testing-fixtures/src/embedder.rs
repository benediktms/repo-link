//! Deterministic fake `EmbeddingProvider` for offline tests (RFC 0007 D9:
//! "testing-fixtures supplies a deterministic fake embedder (hash-derived
//! pseudo-vectors) so CI stays offline"). Pseudo-vectors are SHA-256-derived
//! so identical text maps to an identical, reproducible vector; no real
//! model, no network.

use async_trait::async_trait;
use ports::{EmbeddingProvider, PortResult};
use sha2::{Digest, Sha256};

/// Dimensions of the fake vectors (matches the real profile's 384).
pub const FAKE_DIMS: usize = 384;

/// Deterministic fake embedder. `plan_semantic_inputs` returns the chunk
/// text unchanged (a single input), and `embed_inputs` derives each vector
/// from the text hash, so tests stay offline and repeatable.
#[derive(Clone, Debug, Default)]
pub struct HashEmbedder {
    profile_id: String,
}

impl HashEmbedder {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

fn pseudo_vector(text: &str) -> Vec<f32> {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    // Normalize the 32 hash bytes into FAKE_DIMS floats in [-1, 1]; a
    // deterministic, non-trivial vector without any model.
    let mut v = Vec::with_capacity(FAKE_DIMS);
    for i in 0..FAKE_DIMS {
        let b = digest[i % digest.len()] as f32;
        v.push(b / 255.0 * 2.0 - 1.0);
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / norm).collect()
}

#[async_trait]
impl EmbeddingProvider for HashEmbedder {
    fn profile_id(&self) -> String {
        self.profile_id.clone()
    }

    fn dimensions(&self) -> usize {
        FAKE_DIMS
    }

    fn input_limit(&self) -> usize {
        usize::MAX
    }

    fn plan_semantic_inputs(&self, chunk_text: &str) -> PortResult<Vec<String>> {
        Ok(vec![chunk_text.to_string()])
    }

    async fn embed_query(&self, query: &str) -> PortResult<Vec<f32>> {
        Ok(pseudo_vector(query))
    }

    async fn embed_inputs(&self, texts: &[String]) -> PortResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| pseudo_vector(t)).collect())
    }
}
