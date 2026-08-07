use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ports::PortResult;
use ports::EmbeddingProvider;

use crate::model::{self, CandleModel, EmbedConfig};

/// A candle-backed `EmbeddingProvider` for one prepared profile directory.
pub struct CandleEmbeddingProvider {
    profile_id: String,
    model: CandleModel,
}

impl CandleEmbeddingProvider {
    pub fn new(profile_id: &str, cache_dir: &Path, config: EmbedConfig) -> PortResult<Self> {
        let model = model::load(cache_dir, config)?;
        Ok(Self {
            profile_id: profile_id.to_string(),
            model,
        })
    }
}

#[async_trait]
impl EmbeddingProvider for CandleEmbeddingProvider {
    fn profile_id(&self) -> String {
        self.profile_id.clone()
    }

    fn dimensions(&self) -> usize {
        self.model.dimensions()
    }

    fn input_limit(&self) -> usize {
        self.model.max_input_tokens()
    }

    fn plan_semantic_inputs(&self, chunk_text: &str) -> PortResult<Vec<String>> {
        self.model.plan_semantic_inputs(chunk_text)
    }

    async fn embed_query(&self, query: &str) -> PortResult<Vec<f32>> {
        let mut rows = self
            .model
            .embed_batch(&[query.to_string()], true)?;
        Ok(rows.pop().unwrap_or_default())
    }

    async fn embed_inputs(&self, texts: &[String]) -> PortResult<Vec<Vec<f32>>> {
        self.model.embed_batch(texts, false)
    }
}

/// The profile directory is `Arc`d so a provider can be cloned; docs: keep
/// this cheap (`Arc` to a loaded model).
pub type SharedProvider = Arc<CandleEmbeddingProvider>;
