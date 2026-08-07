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

/// Deterministic fake embedder. `plan_semantic_inputs` splits the chunk text
/// at [`HashEmbedder::with_input_limit`] characters (one input by default),
/// and `embed_inputs` derives each vector from the text hash, so tests stay
/// offline and repeatable.
#[derive(Clone, Debug)]
pub struct HashEmbedder {
    profile_id: String,
    input_limit: usize,
}

impl HashEmbedder {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            input_limit: usize::MAX,
        }
    }

    /// Cap a semantic input at `limit` characters so tests can drive the
    /// multi-segment fill path and the `QueryTooLong` branch.
    pub fn with_input_limit(mut self, limit: usize) -> Self {
        self.input_limit = limit.max(1);
        self
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(String::new())
    }
}

fn pseudo_vector(text: &str) -> Vec<f32> {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
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
        self.input_limit
    }

    fn plan_semantic_inputs(&self, chunk_text: &str) -> PortResult<Vec<String>> {
        let chars: Vec<char> = chunk_text.chars().collect();
        if chars.len() <= self.input_limit {
            return Ok(vec![chunk_text.to_string()]);
        }
        Ok(chars
            .chunks(self.input_limit)
            .map(|c| c.iter().collect())
            .collect())
    }

    async fn embed_query(&self, query: &str) -> PortResult<Vec<f32>> {
        Ok(pseudo_vector(query))
    }

    async fn embed_inputs(&self, texts: &[String]) -> PortResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| pseudo_vector(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plans_one_input() {
        let e = HashEmbedder::new("p");
        assert_eq!(e.input_limit(), usize::MAX);
        assert_eq!(e.plan_semantic_inputs("abcdef").unwrap(), vec!["abcdef"]);
    }

    #[test]
    fn limit_splits_into_complete_segments() {
        let e = HashEmbedder::new("p").with_input_limit(4);
        let inputs = e.plan_semantic_inputs("abcdefghij").unwrap();
        assert_eq!(inputs, vec!["abcd", "efgh", "ij"]);
        assert_eq!(inputs.concat(), "abcdefghij", "every byte must be covered");
    }

    #[test]
    fn limit_respects_char_boundaries() {
        let e = HashEmbedder::new("p").with_input_limit(2);
        assert_eq!(e.plan_semantic_inputs("äöü€").unwrap(), vec!["äö", "ü€"]);
    }
}
