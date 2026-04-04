use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use ort::{session::Session, value::Tensor};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

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
    /// Path within the repo to the ONNX model file.
    pub onnx_path: &'static str,
    /// Path within the repo to the tokenizer file.
    pub tokenizer_path: &'static str,
    /// Sequence length cap for tokenization.
    pub max_length: usize,
    /// Output embedding dimension.
    pub embedding_dim: usize,
}

static KNOWN_MODELS: &[ModelSpec] = &[
    ModelSpec {
        name: "nomic-embed-text-v1.5",
        hf_repo: "nomic-ai/nomic-embed-text-v1.5",
        onnx_path: "onnx/model.onnx",
        tokenizer_path: "tokenizer.json",
        max_length: 512,
        embedding_dim: 768,
    },
    ModelSpec {
        name: "bge-large-en-v1.5",
        hf_repo: "BAAI/bge-large-en-v1.5",
        onnx_path: "onnx/model.onnx",
        tokenizer_path: "tokenizer.json",
        max_length: 512,
        embedding_dim: 1024,
    },
];

/// Loaded embedder ready for inference.
pub struct Embedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    max_length: usize,
    has_token_type_ids: bool,
}

impl Embedder {
    /// Load or download the named model into `cache_dir`.
    /// Displays a progress bar if a download is needed.
    pub async fn load(model_name: &str, cache_dir: &Path) -> Result<Self> {
        let spec = ModelRegistry::lookup(model_name)
            .with_context(|| format!("unknown model: {model_name}"))?;

        let (onnx_file, tokenizer_file) = ensure_model_files(&spec, cache_dir).await?;

        let session = Session::builder()
            .context("failed to create ONNX session builder")?
            .commit_from_file(&onnx_file)
            .with_context(|| format!("failed to load ONNX model from {}", onnx_file.display()))?;

        let has_token_type_ids = session
            .inputs()
            .iter()
            .any(|outlet: &ort::value::Outlet| outlet.name() == "token_type_ids");

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
            has_token_type_ids,
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

        let mut ids_data = vec![0i64; batch_size * seq_len];
        let mut mask_data = vec![0i64; batch_size * seq_len];

        for (b, enc) in encodings.iter().enumerate() {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let len = ids.len().min(seq_len);
            for s in 0..len {
                ids_data[b * seq_len + s] = ids[s] as i64;
                mask_data[b * seq_len + s] = mask[s] as i64;
            }
        }

        let shape = vec![batch_size, seq_len];
        let ids_tensor = Tensor::<i64>::from_array((shape.as_slice(), ids_data))
            .context("failed to create ids tensor")?;
        let mask_tensor = Tensor::<i64>::from_array((shape.as_slice(), mask_data.clone()))
            .context("failed to create mask tensor")?;

        // Run inference and extract hidden state as owned data before releasing the session lock.
        let (out_seq_len, hidden_dim, hidden_owned): (usize, usize, Vec<f32>) = {
            let mut session = self.session.lock().unwrap();
            let outputs = if self.has_token_type_ids {
                let type_ids_data = vec![0i64; batch_size * seq_len];
                let type_ids_tensor = Tensor::<i64>::from_array((shape.as_slice(), type_ids_data))
                    .context("failed to create type_ids tensor")?;
                session
                    .run(ort::inputs! {
                        "input_ids" => ids_tensor,
                        "attention_mask" => mask_tensor,
                        "token_type_ids" => type_ids_tensor,
                    })
                    .context("ONNX inference failed")?
            } else {
                session
                    .run(ort::inputs! {
                        "input_ids" => ids_tensor,
                        "attention_mask" => mask_tensor,
                    })
                    .context("ONNX inference failed")?
            };

            let (out_shape, hidden_data) = outputs["last_hidden_state"]
                .try_extract_tensor::<f32>()
                .context("failed to extract last_hidden_state")?;

            // shape: [batch_size, out_seq_len, hidden_dim]
            let s = out_shape[1] as usize;
            let d = out_shape[2] as usize;
            (s, d, hidden_data.to_vec())
        };

        let mut embeddings = Vec::with_capacity(batch_size);
        for b in 0..batch_size {
            let mut pooled = vec![0.0f32; hidden_dim];
            let mut mask_sum = 0.0f32;

            for s in 0..out_seq_len {
                let m = mask_data[b * seq_len + s.min(seq_len - 1)] as f32;
                mask_sum += m;
                for d in 0..hidden_dim {
                    pooled[d] +=
                        hidden_owned[b * out_seq_len * hidden_dim + s * hidden_dim + d] * m;
                }
            }

            if mask_sum > 0.0 {
                for v in &mut pooled {
                    *v /= mask_sum;
                }
            }

            let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in &mut pooled {
                    *v /= norm;
                }
            }

            embeddings.push(pooled);
        }

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

/// Download model files if missing, returning `(onnx_path, tokenizer_path)`.
/// Partial downloads are written to `*.tmp` and renamed atomically on completion.
async fn ensure_model_files(spec: &ModelSpec, cache_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let model_dir = cache_dir.join(spec.name);
    std::fs::create_dir_all(&model_dir)
        .with_context(|| format!("failed to create model cache dir {}", model_dir.display()))?;

    let onnx_dest = model_dir.join("model.onnx");
    let tokenizer_dest = model_dir.join("tokenizer.json");

    let base_url = format!("https://huggingface.co/{}/resolve/main", spec.hf_repo);

    if !onnx_dest.exists() {
        let url = format!("{base_url}/{}", spec.onnx_path);
        download_file(&url, &onnx_dest)
            .await
            .with_context(|| format!("failed to download ONNX model from {url}"))?;
    }

    if !tokenizer_dest.exists() {
        let url = format!("{base_url}/{}", spec.tokenizer_path);
        download_file(&url, &tokenizer_dest)
            .await
            .with_context(|| format!("failed to download tokenizer from {url}"))?;
    }

    Ok((onnx_dest, tokenizer_dest))
}

/// Download a URL to `dest`, writing to `dest.tmp` first then renaming atomically.
/// Streams the response body in chunks so large model files (~130 MB) are never
/// fully buffered in memory.
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
        assert_eq!(spec.onnx_path, "onnx/model.onnx");
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
