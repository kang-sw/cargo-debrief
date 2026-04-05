use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use burn::prelude::Module;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{
    bert::{BertModel, Config as BertConfig},
    nomic_bert::{Config as NomicBertConfig, NomicBertModel, l2_normalize, mean_pooling},
};
use indicatif::{ProgressBar, ProgressStyle};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use crate::nomic_bert_burn::{
    BurnNomicBertModel, burn_l2_normalize, burn_mean_pooling, load_nomic_bert_burn,
    token_ids_to_burn_tensor,
};

// Feature-gated concrete backend type for the burn path.
// NdArray is used when no GPU feature is active (testing / CPU fallback).
#[cfg(feature = "wgpu")]
type ActiveBackend = burn::backend::Wgpu;

#[cfg(not(feature = "wgpu"))]
type ActiveBackend = burn::backend::NdArray;

/// Prints current RSS (from `ps`) to stderr for memory profiling.
fn log_rss(label: &str) {
    let pid = std::process::id();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(out) => {
            let raw = String::from_utf8_lossy(&out.stdout);
            match raw.trim().parse::<u64>() {
                Ok(kb) => eprintln!("[RSS] {}: {:.1} MB", label, kb as f64 / 1024.0),
                Err(_) => eprintln!("[RSS] {}: (unavailable)", label),
            }
        }
        Err(_) => eprintln!("[RSS] {}: (unavailable)", label),
    }
}

/// Counts how many times `embed_batch` has called model inference; used to
/// limit RSS logging to the first 3 calls.
static EMBED_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

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

/// Discriminates which model architecture to load and run.
#[derive(Debug, Clone, Copy)]
pub enum ModelKind {
    NomicBert,
    Bert,
    /// NomicBERT implemented in burn (GPU via WGPU, CPU via NdArray).
    BurnNomicBert,
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
    /// Model architecture for dispatch.
    pub model_kind: ModelKind,
}

static KNOWN_MODELS: &[ModelSpec] = &[
    ModelSpec {
        name: "nomic-embed-text-v1.5",
        hf_repo: "nomic-ai/nomic-embed-text-v1.5",
        weights_path: "model.safetensors",
        config_path: "config.json",
        tokenizer_path: "tokenizer.json",
        max_length: 512,
        embedding_dim: 768,
        model_kind: ModelKind::NomicBert,
    },
    ModelSpec {
        name: "bge-large-en-v1.5",
        hf_repo: "BAAI/bge-large-en-v1.5",
        weights_path: "model.safetensors",
        config_path: "config.json",
        tokenizer_path: "tokenizer.json",
        max_length: 512,
        embedding_dim: 1024,
        model_kind: ModelKind::Bert,
    },
    // Burn-based NomicBERT — same weights as the candle path, burn backend for WGPU GPU support.
    // Uses the same HF repo and weights; only the inference path differs.
    ModelSpec {
        name: "nomic-embed-text-v1.5-burn",
        hf_repo: "nomic-ai/nomic-embed-text-v1.5",
        weights_path: "model.safetensors",
        config_path: "config.json",
        tokenizer_path: "tokenizer.json",
        max_length: 512,
        embedding_dim: 768,
        model_kind: ModelKind::BurnNomicBert,
    },
];

/// Model variants — dispatched at runtime based on `ModelKind`.
enum EmbedderModel {
    NomicBert(NomicBertModel),
    Bert(BertModel),
    BurnNomicBert(BurnNomicBertModel<ActiveBackend>),
}

/// Loaded embedder ready for inference.
pub struct Embedder {
    model: Mutex<EmbedderModel>,
    tokenizer: Tokenizer,
    max_length: usize,
    device: Device,
}

impl Embedder {
    /// Load or download the named model into `cache_dir`.
    /// Displays a progress bar if a download is needed.
    pub async fn load(model_name: &str, cache_dir: &Path) -> Result<Self> {
        let spec = ModelRegistry::lookup(model_name)
            .with_context(|| format!("unknown model: {model_name}"))?;

        let (weights_file, config_file, tokenizer_file) =
            ensure_model_files(&spec, cache_dir).await?;

        // Device selection: Metal > CUDA > CPU, each feature-gated.
        #[cfg(feature = "metal")]
        let device = Device::new_metal(0).unwrap_or(Device::Cpu);

        #[cfg(all(not(feature = "metal"), feature = "cuda"))]
        let device = Device::new_cuda(0).unwrap_or(Device::Cpu);

        #[cfg(not(any(feature = "metal", feature = "cuda")))]
        let device = Device::Cpu;

        let config_str = std::fs::read_to_string(&config_file)
            .with_context(|| format!("failed to read config {}", config_file.display()))?;

        let model = match spec.model_kind {
            ModelKind::BurnNomicBert => {
                // Burn path — does not use candle VarBuilder; loads via burn-store.
                let burn_device = get_burn_device();
                let config: NomicBertConfig = serde_json::from_str(&config_str)
                    .context("failed to parse NomicBert config for burn path")?;
                let m = load_nomic_bert_burn(&weights_file, &config, &burn_device)
                    .context("failed to load burn NomicBertModel")?;
                EmbedderModel::BurnNomicBert(m)
            }
            _ => {
                // Candle path — load weights via VarBuilder.
                log_rss("before VarBuilder");
                let vb = unsafe {
                    VarBuilder::from_mmaped_safetensors(&[&weights_file], DType::F32, &device)
                        .context("failed to load model weights")?
                };
                log_rss("after VarBuilder");

                match spec.model_kind {
                    ModelKind::NomicBert => {
                        let config: NomicBertConfig = serde_json::from_str(&config_str)
                            .context("failed to parse NomicBert config")?;
                        let m = NomicBertModel::load(vb, &config)
                            .context("failed to build NomicBertModel")?;
                        EmbedderModel::NomicBert(m)
                    }
                    ModelKind::Bert => {
                        let config: BertConfig = serde_json::from_str(&config_str)
                            .context("failed to parse Bert config")?;
                        let m =
                            BertModel::load(vb, &config).context("failed to build BertModel")?;
                        EmbedderModel::Bert(m)
                    }
                    ModelKind::BurnNomicBert => unreachable!(),
                }
            }
        };

        log_rss("after model construction");

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
            device,
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

        // candle embedding layers expect u32 token indices (not i64).
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

        // Burn path — use burn tensors and burn post-processing.
        {
            let model = self.model.lock().unwrap();
            if let EmbedderModel::BurnNomicBert(m) = &*model {
                let burn_device = m.devices().into_iter().next().unwrap_or_default();
                let input_ids_burn = token_ids_to_burn_tensor::<ActiveBackend>(
                    &ids_data,
                    batch_size,
                    seq_len,
                    &burn_device,
                );
                let attention_mask_burn = token_ids_to_burn_tensor::<ActiveBackend>(
                    &mask_data,
                    batch_size,
                    seq_len,
                    &burn_device,
                );
                let hidden = m.forward(input_ids_burn, attention_mask_burn.clone());
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
                return Ok(embeddings);
            }
        }

        let input_ids = Tensor::from_slice(&ids_data, (batch_size, seq_len), &self.device)
            .context("failed to create input_ids tensor")?;
        let attention_mask = Tensor::from_slice(&mask_data, (batch_size, seq_len), &self.device)
            .context("failed to create attention_mask tensor")?;

        let hidden_states = {
            let model = self.model.lock().unwrap();
            let call_index = EMBED_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
            let should_log_rss = call_index < 3;
            if should_log_rss {
                log_rss("before model.forward");
            }
            let result = match &*model {
                EmbedderModel::NomicBert(m) => {
                    m.forward(&input_ids, None, Some(&attention_mask))?
                }
                EmbedderModel::Bert(m) => {
                    let zeros = Tensor::zeros_like(&input_ids)?;
                    m.forward(&input_ids, &zeros, Some(&attention_mask))?
                }
                EmbedderModel::BurnNomicBert(_) => unreachable!(),
            };
            if should_log_rss {
                log_rss("after model.forward");
            }
            result
        };

        // mean_pooling and l2_normalize are free functions from the nomic_bert module;
        // they operate on generic Tensors so they work for both NomicBert and Bert outputs.
        let pooled = mean_pooling(&hidden_states, &attention_mask)?;
        let normalized = l2_normalize(&pooled)?;
        let embeddings: Vec<Vec<f32>> = normalized.to_vec2::<f32>()?;

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

/// Return the burn device to use for the burn inference path.
/// With the `wgpu` feature enabled, returns the default WGPU device (Metal / Vulkan / DX12).
/// Without `wgpu`, returns the NdArray CPU device.
fn get_burn_device() -> <ActiveBackend as burn::tensor::backend::Backend>::Device {
    Default::default()
}

/// Download model files if missing, returning `(weights_path, config_path, tokenizer_path)`.
/// Partial downloads are written to `*.tmp` and renamed atomically on completion.
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

    #[test]
    fn model_registry_lookup_bge() {
        let spec = ModelRegistry::lookup("bge-large-en-v1.5");
        assert!(spec.is_some());
        let spec = spec.unwrap();
        assert_eq!(spec.embedding_dim, 1024);
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
