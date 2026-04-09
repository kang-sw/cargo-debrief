---
title: "GPU performance tuning — batch size, throughput optimization"
related:
  - 260409-refactor-burn-backend-unification  # burn+wgpu now sole backend
  - 260405-epic-gpu-embedding-acceleration    # parent effort
---

# GPU Performance Tuning

## Resolved: cubek-matmul shared memory panic (burn 0.20 → 0.21-pre)

**Benchmark on ripgrep (2026-04-09) revealed a fatal bug on burn 0.20.1:**

```
thread 'main' panicked at cubek-matmul-0.1.1/src/launch/strategy.rs:437:22:
Unable to launch matmul because the config is invalid:
"This algorithm needs 40960 shared memory bytes but hardware limit is 32768."
```

cubek-matmul 0.1.1 selects a matmul tile requiring 40KB threadgroup
shared memory, exceeding Apple Silicon's 32KB limit. The requirement
is determined by NomicBERT's weight matrix dimensions (768/3072), NOT
by batch size — even batch=1 panics identically.

**Fix:** Upgrade to burn 0.21.0-pre.3 / cubek-matmul 0.2.0-pre.3.
Pre-release resolves the shared memory fallback. No API breaks.

### Benchmark (burn 0.21.0-pre.3 / M3 Max / batch=64)

| Metric | GPU (wgpu/Metal) | Old baseline (ort CPU) |
|--------|-----------------|----------------------|
| Wall time | 17m 21s (avg of 2 runs) | 9m 37s |
| Total chunks | 18,874 (1,796 source + 17,078 dep) | 3,070 (source only) |
| Per-chunk throughput | **18.1 chunks/s** | **5.3 chunks/s** |
| Files | 100 | 100 |

**Note:** The raw wall time comparison (17m vs 9m) is misleading —
the GPU run indexed 6.1x more chunks (including all dependency chunks
that were never indexed in the ort baseline). Per-chunk throughput is
~3.4x faster with GPU.

burn NdArray CPU fallback: >15 min (aborted). Not competitive.

## Goal

Optimize burn+WGPU embedding throughput further. Per-chunk throughput
is already 3.4x faster than ort CPU, but wall time on full
project+deps indexing may benefit from larger batches and async dispatch.

## Prerequisite: per-stage tracing

Before any optimization, the new service.rs (from `260409-epic-multi-language-sources`
glue layer rewrite) must emit per-stage timing via the `tracing` crate.
Pipeline spans:

```
rebuild_index
  ├─ file_discovery     (git ls-files, per source)
  ├─ chunking           (tree-sitter, per file)
  ├─ tokenization       (per batch)
  ├─ embedding          (GPU/CPU inference, per batch)
  └─ index_build        (HNSW construction)
```

Use `tracing::info_span!` + `#[instrument]` for automatic span timing.
`tracing-subscriber` with `fmt` layer for console output. This gives
structured timing data without manual `Instant::now()` boilerplate, and
extends naturally to daemon structured logging later.

Current benchmark (17m21s total) has no stage breakdown — optimization
without this data is guesswork.

## Agenda

1. **Batch size sweep.** Benchmark EMBED_BATCH_SIZE at 128, 256, 512,
   1024 on ripgrep. Measure wall time, peak memory, and per-stage
   breakdown from tracing output. Find the sweet spot where GPU compute
   saturates without OOM.

2. **First-batch warmup cost.** WGPU/CubeCL compiles shaders on first
   inference. Quantify via tracing spans and decide whether a dummy
   warmup batch is worth adding (relevant for daemon cold start).

3. **Sequence length padding.** Current tokenizer pads to batch-longest.
   Profile whether shorter average sequences (code chunks ~100-200 tokens
   vs max 512) leave GPU underutilized. Consider bucketed batching by
   length.

4. **Cross-platform validation.** macOS Metal verified. Test on Linux
   (Vulkan) and Windows (DX12) if environments available — at minimum
   confirm the binary builds and runs.

## Non-goals

- Runtime GPU/CPU backend selection (compile-time cfg is sufficient for now)
- Model architecture changes
- Alternative models beyond nomic-embed-text-v1.5
