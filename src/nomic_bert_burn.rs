/// NomicBERT model implemented manually using the burn framework.
///
/// This module provides a parallel implementation to the candle-based NomicBertModel
/// in embedder.rs. The burn path supports WGPU backends (Metal, Vulkan, DX12) for
/// GPU acceleration without the limitations of the candle Metal backend.
///
/// Architecture matches nomic-ai/nomic-embed-text-v1.5 (NomicBERT):
///   - RoPE attention (non-interleaved, GPT-NeoX style)
///   - SwiGLU FFN with 3 projections (fc11 value, fc12 gate, fc2 output)
///   - Post-norm by default (prenorm=false), config-driven
///   - No bias on attention projections or FFN projections
use std::path::Path;

use anyhow::Result;
use burn::{
    module::Module,
    nn::{
        Embedding, EmbeddingConfig, LayerNorm, LayerNormConfig, Linear, LinearConfig,
        RotaryEncoding, RotaryEncodingConfig,
    },
    tensor::{
        Int, Shape, Tensor, TensorData,
        activation::{silu, softmax},
        backend::Backend,
    },
};
use burn_store::{ModuleSnapshot, PyTorchToBurnAdapter, SafetensorsStore};
use serde::Deserialize;

/// Configuration for the NomicBERT model architecture.
///
/// Matches the fields present in the HuggingFace `config.json` for
/// `nomic-ai/nomic-embed-text-v1.5`. All fields carry `#[serde(default)]`
/// so a partial config.json still parses correctly.
#[derive(Debug, Clone, Deserialize)]
pub struct NomicBertConfig {
    #[serde(default = "NomicBertConfig::default_vocab_size")]
    pub vocab_size: usize,
    #[serde(default = "NomicBertConfig::default_n_embd")]
    pub n_embd: usize,
    #[serde(default = "NomicBertConfig::default_type_vocab_size")]
    pub type_vocab_size: usize,
    #[serde(default = "NomicBertConfig::default_n_inner")]
    pub n_inner: usize,
    #[serde(default = "NomicBertConfig::default_n_head")]
    pub n_head: usize,
    #[serde(default = "NomicBertConfig::default_n_layer")]
    pub n_layer: usize,
    #[serde(default = "NomicBertConfig::default_layer_norm_epsilon")]
    pub layer_norm_epsilon: f64,
    #[serde(default = "NomicBertConfig::default_rotary_emb_fraction")]
    pub rotary_emb_fraction: f64,
    #[serde(default = "NomicBertConfig::default_rotary_emb_base")]
    pub rotary_emb_base: f64,
    /// Maps from `max_position_embeddings` in HuggingFace config.json.
    #[serde(
        default = "NomicBertConfig::default_n_positions",
        rename = "max_position_embeddings"
    )]
    pub n_positions: usize,
    #[serde(default = "NomicBertConfig::default_qkv_proj_bias")]
    pub qkv_proj_bias: bool,
    #[serde(default = "NomicBertConfig::default_prenorm")]
    pub prenorm: bool,
    #[serde(default = "NomicBertConfig::default_pad_token_id")]
    pub pad_token_id: usize,
}

impl NomicBertConfig {
    fn default_vocab_size() -> usize {
        30528
    }
    fn default_n_embd() -> usize {
        768
    }
    fn default_type_vocab_size() -> usize {
        2
    }
    fn default_n_inner() -> usize {
        2048
    }
    fn default_n_head() -> usize {
        12
    }
    fn default_n_layer() -> usize {
        12
    }
    fn default_layer_norm_epsilon() -> f64 {
        1e-12
    }
    fn default_rotary_emb_fraction() -> f64 {
        1.0
    }
    fn default_rotary_emb_base() -> f64 {
        10000.0
    }
    fn default_n_positions() -> usize {
        2048
    }
    fn default_qkv_proj_bias() -> bool {
        false
    }
    fn default_prenorm() -> bool {
        false
    }
    fn default_pad_token_id() -> usize {
        0
    }
}

impl Default for NomicBertConfig {
    fn default() -> Self {
        Self {
            vocab_size: Self::default_vocab_size(),
            n_embd: Self::default_n_embd(),
            type_vocab_size: Self::default_type_vocab_size(),
            n_inner: Self::default_n_inner(),
            n_head: Self::default_n_head(),
            n_layer: Self::default_n_layer(),
            layer_norm_epsilon: Self::default_layer_norm_epsilon(),
            rotary_emb_fraction: Self::default_rotary_emb_fraction(),
            rotary_emb_base: Self::default_rotary_emb_base(),
            n_positions: Self::default_n_positions(),
            qkv_proj_bias: Self::default_qkv_proj_bias(),
            prenorm: Self::default_prenorm(),
            pad_token_id: Self::default_pad_token_id(),
        }
    }
}

// ---------------------------------------------------------------------------
// Embeddings
// ---------------------------------------------------------------------------

#[derive(Module, Debug)]
struct BurnNomicBertEmbeddings<B: Backend> {
    word_embeddings: Embedding<B>,
    token_type_embeddings: Embedding<B>,
}

impl<B: Backend> BurnNomicBertEmbeddings<B> {
    fn init(config: &NomicBertConfig, device: &B::Device) -> Self {
        Self {
            word_embeddings: EmbeddingConfig::new(config.vocab_size, config.n_embd).init(device),
            token_type_embeddings: EmbeddingConfig::new(config.type_vocab_size, config.n_embd)
                .init(device),
        }
    }

    fn forward(
        &self,
        input_ids: Tensor<B, 2, Int>,
        token_type_ids: Tensor<B, 2, Int>,
    ) -> Tensor<B, 3> {
        let word_emb = self.word_embeddings.forward(input_ids);
        let tte = self.token_type_embeddings.forward(token_type_ids);
        word_emb + tte
    }
}

// ---------------------------------------------------------------------------
// SwiGLU FFN (3-layer: value + gate + projection)
// ---------------------------------------------------------------------------

#[derive(Module, Debug)]
struct BurnNomicBertSwiGLU<B: Backend> {
    fc11: Linear<B>,
    fc12: Linear<B>,
    fc2: Linear<B>,
}

impl<B: Backend> BurnNomicBertSwiGLU<B> {
    fn init(config: &NomicBertConfig, device: &B::Device) -> Self {
        let fc_in = LinearConfig::new(config.n_embd, config.n_inner).with_bias(false);
        let fc_out = LinearConfig::new(config.n_inner, config.n_embd).with_bias(false);
        Self {
            fc11: fc_in.clone().init(device),
            fc12: fc_in.init(device),
            fc2: fc_out.init(device),
        }
    }

    fn forward(&self, x: Tensor<B, 3>) -> Tensor<B, 3> {
        let value = self.fc11.forward(x.clone());
        let gate = silu(self.fc12.forward(x));
        self.fc2.forward(value * gate)
    }
}

// ---------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------

#[derive(Module, Debug)]
struct BurnNomicBertAttention<B: Backend> {
    wqkv: Linear<B>,
    out_proj: Linear<B>,
    rotary_enc: RotaryEncoding<B>,
    num_heads: usize,
    head_dim: usize,
    n_embd: usize,
}

impl<B: Backend> BurnNomicBertAttention<B> {
    fn init(config: &NomicBertConfig, device: &B::Device) -> Self {
        let head_dim = config.n_embd / config.n_head;
        let rotary_emb_dim = (config.rotary_emb_fraction * head_dim as f64) as usize;
        assert_eq!(
            rotary_emb_dim, head_dim,
            "partial RoPE rotation (rotary_emb_fraction < 1.0) is not supported"
        );

        // RotaryEncodingConfig takes (max_sequence_length, d_model)
        let rotary_enc = RotaryEncodingConfig::new(config.n_positions, rotary_emb_dim)
            .with_theta(config.rotary_emb_base as f32)
            .init(device);

        Self {
            wqkv: LinearConfig::new(config.n_embd, 3 * config.n_embd)
                .with_bias(config.qkv_proj_bias)
                .init(device),
            out_proj: LinearConfig::new(config.n_embd, config.n_embd)
                .with_bias(false)
                .init(device),
            rotary_enc,
            num_heads: config.n_head,
            head_dim,
            n_embd: config.n_embd,
        }
    }

    /// Forward pass.
    ///
    /// `hidden_states`: `[batch, seq, n_embd]`
    /// `attention_mask`: `[batch, 1, 1, seq]` float, 0.0 for attend, -1e4 for pad
    ///
    /// Returns: `[batch, seq, n_embd]`
    fn forward(&self, hidden_states: Tensor<B, 3>, attention_mask: Tensor<B, 4>) -> Tensor<B, 3> {
        let [batch, seq, _n_embd] = hidden_states.dims();

        // [batch, seq, 3*n_embd]
        let qkv = self.wqkv.forward(hidden_states);

        // Slice into q, k, v each [batch, seq, n_embd]
        let q = qkv.clone().slice([0..batch, 0..seq, 0..self.n_embd]);
        let k = qkv
            .clone()
            .slice([0..batch, 0..seq, self.n_embd..2 * self.n_embd]);
        let v = qkv.slice([0..batch, 0..seq, 2 * self.n_embd..3 * self.n_embd]);

        // Reshape to [batch, seq, num_heads, head_dim] then transpose to [batch, num_heads, seq, head_dim]
        let q = q
            .reshape([batch, seq, self.num_heads, self.head_dim])
            .swap_dims(1, 2);
        let k = k
            .reshape([batch, seq, self.num_heads, self.head_dim])
            .swap_dims(1, 2);
        let v = v
            .reshape([batch, seq, self.num_heads, self.head_dim])
            .swap_dims(1, 2);

        // Apply RoPE — burn RotaryEncoding works on (..., seq, d_model) tensors.
        // Our tensors are [batch, num_heads, seq, head_dim], which satisfies this shape.
        let q = self.rotary_enc.forward(q);
        let k = self.rotary_enc.forward(k);

        // Scaled dot-product attention
        let scale = (self.head_dim as f32).sqrt();
        // k transposed: swap last two dims → [batch, num_heads, head_dim, seq]
        let scores = q.matmul(k.swap_dims(2, 3)).div_scalar(scale) + attention_mask;
        let probs = softmax(scores, 3);

        // Context: [batch, num_heads, seq, head_dim]
        let ctx = probs.matmul(v);

        // Transpose back to [batch, seq, num_heads, head_dim], then reshape to [batch, seq, n_embd]
        let ctx = ctx.swap_dims(1, 2).reshape([batch, seq, self.n_embd]);

        self.out_proj.forward(ctx)
    }
}

// ---------------------------------------------------------------------------
// Transformer block
// ---------------------------------------------------------------------------

#[derive(Module, Debug)]
struct BurnNomicBertBlock<B: Backend> {
    attn: BurnNomicBertAttention<B>,
    mlp: BurnNomicBertSwiGLU<B>,
    norm1: LayerNorm<B>,
    norm2: LayerNorm<B>,
    prenorm: bool,
}

impl<B: Backend> BurnNomicBertBlock<B> {
    fn init(config: &NomicBertConfig, device: &B::Device) -> Self {
        let ln_config = LayerNormConfig::new(config.n_embd).with_epsilon(config.layer_norm_epsilon);
        Self {
            attn: BurnNomicBertAttention::init(config, device),
            mlp: BurnNomicBertSwiGLU::init(config, device),
            norm1: ln_config.clone().init(device),
            norm2: ln_config.init(device),
            prenorm: config.prenorm,
        }
    }

    fn forward(&self, hidden_states: Tensor<B, 3>, attention_mask: Tensor<B, 4>) -> Tensor<B, 3> {
        if self.prenorm {
            // Pre-norm: normalize before each sub-layer
            let normed1 = self.norm1.forward(hidden_states.clone());
            let attn_out = self.attn.forward(normed1, attention_mask.clone());
            let hidden = hidden_states + attn_out;
            let normed2 = self.norm2.forward(hidden.clone());
            let mlp_out = self.mlp.forward(normed2);
            hidden + mlp_out
        } else {
            // Post-norm (default): apply norm after residual connection
            let attn_out = self.attn.forward(hidden_states.clone(), attention_mask);
            let hidden = self.norm1.forward(hidden_states + attn_out);
            let mlp_out = self.mlp.forward(hidden.clone());
            self.norm2.forward(hidden + mlp_out)
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder wrapper — produces "encoder.layers.{i}.*" key paths
// ---------------------------------------------------------------------------

#[derive(Module, Debug)]
struct BurnNomicBertEncoder<B: Backend> {
    layers: Vec<BurnNomicBertBlock<B>>,
}

impl<B: Backend> BurnNomicBertEncoder<B> {
    fn init(config: &NomicBertConfig, device: &B::Device) -> Self {
        let layers = (0..config.n_layer)
            .map(|_| BurnNomicBertBlock::init(config, device))
            .collect();
        Self { layers }
    }

    fn forward(&self, mut hidden: Tensor<B, 3>, attention_mask: Tensor<B, 4>) -> Tensor<B, 3> {
        for layer in &self.layers {
            hidden = layer.forward(hidden, attention_mask.clone());
        }
        hidden
    }
}

// ---------------------------------------------------------------------------
// Top-level model
// ---------------------------------------------------------------------------

/// NomicBERT model implemented in burn.
///
/// Weight key path structure produced by Module derive:
/// ```text
/// embeddings.word_embeddings.weight
/// embeddings.token_type_embeddings.weight
/// emb_ln.gamma / emb_ln.beta
/// encoder.layers.{i}.attn.wqkv.weight
/// encoder.layers.{i}.attn.out_proj.weight
/// encoder.layers.{i}.mlp.fc11.weight / fc12.weight / fc2.weight
/// encoder.layers.{i}.norm1.gamma / norm1.beta
/// encoder.layers.{i}.norm2.gamma / norm2.beta
/// ```
#[derive(Module, Debug)]
pub struct BurnNomicBertModel<B: Backend> {
    embeddings: BurnNomicBertEmbeddings<B>,
    emb_ln: LayerNorm<B>,
    encoder: BurnNomicBertEncoder<B>,
}

impl<B: Backend> BurnNomicBertModel<B> {
    /// Allocate the model with random weights. Used as scaffolding before `load_record`.
    pub fn init(config: &NomicBertConfig, device: &B::Device) -> Self {
        let emb_ln = LayerNormConfig::new(config.n_embd)
            .with_epsilon(config.layer_norm_epsilon)
            .init(device);
        Self {
            embeddings: BurnNomicBertEmbeddings::init(config, device),
            emb_ln,
            encoder: BurnNomicBertEncoder::init(config, device),
        }
    }

    /// Run the model on a batch.
    ///
    /// - `input_ids`: `[batch, seq]` Int tensor (token ids)
    /// - `attention_mask`: `[batch, seq]` Int tensor (1=attend, 0=pad)
    ///
    /// Returns hidden states `[batch, seq, n_embd]`.
    pub fn forward(
        &self,
        input_ids: Tensor<B, 2, Int>,
        attention_mask: Tensor<B, 2, Int>,
    ) -> Tensor<B, 3> {
        let [batch, seq] = input_ids.dims();
        let device = input_ids.device();

        // Build token_type_ids: all zeros, same shape as input_ids
        let token_type_ids = Tensor::<B, 2, Int>::zeros([batch, seq], &device);

        let mut hidden = self.embeddings.forward(input_ids, token_type_ids);
        hidden = self.emb_ln.forward(hidden);

        // Build extended attention mask: [batch, 1, 1, seq] float
        // 1 → 0.0 (attend), 0 → -1e4 (mask out)
        let mask_f = attention_mask.float();
        // [batch, 1, 1, seq]
        let mask_exp = mask_f
            .clone()
            .unsqueeze_dim::<3>(1) // [batch, 1, seq]
            .unsqueeze_dim::<4>(2); // [batch, 1, 1, seq]
        // Convert: 1→0.0, 0→-10000.0
        let ones = Tensor::<B, 4>::ones([batch, 1, 1, seq], &device);
        let extended_mask =
            (ones - mask_exp) * Tensor::<B, 4>::full([batch, 1, 1, seq], -1e4, &device);

        self.encoder.forward(hidden, extended_mask)
    }
}

// ---------------------------------------------------------------------------
// Weight loading
// ---------------------------------------------------------------------------

/// Load a NomicBERT model from a safetensors file, applying HuggingFace → burn key remapping.
///
/// Key remapping applied (in order):
/// 1. Uppercase `Wqkv` → lowercase `wqkv` in attention layers
/// 2. LayerNorm weight/bias → gamma/beta is handled automatically by `PyTorchToBurnAdapter`
/// 3. Linear weight transpose (PyTorch: [out, in] → burn: [in, out]) via `PyTorchToBurnAdapter`
pub fn load_nomic_bert_burn<B: Backend>(
    weights_path: &Path,
    config: &NomicBertConfig,
    device: &B::Device,
) -> Result<BurnNomicBertModel<B>> {
    let mut model = BurnNomicBertModel::init(config, device);

    let mut store = SafetensorsStore::from_file(weights_path)
        // Wqkv (uppercase) → wqkv (lowercase) to match Rust field naming
        .with_key_remapping(r"(encoder\.layers\.\d+\.attn)\.Wqkv\.", "$1.wqkv.")
        // PyTorchToBurnAdapter handles:
        //   - Linear weight transpose [out,in] → [in,out]
        //   - LayerNorm: weight → gamma, bias → beta
        .with_from_adapter(PyTorchToBurnAdapter)
        // Checkpoint may have extra keys (position_ids, etc.) that we don't model
        .allow_partial(true);

    model
        .load_from(&mut store)
        .map_err(|e| anyhow::anyhow!("failed to load safetensors weights: {e}"))?;

    Ok(model)
}

// ---------------------------------------------------------------------------
// Post-processing
// ---------------------------------------------------------------------------

/// Mean pooling over the sequence dimension, weighted by the attention mask.
///
/// `hidden`: `[batch, seq, n_embd]`
/// `attention_mask`: `[batch, seq]` Int (1=token, 0=pad)
///
/// Returns `[batch, n_embd]`.
pub fn burn_mean_pooling<B: Backend>(
    hidden: Tensor<B, 3>,
    attention_mask: Tensor<B, 2, Int>,
) -> Tensor<B, 2> {
    let mask_f = attention_mask.float();
    // [batch, seq, 1] → broadcast to [batch, seq, n_embd]
    let mask_exp = mask_f.clone().unsqueeze_dim::<3>(2);
    let sum_hidden = (hidden * mask_exp).sum_dim(1).squeeze_dim::<2>(1);
    let sum_mask = mask_f
        .sum_dim(1)
        .squeeze_dim::<1>(1)
        .clamp(1e-9, f64::MAX)
        .unsqueeze_dim::<2>(1);
    sum_hidden / sum_mask
}

/// L2-normalize each row of a 2D tensor.
///
/// `x`: `[batch, n_embd]`
///
/// Returns `[batch, n_embd]` with each row having unit L2 norm.
pub fn burn_l2_normalize<B: Backend>(x: Tensor<B, 2>) -> Tensor<B, 2> {
    let norm = x
        .clone()
        .powf_scalar(2.0_f32)
        .sum_dim(1)
        .squeeze_dim::<1>(1)
        .sqrt()
        .clamp(1e-12, f64::MAX)
        .unsqueeze_dim::<2>(1);
    x / norm
}

// ---------------------------------------------------------------------------
// Helper: build a burn Int tensor from flat u32 token data
// ---------------------------------------------------------------------------

/// Convert a flat `u32` buffer (as produced by the tokenizer) into a burn Int tensor
/// of shape `[batch, seq]`. Burn's NdArray and Wgpu backends use `i64` for Int elements,
/// so values are widened from u32 to i64.
pub fn token_ids_to_burn_tensor<B: Backend>(
    ids: &[u32],
    batch: usize,
    seq: usize,
    device: &B::Device,
) -> Tensor<B, 2, Int> {
    let data: Vec<i64> = ids.iter().map(|&x| x as i64).collect();
    Tensor::from_data(TensorData::new(data, Shape::new([batch, seq])), device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use burn::backend::NdArray;

    type TestBackend = NdArray;

    fn default_config() -> NomicBertConfig {
        NomicBertConfig::default()
    }

    #[test]
    fn model_init_does_not_panic() {
        let device = Default::default();
        let config = default_config();
        let _model = BurnNomicBertModel::<TestBackend>::init(&config, &device);
    }

    #[test]
    fn token_ids_to_burn_tensor_shape() {
        let device = Default::default();
        let ids = vec![101u32, 2003, 102, 0];
        let t = token_ids_to_burn_tensor::<TestBackend>(&ids, 1, 4, &device);
        assert_eq!(t.dims(), [1, 4]);
    }

    #[test]
    fn mean_pooling_masked() {
        // 2 sequences, seq_len=3, emb_dim=2
        // seq 0: tokens at [0,1], pad at [2]
        // seq 1: tokens at [0,1,2]
        let device: <TestBackend as Backend>::Device = Default::default();
        let hidden = Tensor::<TestBackend, 3>::from_data(
            TensorData::new(
                vec![
                    1.0f32, 1.0, 2.0, 2.0, 0.0, 0.0, // seq 0: [1,1],[2,2],[0,0]
                    3.0, 3.0, 4.0, 4.0, 5.0, 5.0, // seq 1: [3,3],[4,4],[5,5]
                ],
                vec![2, 3, 2],
            ),
            &device,
        );
        let mask = Tensor::<TestBackend, 2, Int>::from_data(
            TensorData::new(vec![1i64, 1, 0, 1, 1, 1], vec![2, 3]),
            &device,
        );
        let pooled = burn_mean_pooling(hidden, mask);
        let data = pooled.into_data();
        let vals = data.as_slice::<f32>().unwrap();
        // seq 0: (1+2)/2 = 1.5 for both dims
        assert!(
            (vals[0] - 1.5).abs() < 1e-5,
            "expected 1.5, got {}",
            vals[0]
        );
        assert!(
            (vals[1] - 1.5).abs() < 1e-5,
            "expected 1.5, got {}",
            vals[1]
        );
        // seq 1: (3+4+5)/3 = 4.0 for both dims
        assert!(
            (vals[2] - 4.0).abs() < 1e-5,
            "expected 4.0, got {}",
            vals[2]
        );
        assert!(
            (vals[3] - 4.0).abs() < 1e-5,
            "expected 4.0, got {}",
            vals[3]
        );
    }

    #[test]
    fn l2_normalize_unit_norm() {
        let device: <TestBackend as Backend>::Device = Default::default();
        let x = Tensor::<TestBackend, 2>::from_data(
            TensorData::new(vec![3.0f32, 4.0, 0.0, 1.0], vec![2, 2]),
            &device,
        );
        let normed = burn_l2_normalize(x);
        let data = normed.into_data();
        let vals = data.as_slice::<f32>().unwrap();
        // row 0: [3,4] → norm=5 → [0.6, 0.8]
        assert!(
            (vals[0] - 0.6).abs() < 1e-5,
            "expected 0.6, got {}",
            vals[0]
        );
        assert!(
            (vals[1] - 0.8).abs() < 1e-5,
            "expected 0.8, got {}",
            vals[1]
        );
        // row 1: [0,1] → norm=1 → [0, 1]
        assert!((vals[2] - 0.0).abs() < 1e-5);
        assert!((vals[3] - 1.0).abs() < 1e-5);
    }
}
