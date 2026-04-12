---
title: "Unify on burn backend — remove candle, wgpu as default"
related:
  260404-fix-gpu-acceleration: candle migration that proved Metal unusable
  260405-epic-gpu-embedding-acceleration: "burn NomicBERT implementation (child 1 done)"
started: 2026-04-09
---

# Unify on burn backend — remove candle, wgpu as default

## Goal

Remove the candle embedding backend entirely and consolidate on burn as the
sole inference backend. Make the `wgpu` feature a default so GPU acceleration
is available out of the box.

## Background

The project accumulated three embedding backends through a series of
migrations, each attempting to solve GPU acceleration:

| Backend | Outcome |
|---------|---------|
| ort + CoreML | 40 GB RSS explosion on first inference (upstream bug) |
| candle + Metal | `no metal implementation for layer-norm` — unusable |
| candle CPU | Works, but 206K dep chunks take ~24h — impractical |
| **burn + WGPU** | **844 MB RSS, correct output, 5.73s including model load** |

burn+WGPU is the first backend that actually delivers GPU acceleration.
candle and its broken feature flags (`metal`, `cuda`) are now dead weight.

## Significance

- **Single code path.** Two parallel inference paths (candle + burn) collapse
  to one. Eliminates `EmbedderModel` enum dispatch, `ModelKind` enum,
  candle device selection logic.
- **Dependency cleanup.** Removes `candle-core`, `candle-nn`,
  `candle-transformers` — three crates with no working GPU path.
  Removes broken `metal`/`cuda` feature flags.
- **GPU by default.** `wgpu` becomes a default feature. Users get Metal
  (macOS), Vulkan (Linux), DX12 (Windows) without knowing about feature
  flags. CPU fallback via `--no-default-features` (NdArray backend).
- **Unblocks dep indexing.** Practical dependency indexing (the feature that
  makes the tool production-useful) requires GPU — now available by default.
- **Config decoupling.** `NomicBertConfig` defined locally instead of
  importing from candle-transformers. No external dependency for model
  config parsing.

## Scope

### Phase 1: Backend consolidation [wip]

- **Cargo.toml:** Remove candle-\* deps, remove `metal`/`cuda` features,
  add `default = ["wgpu"]`
- **embedder.rs:** Remove candle imports, `ModelKind` enum, `EmbedderModel`
  enum, candle device selection, candle inference path, bge-large-en-v1.5
  model entry, RSS diagnostic code (`log_rss`, `EMBED_CALL_COUNT`). Rename
  `nomic-embed-text-v1.5-burn` → `nomic-embed-text-v1.5`.
- **nomic_bert_burn.rs:** Replace `candle_transformers::models::nomic_bert::Config`
  import with local `NomicBertConfig` struct (~10 fields, `serde::Deserialize`).
- **Tests:** Update model name references, remove bge test.

### Phase 2: Validation

- `cargo build` (default features = wgpu)
- `cargo build --no-default-features` (NdArray CPU)
- `cargo test --lib` + `cargo test --test integration`
- Network integration test with real model

## Dropped models

- **bge-large-en-v1.5:** candle-only (standard BERT architecture). No burn
  implementation exists. Dropped with candle. Can be re-added later if needed
  (burn BertModel implementation, epic child ticket 2).

## Post-merge finding: cubek-matmul panic → resolved via pre-release

**burn 0.20.1 / cubek-matmul 0.1.1** panicked on real workloads due to
40KB threadgroup shared memory selection exceeding Apple Silicon's 32KB
limit. The panic is weight-matrix-driven (768/3072 dims), not batch-driven
— even batch=1 panics.

**Resolution:** Upgraded to burn 0.21.0-pre.3 / cubek-matmul 0.2.0-pre.3
(pre-release). No API breaks, compiles clean. GPU indexing works on full
ripgrep: 18,874 chunks in 17m21s (3.4x faster per-chunk than ort CPU baseline).

See `260409-feat-gpu-performance-tuning` for benchmark details and
throughput optimization agenda.

## Open Questions

- Runtime fallback: if wgpu feature is on but no GPU adapter found at runtime,
  does burn-wgpu degrade gracefully or error? Needs testing on headless/CI
  environments.
