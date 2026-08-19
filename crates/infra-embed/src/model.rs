use std::path::Path;

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert as candle_bert;
use candle_transformers::models::xlm_roberta as candle_xlmr;
use ports::PortError;
use tokenizers::tokenizer::{Encoding, Tokenizer};

/// Pooling rule for a sentence-embedding model (RFC 0007 D7 manifest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Mean-pool token embeddings over the attention mask.
    Mean,
    /// Take the `[CLS]` (first) token embedding.
    Cls,
}

/// A candle runtime embedding model for one pinned profile.
pub struct CandleModel {
    model: ModelKind,
    tokenizer: Tokenizer,
    pooling: Pooling,
    /// Instruction prefixes from the profile manifest (D7). Corpus
    /// (task/chunk) inputs and query inputs are embedded asymmetrically.
    corpus_prefix: Option<String>,
    query_prefix: Option<String>,
    dims: usize,
    max_input_tokens: usize,
    device: Device,
}

enum ModelKind {
    Bert(candle_bert::BertModel),
    XlmRoberta(candle_xlmr::XLMRobertaModel),
}

pub struct EmbedConfig {
    pub pooling: Pooling,
    pub corpus_prefix: Option<String>,
    pub query_prefix: Option<String>,
    pub dims: usize,
    pub max_input_tokens: usize,
}

fn model_kind_from_config(json: &serde_json::Value) -> Result<String, PortError> {
    json.get("model_type")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| PortError::Backend("config.json missing model_type".into()))
}

fn load_bert(
    vb: VarBuilder,
    config_json: serde_json::Value,
) -> Result<candle_bert::BertModel, PortError> {
    let config: candle_bert::Config = serde_json::from_value(config_json)
        .map_err(|e| PortError::Backend(format!("bert config: {e}")))?;
    candle_bert::BertModel::load(vb, &config)
        .map_err(|e| PortError::Backend(format!("bert load: {e}")))
}

fn load_xlmr(
    vb: VarBuilder,
    config_json: serde_json::Value,
) -> Result<candle_xlmr::XLMRobertaModel, PortError> {
    let config: candle_xlmr::Config = serde_json::from_value(config_json)
        .map_err(|e| PortError::Backend(format!("xlmr config: {e}")))?;
    candle_xlmr::XLMRobertaModel::new(&config, vb)
        .map_err(|e| PortError::Backend(format!("xlmr load: {e}")))
}

/// Candidate split points at sentence boundaries (after `.`, `!`, `?`), as
/// `(start, end)` byte offsets into `text`, excluding trailing whitespace.
fn sentence_boundaries(text: &str) -> Vec<usize> {
    let mut out = vec![text.len()];
    let mut last = 0usize;
    for (idx, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            last = idx + ch.len_utf8();
            out.push(last);
        }
    }
    let _ = last;
    out.sort_unstable();
    out.dedup();
    out
}

/// Candidate split points at word boundaries (whitespace), as byte offsets.
fn word_boundaries(text: &str) -> Vec<usize> {
    let mut out = vec![text.len()];
    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            out.push(idx);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// A byte offset into `text` at or before `budget` bytes, aligned to a UTF-8
/// scalar boundary. Never returns a non-boundary offset: when even one
/// character exceeds the budget, it returns that character's full width.
fn byte_boundary(text: &str, budget: usize) -> usize {
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 && !text.is_empty() {
        text.chars().next().map(|c| c.len_utf8()).unwrap_or(1)
    } else {
        end
    }
}

/// Largest ascending boundary whose prefix of `text` tokenizes within
/// `budget` (binary search: token_count is non-decreasing in the boundary).
fn largest_fit(
    model: &CandleModel,
    text: &str,
    boundaries: &[usize],
    budget: usize,
) -> Result<usize, PortError> {
    let mut lo = 0usize;
    let mut hi = boundaries.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let b = boundaries[mid];
        if model.token_count(&text[..b])? <= budget {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(if lo == 0 { 0 } else { boundaries[lo - 1] })
}

/// Pick the fastest available inference device (RFC 0007 D7): CUDA when the
/// `cuda` feature is enabled and a GPU is present, Metal on macOS, else CPU.
/// Every candidate falls back gracefully when its backend is absent, including
/// when the backend reports absence by panicking (see [`probe_device`]).
fn pick_device() -> Device {
    #[cfg(feature = "cuda")]
    {
        if let Some(d) = probe_device(|| candle_core::Device::cuda_if_available(0)) {
            return d;
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(d) = probe_device(|| candle_core::Device::metal_if_available(0)) {
            return d;
        }
    }
    Device::Cpu
}

/// Guards the process-global panic hook while a probe runs, so two concurrent
/// probes cannot leave the silencing hook installed for the rest of the process.
static PROBE_HOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run one accelerator probe, treating an error *and* a panic inside the
/// backend as "this device is not available". candle's Metal probe panics
/// (`swap_remove index (is 0) should be < len (is 0)`) instead of returning
/// `Err` when the system exposes no Metal device, which crashed every command
/// that loads a model.
///
/// The probe silences the panic hook so a fallback prints no backtrace over the
/// command's own output. That hook is process-global, so a probe holds
/// [`PROBE_HOOK`] across the swap and takes the lock back after a poisoning
/// panic: an unrestored hook would swallow every later panic message.
#[cfg_attr(
    not(any(feature = "cuda", target_os = "macos")),
    allow(dead_code, reason = "no accelerator probe is compiled for this target")
)]
fn probe_device(
    probe: impl FnOnce() -> candle_core::Result<Device> + std::panic::UnwindSafe,
) -> Option<Device> {
    let _guard = PROBE_HOOK.lock().unwrap_or_else(|e| e.into_inner());
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(probe);
    std::panic::set_hook(hook);
    match outcome {
        Ok(Ok(device)) => Some(device),
        Ok(Err(e)) => {
            tracing::debug!("accelerator probe unavailable: {e}");
            None
        }
        Err(_) => {
            tracing::debug!("accelerator probe panicked; treating the device as unavailable");
            None
        }
    }
}

/// Load a prepared model from a cache directory (RFC 0007 D7): expects
/// `model.safetensors`, `config.json`, `tokenizer.json` inside `dir`.
pub fn load(dir: &Path, config: EmbedConfig) -> Result<CandleModel, PortError> {
    let device = pick_device();
    let tokenizer_path = dir.join("tokenizer.json");
    let tokenizer = Tokenizer::from_file(&tokenizer_path)
        .map_err(|e| PortError::Backend(format!("tokenizer load: {e}")))?;

    let config_json: serde_json::Value = serde_json::from_reader(
        std::fs::File::open(dir.join("config.json"))
            .map_err(|e| PortError::Backend(format!("config open: {e}")))?,
    )
    .map_err(|e| PortError::Backend(format!("config parse: {e}")))?;

    let hidden = config_json
        .get("hidden_size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| PortError::Backend("config.json missing hidden_size".to_string()))?;
    if hidden as usize != config.dims {
        return Err(PortError::Backend(format!(
            "config.json hidden_size {hidden} != profile dims {}",
            config.dims
        )));
    }

    let safetensors_path = dir.join("model.safetensors");
    // Safety: mmap of a digest-verified artifact from an owner-only cache;
    // the mapping is read-only and the file is never written after verify.
    let vb =
        unsafe { VarBuilder::from_mmaped_safetensors(&[safetensors_path], DType::F32, &device) }
            .map_err(|e| PortError::Backend(format!("safetensors mmap: {e}")))?;

    let model = match model_kind_from_config(&config_json)?.as_str() {
        "bert" => ModelKind::Bert(load_bert(vb, config_json)?),
        "xlm-roberta" => ModelKind::XlmRoberta(load_xlmr(vb, config_json)?),
        other => {
            return Err(PortError::Backend(format!(
                "unsupported model_type: {other}"
            )));
        }
    };

    Ok(CandleModel {
        model,
        tokenizer,
        pooling: config.pooling,
        corpus_prefix: config.corpus_prefix,
        query_prefix: config.query_prefix,
        dims: config.dims,
        max_input_tokens: config.max_input_tokens,
        device,
    })
}

impl CandleModel {
    pub fn dimensions(&self) -> usize {
        self.dims
    }

    pub fn max_input_tokens(&self) -> usize {
        self.max_input_tokens
    }

    fn prefix(&self, query: bool) -> Option<&str> {
        if query {
            self.query_prefix.as_deref()
        } else {
            self.corpus_prefix.as_deref()
        }
    }

    fn encode(&self, text: &str, query: bool) -> Result<Encoding, PortError> {
        let full = match self.prefix(query) {
            Some(p) => format!("{p}{text}"),
            None => text.to_string(),
        };
        self.tokenizer
            .encode(full, true)
            .map_err(|e| PortError::Backend(format!("encode: {e}")))
    }

    /// Token count of `text` including the corpus instruction prefix and
    /// model special tokens (RFC 0007 D2 "effective token budget").
    fn token_count(&self, text: &str) -> Result<usize, PortError> {
        let full = match self.corpus_prefix.as_deref() {
            Some(p) => format!("{p}{text}"),
            None => text.to_string(),
        };
        let enc = self
            .tokenizer
            .encode(full, true)
            .map_err(|e| PortError::Backend(format!("token count: {e}")))?;
        Ok(enc.get_ids().len())
    }

    /// Split one lexical chunk into complete, tokenizer-bounded semantic
    /// inputs (RFC 0007 D2/D7): every byte of `chunk_text` is covered, and
    /// each input fits under `max_input_tokens` including the corpus prefix
    /// and special tokens. Never relies on runtime truncation.
    pub fn plan_semantic_inputs(&self, chunk_text: &str) -> Result<Vec<String>, PortError> {
        let budget = self.max_input_tokens.saturating_sub(1);
        if chunk_text.is_empty() {
            return Ok(Vec::new());
        }
        if self.token_count(chunk_text)? <= budget {
            return Ok(vec![chunk_text.to_string()]);
        }
        let mut inputs: Vec<String> = Vec::new();
        let mut remaining = chunk_text;
        while !remaining.is_empty() {
            // Boundaries are ascending and token_count is non-decreasing in
            // the boundary, so binary-search the largest fit instead of
            // tokenizing every candidate (O(log N) calls per segment).
            let mut best_len =
                largest_fit(self, remaining, &sentence_boundaries(remaining), budget)?;
            if best_len == 0 {
                best_len = largest_fit(self, remaining, &word_boundaries(remaining), budget)?;
            }
            if best_len == 0 {
                let mut end = byte_boundary(remaining, budget);
                while end > 0 && self.token_count(&remaining[..end])? > budget {
                    end = byte_boundary(&remaining[..end], end - 1);
                }
                if end == 0 {
                    return Err(PortError::Backend(format!(
                        "single token exceeds max_input_tokens={}: {:?}",
                        budget,
                        &remaining[..remaining.len().min(16)]
                    )));
                }
                best_len = end;
            }
            inputs.push(remaining[..best_len].to_string());
            remaining = &remaining[best_len..];
        }
        Ok(inputs)
    }

    /// Embed `texts` (all corpus or all query, decided by `query`) into
    /// L2-normalized vectors of `dims` rows.
    pub fn embed_batch(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>, PortError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings: Vec<Encoding> = texts
            .iter()
            .map(|t| self.encode(t, query))
            .collect::<Result<_, _>>()?;

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(1)
            .min(self.max_input_tokens);

        let n_batch = encodings.len();
        let mut ids: Vec<u32> = Vec::with_capacity(n_batch * max_len);
        let mut masks: Vec<u32> = Vec::with_capacity(n_batch * max_len);
        for enc in &encodings {
            let mut row_ids = enc.get_ids().to_vec();
            let mut row_mask = enc.get_attention_mask().to_vec();
            if row_ids.len() > max_len {
                row_ids.truncate(max_len);
                row_mask.truncate(max_len);
            }
            row_ids.resize(max_len, 0);
            row_mask.resize(max_len, 0);
            ids.extend_from_slice(&row_ids);
            masks.extend_from_slice(&row_mask);
        }

        let rows = self
            .forward_batch(&ids, &masks, n_batch, max_len)
            .map_err(|e| PortError::Backend(format!("candle: {e}")))?;
        Ok(rows)
    }

    /// Pure-candle batch forward + pooling + L2 normalization.
    fn forward_batch(
        &self,
        ids: &[u32],
        masks: &[u32],
        n_batch: usize,
        max_len: usize,
    ) -> candle_core::Result<Vec<Vec<f32>>> {
        let input_ids = Tensor::new(ids, &self.device)?.reshape((n_batch, max_len))?;
        let attention_mask = Tensor::new(masks, &self.device)?.reshape((n_batch, max_len))?;
        let token_type_ids = input_ids.zeros_like()?;

        let logits: Tensor = match &self.model {
            ModelKind::Bert(m) => m.forward(&input_ids, &token_type_ids, Some(&attention_mask))?,
            ModelKind::XlmRoberta(m) => m.forward(
                &input_ids,
                &attention_mask,
                &token_type_ids,
                None,
                None,
                None,
            )?,
        };

        let (_b, _s, h) = logits.dims3()?;
        if h != self.dims {
            candle_core::bail!("model hidden size {h} != profile dims {}", self.dims);
        }
        let f32_logits = logits.to_dtype(DType::F32)?;
        let mask_t = Tensor::new(masks, &self.device)?
            .reshape((n_batch, max_len, 1))?
            .to_dtype(DType::F32)?;

        let pooled = match self.pooling {
            Pooling::Mean => {
                let masked = f32_logits.broadcast_mul(&mask_t)?;
                let denom = mask_t.sum_keepdim(1)?.affine(1.0, 1e-9)?;
                masked.sum_keepdim(1)?.broadcast_div(&denom)?
            }
            Pooling::Cls => f32_logits.i((.., 0, ..))?.unsqueeze(1)?,
        };

        let pooled = pooled.squeeze(1)?;
        let norms = pooled.sqr()?.sum_keepdim(1)?.sqrt()?.affine(1.0, 1e-9)?;
        let normalized = pooled.broadcast_div(&norms)?;

        normalized.to_vec2::<f32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a backend that panics while enumerating devices (candle's
    /// Metal path when the system reports none) must degrade to the next
    /// candidate, not abort the command.
    #[test]
    fn probe_device_treats_a_panicking_backend_as_absent() {
        assert!(
            probe_device(|| panic!("swap_remove index (is 0) should be < len (is 0)")).is_none()
        );
    }

    #[test]
    fn probe_device_treats_an_erroring_backend_as_absent() {
        assert!(probe_device(|| candle_core::bail!("no device")).is_none());
    }

    #[test]
    fn pick_device_returns_a_device_on_every_target() {
        let _ = pick_device();
    }
}
