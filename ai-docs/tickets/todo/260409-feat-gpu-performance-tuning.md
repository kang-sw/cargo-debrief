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

### NdArray CPU fair benchmark (2026-04-11, source-only)

**Why the original result was misleading.** The ">15 min (aborted)" entry was
not a timed result — the process was killed by the macOS OOM reaper during a
source+deps run (~18,874 chunks total). No per-chunk throughput was ever
captured, and the scope did not match the ort baseline (3,070 source chunks
only). This section records a controlled source-only run that matches the ort
baseline scope.

**Method.** `cargo build --release --no-default-features` (NdArray backend,
no wgpu). Cold index (index.bin deleted). `CARGO_DEBRIEF_NO_DAEMON=1`.
`EMBED_BATCH_SIZE=64` (default, unchanged). Run terminated after 5 completed
batches; total wall time is extrapolated.

**Memory profile.** RSS cycles 3.0–4.4 GB per batch during NdArray inference.
This is qualitatively different from ort CPU, which never exhibited multi-GB
RSS on the same workload. The prior run was OOM-killed by macOS at ~4.4 GB
peak. Run 3 survived by surviving system pressure at the same peak — the
behavior is non-deterministic near the resident limit.

| Metric | NdArray CPU (M3 Max) | ort CPU baseline |
|--------|---------------------|-----------------|
| Batches measured | 5 of 48 | — |
| Wall time (measured) | 9m 59s (5 batches, 320 chunks) | 9m 37s (full run) |
| Wall time (extrapolated) | ~89 min (extrapolated from batches 2–5) | 9m 37s |
| Source chunks | 3,070 (target scope) | 3,070 |
| Per-chunk throughput | **0.58 chunks/s** (batches 2–5, stable) | **5.3 chunks/s** |
| Throughput ratio | ~9× slower than ort | baseline |
| Peak RSS | 3.0–4.4 GB (cycling) | not recorded, no issue observed |

**Batch 1 warmup note.** Batch 1 took ~161s vs ~110s for batches 2–5. The
extra ~51s is attributable to model/tokenizer initialization. Throughput
figure above uses batches 2–5 only to avoid warmup skew.

**Verdict.** NdArray CPU is unusable for ripgrep-scale workloads on M3 Max:
~9× slower than ort CPU at stable throughput, with extrapolated wall time of
~89 minutes vs 9m37s for ort. The high RSS ceiling (4.4 GB peak) also makes
it unreliable on memory-constrained hosts.

**Retention decision.** NdArray should be retained as the `--no-default-features`
build path. Slow CPU embedding is preferable to no embedding path for
environments without GPU support (CI, headless Linux servers, Windows without
Vulkan). The flag is not recommended for interactive use on large codebases.

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
