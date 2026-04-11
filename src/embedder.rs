use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
// Tokenizer import is shared by both backends (both structs carry a Tokenizer field).
// The no-feature build is caught by compile_error in lib.rs before this is relevant.
#[cfg(any(feature = "wgpu", feature = "ort-cpu"))]
use tokenizers::Tokenizer;

// Tokenizer configuration types used by both wgpu and ort-cpu inference paths.
#[cfg(any(feature = "wgpu", feature = "ort-cpu"))]
use tokenizers::{PaddingParams, PaddingStrategy, TruncationParams, TruncationStrategy};

#[cfg(any(feature = "wgpu", feature = "ort-cpu"))]
use std::sync::Mutex;

#[cfg(feature = "ort-cpu")]
use ort::{
    ep,
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};

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

/// Embedder backed by ONNX Runtime (CPU execution provider).
#[cfg(feature = "ort-cpu")]
pub struct Embedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
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

/// ort-cpu inference implementation.
#[cfg(feature = "ort-cpu")]
impl Embedder {
    pub async fn load(model_name: &str, cache_dir: &Path) -> Result<Self> {
        let spec = ModelRegistry::lookup(model_name)
            .with_context(|| format!("unknown model: {model_name}"))?;

        let (onnx_file, _config_file, tokenizer_file) =
            ensure_model_files(&spec, cache_dir).await?;

        let cpu_ep = ep::CPU::default().build();
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create session builder: {e}"))?
            .with_execution_providers([cpu_ep])
            .map_err(|e| anyhow::anyhow!("failed to register CPU EP: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("failed to set optimization level: {e}"))?
            .commit_from_file(&onnx_file)
            .with_context(|| format!("failed to load ONNX model from {}", onnx_file.display()))?;

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
            session: Mutex::new(session),
            tokenizer,
            max_length: spec.max_length,
        })
    }

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

        let mut input_ids_data = vec![0i64; batch_size * seq_len];
        let mut attention_mask_data = vec![0i64; batch_size * seq_len];

        for (b, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let len = ids.len().min(seq_len);
            for s in 0..len {
                input_ids_data[b * seq_len + s] = ids[s] as i64;
                attention_mask_data[b * seq_len + s] = mask[s] as i64;
            }
        }

        let shape = [batch_size, seq_len];
        let input_ids_tensor = Tensor::<i64>::from_array((shape, input_ids_data.clone()))
            .context("failed to create input_ids tensor")?;
        let attention_mask_tensor = Tensor::<i64>::from_array((shape, attention_mask_data.clone()))
            .context("failed to create attention_mask tensor")?;

        let mut session = self.session.lock().unwrap();
        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            ])
            .context("ONNX session run failed")?;

        let output_val = &outputs["last_hidden_state"];
        let (out_shape, data) = output_val
            .try_extract_tensor::<f32>()
            .context("failed to extract last_hidden_state tensor")?;

        // Shape: [batch_size, seq_len, embedding_dim]
        let embedding_dim = out_shape[2] as usize;
        debug_assert_eq!(out_shape[0] as usize, batch_size);
        debug_assert_eq!(out_shape[1] as usize, seq_len);

        let mut result = Vec::with_capacity(batch_size);
        for b in 0..batch_size {
            let mut sum = vec![0.0f32; embedding_dim];
            let mut weight_sum = 0.0f32;

            for s in 0..seq_len {
                if attention_mask_data[b * seq_len + s] == 0 {
                    continue;
                }
                weight_sum += 1.0;
                let offset = b * seq_len * embedding_dim + s * embedding_dim;
                for h in 0..embedding_dim {
                    sum[h] += data[offset + h];
                }
            }

            let weight_sum = if weight_sum == 0.0 { 1e-9 } else { weight_sum };
            for h in 0..embedding_dim {
                sum[h] /= weight_sum;
            }

            // L2 normalize
            let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
            for h in 0..embedding_dim {
                sum[h] /= norm;
            }

            result.push(sum);
        }

        Ok(result)
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
/// Files are placed under `cache_dir/<model-name>/<backend>/` where backend is `"burn"` for wgpu
/// and `"ort"` for ort-cpu. The weights file is `model.safetensors` (wgpu) or `model.onnx` (ort-cpu).
/// Partial downloads are written to `*.tmp` and renamed atomically on completion.
async fn ensure_model_files(
    spec: &ModelSpec,
    cache_dir: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    #[cfg(feature = "wgpu")]
    let (backend_subdir, weights_filename, weights_url_path) =
        ("burn", "model.safetensors", spec.weights_path);

    #[cfg(feature = "ort-cpu")]
    let (backend_subdir, weights_filename, weights_url_path) =
        ("ort", "model.onnx", "onnx/model.onnx");

    let model_dir = cache_dir.join(spec.name).join(backend_subdir);
    std::fs::create_dir_all(&model_dir)
        .with_context(|| format!("failed to create model cache dir {}", model_dir.display()))?;

    let weights_dest = model_dir.join(weights_filename);
    let config_dest = model_dir.join("config.json");
    let tokenizer_dest = model_dir.join("tokenizer.json");

    let base_url = format!("https://huggingface.co/{}/resolve/main", spec.hf_repo);

    if !weights_dest.exists() {
        let url = format!("{base_url}/{weights_url_path}");
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
