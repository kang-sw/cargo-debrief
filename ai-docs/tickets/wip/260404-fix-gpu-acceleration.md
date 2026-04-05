---
title: "GPU Acceleration — Migrate from ort to candle for Cross-Platform GPU Embedding"
category: feat
priority: high
started: 2026-04-05
parent: null
plans:
  phase-2: 2026-04/05-1400.candle-migration
related:
  - 260404-feat-dependency-chunking  # GPU speeds up dep indexing; volume increase amplifies value
---

# GPU Acceleration — Migrate from ort to candle

## Goal

Replace the ort (ONNX Runtime) inference backend with candle (Hugging Face)
to achieve stable cross-platform GPU-accelerated embedding. Eliminates the
CoreML memory explosion bug, resolves the pre-release dependency problem,
and adds Metal (macOS) + CUDA (Linux/Windows) GPU support.

## Background

### Original Problem — CoreML Memory Explosion

CoreML EP via ort causes 40–110 GB RSS explosion on first `session.run()`.
Investigation confirmed the allocation happens during the first inference
call, not during EP registration or model loading. See Phase 1 Result below.

### Why candle, not ort fix

| Factor | ort | candle |
|--------|-----|--------|
| Stable crates.io version | **None** — all 1.x yanked, only 2.0.0-rc.12 | **0.10.2 (stable)** |
| macOS GPU | CoreML EP — 40GB explosion, upstream bug | **Metal backend** |
| Linux/Windows GPU | CUDA EP — untested | **CUDA backend** |
| nomic-embed-text-v1.5 | ONNX format | **Native `nomic_bert` module** in candle-transformers |
| mean_pooling / l2_normalize | Manual implementation | **Built-in functions** |
| Publishability | Pre-release dep blocks clean publish | No issue |

### Key candle API surface (candle-transformers 0.10.2)

- `candle_transformers::models::nomic_bert::NomicBertModel` — model struct
- `candle_transformers::models::nomic_bert::Config` — model configuration
- `candle_nn::VarBuilder` — weight loading from safetensors
- `candle_core::Device` — `Device::Metal(0)`, `Device::Cuda(0)`, `Device::Cpu`
- `mean_pooling()`, `l2_normalize()` — post-processing utilities

Model weights: HuggingFace `nomic-ai/nomic-embed-text-v1.5` repo provides
both ONNX and safetensors formats. Tokenizer (`tokenizer.json`) is shared.

## Phases

### Phase 1 — Reproduce and Diagnose [done]

Reproduce the CoreML RSS growth under controlled conditions. Isolate the
allocation site: EP registration, model compilation, or first inference.

### Result (f6cb6e4) - 26-04-05

RSS instrumentation added to `Embedder::load` and `embed_batch`. Also added
`CARGO_DEBRIEF_NO_DAEMON` env var to force in-process mode for profiling.

**Measurements:**

| Stage | RSS | Delta |
|-------|-----|-------|
| before session builder | 11.2 MB | — |
| after session builder | 19.6 MB | +8 MB |
| after EP registration | 19.7 MB | +0.1 MB |
| after commit_from_file | 1,377 MB | +1.36 GB (model load) |
| before session.run #1 | 1,393 MB | +16 MB |
| **after session.run #1** | **42,272 MB** | **+40.9 GB** |

**Conclusion:** Explosion happens on first `session.run()`, not during EP
registration or model loading. "Context leak detected, CoreAnalytics returned
false" messages appear immediately before. CoreML JIT compilation allocates
40+ GB on first inference.

Session recreation would not help — every new session's first run triggers
the same explosion. ANE disable (CPUAndGPU) made it worse (110 GB+).
ort 2.0.0-rc.12 is the latest — no upstream fix path.

**Decision:** Migrate to candle + Metal/CUDA instead of working around ort.

### Phase 2 — Replace ort with candle in embedder.rs

Replace the entire ort-based inference pipeline with candle.

**Cargo.toml changes:**
- Remove: `ort` (and `gpu`/`cuda` features referencing `ort/coreml`, `ort/cuda`)
- Add: `candle-core`, `candle-nn`, `candle-transformers` (version 0.10)
- New feature flags: `metal` → `candle-core/metal`, `cuda` → `candle-core/cuda`

**ModelRegistry changes:**
- Model spec needs safetensors file info instead of ONNX file info
- Download from same HuggingFace repo (`nomic-ai/nomic-embed-text-v1.5`)
  but fetch `model.safetensors` (or sharded `model-00001-of-*.safetensors`)
  and `config.json` instead of `onnx/model.onnx`
- Tokenizer download unchanged (`tokenizer.json`)

**Embedder::load rewrite:**
- Select device: `Device::new_metal(0)` → `Device::new_cuda(0)` → `Device::Cpu`
  (try in order, fallback on error — same silent fallback pattern as current ort)
- Load config: `serde_json::from_str` on `config.json`
- Load weights: `VarBuilder::from_buffered_safetensors` or equivalent
- Build model: `NomicBertModel::new(config, vb)`
- Store `NomicBertModel` + `Device` in Embedder struct (replacing `Session`)

**embed_batch rewrite:**
- Tokenize (unchanged — same `tokenizers` crate)
- Convert token IDs and attention mask to `candle_core::Tensor` on device
- Forward pass: `model.forward(&input_ids, &token_type_ids)` → hidden states
- `mean_pooling()` with attention mask
- `l2_normalize()`
- Convert output tensor to `Vec<Vec<f32>>`

**Mutex note:** `NomicBertModel` may or may not be `Send`. If not, keep the
`Mutex` wrapper. If it is `Send`, evaluate whether the Mutex can be removed
(daemon holds single Embedder, CLI is single-threaded).

**Second model support (bge-large-en-v1.5):** Currently in ModelRegistry but
uses standard BERT architecture, not NomicBERT. candle-transformers has a
`bert` module for this. Handle both model types via enum dispatch or defer
bge support to a follow-up ticket.

### Result (ab2c77b) - 26-04-05

ort fully replaced with candle. Embedder rewrite: EmbedderModel enum
(NomicBert + Bert), safetensors loading via VarBuilder, Device selection
(Metal/CUDA/CPU), mean_pooling/l2_normalize from candle builtins.
Feature flags: gpu/cuda → metal/cuda. INDEX_VERSION 4→5, DEPS_INDEX_VERSION 1→2.

Build passes on default, `--features metal`, `--features cuda`.
81 unit + offline integration tests pass. Mental model and spec updated.

### Phase 3 — Validate and Benchmark [partial]

### Result (validation) - 26-04-05

**candle CPU path:** Working. Unit tests pass (81 passed, 0 failed).
RSS stable at ~4 GB peak during inference (vs 42 GB with ort CoreML).
Network integration tests pass (model downloads, embedding + search work).

**candle Metal path:** FAILED. `no metal implementation for layer-norm`
on first `model.forward()` call. candle 0.10.x Metal backend does not
implement LayerNorm as a GPU kernel. This affects ALL transformer models,
not just NomicBERT. Metal is unusable for our use case.

**Deps indexing diagnostic:** Pipeline works correctly (no crash/hang).
354 packages → 206,921 pub chunks → 3,234 batches. CPU embedding speed
~27s/batch (release) → ~24 hours total. Impractical without GPU.

**First forward RSS spike:** +4.9 GB on first candle inference call
(gemm workspace allocation). Stabilizes after that. Acceptable.

**Conclusion:** candle migration successful for CPU stability and
crates.io publishability. GPU acceleration requires a different backend.
burn + WGPU identified as the viable path — see epic
`260405-epic-gpu-embedding-acceleration`.

### Phase 4 — Cleanup [deferred]

- Remove RSS instrumentation from embedder.rs
- Remove `CARGO_DEBRIEF_NO_DAEMON` env var check (or keep if useful)
- Deferred until burn+WGPU migration is complete (diagnostic code useful
  for validating the next backend too)

## Open Questions [resolved]

- ~~Sharded vs single safetensors~~ → single `model.safetensors` (~270 MB
  for nomic, ~1.34 GB for bge)
- ~~candle Metal stability~~ → Metal backend missing LayerNorm. Unusable
  for transformers. burn+WGPU is the replacement path.
- candle CPU produces different float values than ort → INDEX_VERSION
  bumped to 5. Search quality TBD (network tests pass, full eval pending).
