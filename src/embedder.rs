use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
// Tokenizer import is shared by both backends (both structs carry a Tokenizer field).
// The no-feature build is caught by compile_error in lib.rs before this is relevant.
#[cfg(any(feature = "wgpu", feature = "ort-cpu"))]
use tokenizers::Tokenizer;

// Tokenizer configuration types used only in the wgpu (burn) inference path.
#[cfg(feature = "wgpu")]
use tokenizers::{PaddingParams, PaddingStrategy, TruncationParams, TruncationStrategy};

#[cfg(feature = "wgpu")]
use std::sync::Mutex;

#[cfg(feature = "wgpu")]
use burn::prelude::Module;

#[cfg(feature = "wgpu")]
use crate::nomic_bert_burn::{
    BurnNomicBertModel, NomicBertConfig, burn_l2_normalize, burn_mean_pooling,
    load_nomic_bert_burn, token_ids_to_burn_tensor,
};

// Feature-gated concrete backend type for the burn path.
#[cfg(feature = "wgpu")]
type ActiveBackend = burn::backend::Wgpu;

/// Known model identifiers and their HuggingFace repository locations.
pub struct ModelRegistry;

impl ModelRegistry {
    /// Name of the default model used when config has no `embedding_model`.
    pub const DEFAULT_MODEL: &'static str = "nomic-embed-text-v1.5";

    /// Resolve a user-supplied model name to a `ModelSpec`.
    /// Returns `None` for unknown names.
    pub fn lookup(name: &str) -> Option<ModelSpec> {
        KNOWN_MODELS.iter().find(|m| m.name == name).copied()
    }
}

/// Download coordinates for a model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    /// Short user-facing name (e.g. "nomic-embed-text-v1.5").
    pub name: &'static str,
    /// HuggingFace repo ID (e.g. "nomic-ai/nomic-embed-text-v1.5").
    pub hf_repo: &'static str,
    /// Path within the repo to the safetensors weights file.
    pub weights_path: &'static str,
    /// Path within the repo to the model config.json file.
    pub config_path: &'static str,
    /// Path within the repo to the tokenizer file.
    pub tokenizer_path: &'static str,
    /// Sequence length cap for tokenization.
    pub max_length: usize,
    /// Output embedding dimension.
    pub embedding_dim: usize,
}

static KNOWN_MODELS: &[ModelSpec] = &[ModelSpec {
    name: "nomic-embed-text-v1.5",
    hf_repo: "nomic-ai/nomic-embed-text-v1.5",
    weights_path: "model.safetensors",
    config_path: "config.json",
    tokenizer_path: "tokenizer.json",
    max_length: 512,
    embedding_dim: 768,
}];

/// Loaded embedder ready for inference.
#[cfg(feature = "wgpu")]
pub struct Embedder {
    model: Mutex<BurnNomicBertModel<ActiveBackend>>,
    tokenizer: Tokenizer,
    max_length: usize,
}

/// Placeholder Embedder for the ort-cpu build path.
/// Inference implementation is filled in during Phase 2.
#[cfg(feature = "ort-cpu")]
pub struct Embedder {
    // Phase 2 will use these fields; suppress dead_code until then.
    #[allow(dead_code)]
    tokenizer: Tokenizer,
    #[allow(dead_code)]
    max_length: usize,
}

#[cfg(feature = "wgpu")]
impl Embedder {
    /// Load or download the named model into `cache_dir`.
    /// Displays a progress bar if a download is needed.
    pub async fn load(model_name: &str, cache_dir: &Path) -> Result<Self> {
        let spec = ModelRegistry::lookup(model_name)
            .with_context(|| format!("unknown model: {model_name}"))?;

        let (weights_file, config_file, tokenizer_file) =
            ensure_model_files(&spec, cache_dir).await?;

        let config_str = std::fs::read_to_string(&config_file)
            .with_context(|| format!("failed to read config {}", config_file.display()))?;

        let burn_device = get_burn_device();
        let config: NomicBertConfig =
            serde_json::from_str(&config_str).context("failed to parse NomicBert config")?;
        let model = load_nomic_bert_burn(&weights_file, &config, &burn_device)
            .context("failed to load burn NomicBertModel")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_file)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_length,
                strategy: TruncationStrategy::LongestFirst,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("failed to configure truncation: {e}"))?;

        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            max_length: spec.max_length,
        })
    }

    /// Embed a batch of texts. Returns one `Vec<f32>` per input text.
    /// Texts exceeding `max_length` tokens are truncated.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let batch_size = encodings.len();
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .min(self.max_length);

        let mut ids_data = vec![0u32; batch_size * seq_len];
        let mut mask_data = vec![0u32; batch_size * seq_len];

        for (b, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let len = ids.len().min(seq_len);
            for s in 0..len {
                ids_data[b * seq_len + s] = ids[s];
                mask_data[b * seq_len + s] = mask[s];
            }
        }

        let model = self.model.lock().unwrap();
        let burn_device = model
            .devices()
            .into_iter()
            .next()
            .expect("loaded burn model must have a device");
        let input_ids_burn =
            token_ids_to_burn_tensor::<ActiveBackend>(&ids_data, batch_size, seq_len, &burn_device);
        let attention_mask_burn = token_ids_to_burn_tensor::<ActiveBackend>(
            &mask_data,
            batch_size,
            seq_len,
            &burn_device,
        );
        let hidden = model.forward(input_ids_burn, attention_mask_burn.clone());
        let pooled = burn_mean_pooling(hidden, attention_mask_burn);
        let normalized = burn_l2_normalize(pooled);
        let tensor_data = normalized.into_data();
        let flat = tensor_data
            .as_slice::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to read burn tensor: {e:?}"))?;
        let embedding_dim = flat.len() / batch_size;
        let embeddings: Vec<Vec<f32>> = flat
            .chunks_exact(embedding_dim)
            .map(|c| c.to_vec())
            .collect();

        Ok(embeddings)
    }

    /// Convenience wrapper around `embed_batch` for a single text.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut batch = self.embed_batch(&[text])?;
        batch
            .pop()
            .context("embed_batch returned empty result for single text")
    }
}

/// ort-cpu stub implementation — filled in during Phase 2.
#[cfg(feature = "ort-cpu")]
impl Embedder {
    pub async fn load(_model_name: &str, _cache_dir: &Path) -> Result<Self> {
        unimplemented!("ort-cpu embedder load: Phase 2 not yet implemented")
    }

    pub fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        unimplemented!("ort-cpu embed_batch: Phase 2 not yet implemented")
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut batch = self.embed_batch(&[text])?;
        batch
            .pop()
            .context("embed_batch returned empty result for single text")
    }
}

/// Return the burn device to use for the burn inference path.
/// With the `wgpu` feature enabled, returns the default WGPU device (Metal / Vulkan / DX12).
#[cfg(feature = "wgpu")]
fn get_burn_device() -> <ActiveBackend as burn::tensor::backend::Backend>::Device {
    Default::default()
}

/// Download model files if missing, returning `(weights_path, config_path, tokenizer_path)`.
/// Partial downloads are written to `*.tmp` and renamed atomically on completion.
// Phase 2 will use this in the ort-cpu path; suppress dead_code in the interim.
#[allow(dead_code)]
async fn ensure_model_files(
    spec: &ModelSpec,
    cache_dir: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let model_dir = cache_dir.join(spec.name);
    std::fs::create_dir_all(&model_dir)
        .with_context(|| format!("failed to create model cache dir {}", model_dir.display()))?;

    let weights_dest = model_dir.join("model.safetensors");
    let config_dest = model_dir.join("config.json");
    let tokenizer_dest = model_dir.join("tokenizer.json");

    let base_url = format!("https://huggingface.co/{}/resolve/main", spec.hf_repo);

    if !weights_dest.exists() {
        let url = format!("{base_url}/{}", spec.weights_path);
        download_file(&url, &weights_dest)
            .await
            .with_context(|| format!("failed to download weights from {url}"))?;
    }

    if !config_dest.exists() {
        let url = format!("{base_url}/{}", spec.config_path);
        download_file(&url, &config_dest)
            .await
            .with_context(|| format!("failed to download config from {url}"))?;
    }

    if !tokenizer_dest.exists() {
        let url = format!("{base_url}/{}", spec.tokenizer_path);
        download_file(&url, &tokenizer_dest)
            .await
            .with_context(|| format!("failed to download tokenizer from {url}"))?;
    }

    Ok((weights_dest, config_dest, tokenizer_dest))
}

/// Download a URL to `dest`, writing to `dest.tmp` first then renaming atomically.
/// Streams the response body in chunks so large model files are never fully buffered.
// Phase 2 will use this in the ort-cpu path; suppress dead_code in the interim.
#[allow(dead_code)]
async fn download_file(url: &str, dest: &Path) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let tmp = dest.with_extension("tmp");

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("GET {url} failed"))?;

    if !response.status().is_success() {
        bail!("HTTP {} downloading {url}", response.status());
    }

    let total = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );
    pb.set_message(format!(
        "Downloading {}",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("failed to create {}", tmp.display()))?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("stream error during download")?;
        pb.inc(chunk.len() as u64);
        file.write_all(&chunk)
            .await
            .context("write error during download")?;
    }

    file.flush().await.context("flush failed")?;
    drop(file);
    pb.finish_with_message("done");

    // A concurrent download may have already placed `dest` while we were streaming.
    // If so, discard our tmp copy and consider the download complete.
    if dest.exists() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Ok(());
    }

    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("failed to rename {} to {}", tmp.display(), dest.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_registry_lookup_known() {
        let spec = ModelRegistry::lookup("nomic-embed-text-v1.5");
        assert!(
            spec.is_some(),
            "nomic-embed-text-v1.5 should be in registry"
        );
        let spec = spec.unwrap();
        assert_eq!(spec.name, "nomic-embed-text-v1.5");
        assert_eq!(spec.hf_repo, "nomic-ai/nomic-embed-text-v1.5");
        assert_eq!(spec.weights_path, "model.safetensors");
        assert_eq!(spec.tokenizer_path, "tokenizer.json");
        assert_eq!(spec.max_length, 512);
        assert_eq!(spec.embedding_dim, 768);
    }

    #[test]
    fn model_registry_lookup_unknown() {
        assert!(ModelRegistry::lookup("not-a-model").is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn embed_batch_returns_normalized_vectors() {
        let cache_dir = dirs::data_dir()
            .expect("no data dir")
            .join("debrief")
            .join("models");

        let embedder = Embedder::load(ModelRegistry::DEFAULT_MODEL, &cache_dir)
            .await
            .expect("failed to load embedder");

        let texts = [
            "fn main() { println!(\"hello\"); }",
            "struct Foo { x: i32 }",
        ];
        let embeddings = embedder.embed_batch(&texts).expect("embed failed");

        assert_eq!(embeddings.len(), 2);
        for emb in &embeddings {
            assert_eq!(emb.len(), 768, "embedding dim should be 768");
            let norm: f32 = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-5,
                "vector should be L2-normalized, got norm={norm}"
            );
        }
    }

    #[tokio::test]
    #[ignore]
    async fn embed_batch_truncates_long_input() {
        let cache_dir = dirs::data_dir()
            .expect("no data dir")
            .join("debrief")
            .join("models");

        let embedder = Embedder::load(ModelRegistry::DEFAULT_MODEL, &cache_dir)
            .await
            .expect("failed to load embedder");

        // 600 tokens worth of text (beyond max_length=512), should not panic
        let long_text = "token ".repeat(600);
        let result = embedder.embed(&long_text);
        assert!(result.is_ok(), "long input should not panic: {:?}", result);
    }
}
