use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ports::EmbeddingProvider;
use ports::PortResult;

use crate::model::{self, CandleModel, EmbedConfig};

/// A candle-backed `EmbeddingProvider` for one prepared profile directory.
pub struct CandleEmbeddingProvider {
    profile_id: String,
    model: Arc<CandleModel>,
}

impl CandleEmbeddingProvider {
    pub fn new(profile_id: &str, cache_dir: &Path, config: EmbedConfig) -> PortResult<Self> {
        let model = Arc::new(model::load(cache_dir, config)?);
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
        let model = Arc::clone(&self.model);
        let query = query.to_string();
        tokio::task::spawn_blocking(move || {
            let mut rows = model.embed_batch(&[query], true)?;
            Ok(rows.pop().unwrap_or_default())
        })
        .await
        .map_err(|e| ports::PortError::Backend(format!("embed join: {e}")))?
    }

    async fn embed_inputs(&self, texts: &[String]) -> PortResult<Vec<Vec<f32>>> {
        let model = Arc::clone(&self.model);
        let texts = texts.to_vec();
        tokio::task::spawn_blocking(move || model.embed_batch(&texts, false))
            .await
            .map_err(|e| ports::PortError::Backend(format!("embed join: {e}")))?
    }
}

/// The profile directory is `Arc`d so a provider can be cloned; docs: keep
/// this cheap (`Arc` to a loaded model).
pub type SharedProvider = Arc<CandleEmbeddingProvider>;
