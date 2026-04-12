---
title: "GPU Embedding Acceleration — burn + WGPU Cross-Platform Backend"
category: epic
priority: high
related:
  260404-fix-gpu-acceleration: "candle migration (Phase 1-2 done, Metal failed)"
  260404-feat-dependency-chunking: "GPU needed for practical dep indexing (206K chunks)"
---

# GPU Embedding Acceleration — burn + WGPU

## Goal

Replace candle inference backend with burn + WGPU to achieve cross-platform
GPU-accelerated embedding. burn's WGPU backend supports Metal (macOS),
Vulkan (Linux), and DX12 (Windows) through a single code path — and
critically, implements LayerNorm (which candle Metal lacks).

## Motivation

| Backend | Status | Blocker |
|---------|--------|---------|
| ort CoreML | 40 GB RSS explosion | ort upstream bug, all stable versions yanked |
| candle Metal | LayerNorm not implemented | candle upstream gap |
| candle CPU | Works, ~27s/batch | 206K dep chunks → ~24h. Impractical |
| **burn WGPU** | All ops supported | **Viable path** |

Without GPU acceleration, dependency indexing (the feature that makes this
tool production-useful) is impractical. A representative project
(cargo-debrief itself) produces 206,921 dependency chunks requiring
~24 hours of CPU embedding time.

## Research Summary (2026-04-05)

### burn framework (v0.20.1 stable)

- **WGPU backend** covers Metal, Vulkan, DX12, OpenGL, WebGPU
- **All transformer ops available:** LayerNorm, RotaryEncoding, SwiGLU,
  Linear, Embedding, Softmax, GELU — as generic tensor operations
  decomposed for any backend (not per-backend kernels)
- **Kernel fusion** system (CubeCL) optimizes elementwise ops automatically
- **Safetensors loading** via `burn-import` with key remapping
- **No NomicBERT model exists** — must be implemented (~300-500 LOC)
- Published on crates.io, stable, no yanked versions

### burn-onnx alternative

- Build-time ONNX → Rust code generation
- Nearly all ONNX ops implemented (MiniLM verified)
- Could auto-generate NomicBERT model from ONNX file
- Trade-off: faster to implement but generated code less maintainable

## Scope and Decomposition

### Child ticket 1: NomicBERT model in burn

Implement NomicBERT architecture using burn's `nn` module:
- Embedding layer (no positional embeddings — RoPE applied in attention)
- Encoder layers: attention with RotaryEncoding + SwiGLU FFN
- Pre-norm LayerNorm placement
- mean_pooling + l2_normalize post-processing
- ~300-500 LOC, reference: `candle_transformers::models::nomic_bert`

Weight loading from safetensors via `SafetensorsFileRecorder` with
key remapping from HuggingFace naming to burn parameter naming.

#### Result (0e04e66) - 26-04-05

Implemented in `src/nomic_bert_burn.rs` (517 LOC). Architecture: Embeddings,
SwiGLU (manual 3-layer), Attention with RotaryEncoding, Block (pre/post-norm),
Encoder, Model. Weight loading via burn-store `SafetensorsStore` +
`PyTorchToBurnAdapter` (diverges from plan's `SafetensorsFileRecorder` — correct
per actual burn 0.20 API). Encoder struct wrapper produces correct key paths
naturally. `BurnNomicBert` variant added to `EmbedderModel`, `wgpu` feature flag,
`nomic-embed-text-v1.5-burn` registry entry. INDEX_VERSION 6, DEPS_INDEX_VERSION 3.

Deviations: burn-store API instead of burn-import (plan approximated wrong API),
RotaryEncodingConfig arg order corrected. Review finding: partial RoPE assert
guard added (fraction < 1.0 unsupported). Candle path fully preserved.

Pending validation: network integration test (numerical consistency burn vs candle,
cosine sim ≥ 0.99). Requires model download.

### Child ticket 2: BertModel in burn (for bge-large-en-v1.5)

Standard BERT architecture. `bert-burn` exists in tracel-ai/models
but uses RoBERTa variant. May need adaptation or fresh implementation.
Lower priority than NomicBERT.

### Child ticket 3: Embedder integration

Replace candle with burn+wgpu in `src/embedder.rs`:
- `Embedder` struct becomes generic over `Backend` or uses concrete
  `burn_wgpu::Wgpu` type
- Device auto-detection (WGPU adapter enumeration)
- embed_batch with burn Tensor ops
- INDEX_VERSION bump (embeddings change numerically)

### Child ticket 4: Validation and benchmarking

- RSS on macOS Metal via WGPU: target < 2 GB
- Wall-clock comparison vs candle CPU
- Search quality eval (24-query suite, baseline 15/24)
- Dependency indexing on representative project

### Child ticket 5: Cleanup

- Remove candle dependencies from Cargo.toml
- Remove RSS diagnostic instrumentation
- Update mental model, spec
- Final feature flags: `gpu` (enables burn-wgpu) or keep `metal`/`cuda`?
  WGPU is backend-agnostic so a single `gpu` flag may suffice.

## Alternative: burn-onnx approach

Instead of manually implementing NomicBERT, use burn-onnx to generate
model code from the ONNX file at build time. This collapses child
tickets 1-2 into a single auto-generation step. Evaluate feasibility
during planning — key question is whether all NomicBERT-specific ops
(SwiGLU decomposed in ONNX, rotary embeddings) are handled correctly
by burn-onnx's code generator.

## Open Questions

- burn-wgpu performance for batch embedding: is WGPU compute shader
  overhead acceptable vs native Metal/CUDA? No published benchmarks
  for BERT-class inference.
- Weight name mapping between HuggingFace checkpoints and burn
  parameter naming — requires careful validation.
- burn's `Backend` generic infects type signatures. Should Embedder
  be generic or use a concrete backend type?
- burn-onnx vs manual implementation — which is more maintainable
  long-term? Manual is cleaner but more effort.
- burn v0.20 stability — pre-1.0 framework, API may change.
