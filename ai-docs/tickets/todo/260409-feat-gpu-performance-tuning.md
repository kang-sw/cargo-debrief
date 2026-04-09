---
title: "GPU performance tuning — batch size, throughput optimization"
related:
  - 260409-refactor-burn-backend-unification  # burn+wgpu now sole backend
  - 260405-epic-gpu-embedding-acceleration    # parent effort
---

# GPU Performance Tuning

## Blocker: cubek-matmul shared memory panic (burn 0.20 / M3 Max)

**Benchmark on ripgrep (2026-04-09) revealed a fatal bug:**

```
thread 'main' panicked at cubek-matmul-0.1.1/src/launch/strategy.rs:437:22:
Unable to launch matmul because the config is invalid:
"This algorithm needs 40960 shared memory bytes but hardware limit is 32768."
```

burn+WGPU panics on real indexing workloads. The cubek-matmul kernel
selects a matmul algorithm requiring 40KB threadgroup shared memory,
but Apple M3 Max Metal has a 32KB per-threadgroup limit. The kernel
does not fall back to a smaller tile size.

**Why the earlier validation passed:** The integration test ran only 2
short text strings. The matmul algorithm selection differs for larger
batch sizes / longer sequences — the panic only triggers on real
workloads (ripgrep: 3070 chunks, 64-chunk batches).

### CPU fallback also degraded

burn NdArray (CPU, `--no-default-features`) timed out at >15 minutes
on ripgrep without completing. Old ort CPU baseline was 9m37s. burn
NdArray is a reference implementation, not optimized for inference.

### Benchmark results

| Metric | GPU (wgpu/Metal) | CPU (NdArray) | Old baseline (ort CPU) |
|--------|-----------------|---------------|----------------------|
| Wall time | PANIC (immediate) | >15 min (aborted) | 9m37s |
| Chunks | N/A | N/A | 3070 |
| Files | N/A | N/A | 100 |

### Resolution options

1. **Upgrade burn/cubek-matmul** — check if newer versions handle the
   shared memory fallback correctly
2. **Reduce matmul tile size** — configure cubek to use smaller tiles
   that fit in 32KB (if API allows)
3. **Reduce batch size or sequence length** — smaller inputs may avoid
   the large matmul config, but this is a workaround not a fix
4. **Restore candle CPU path** — as interim fallback until GPU is fixed
   (candle CPU was 9m37s on ripgrep, burn NdArray is >15min)
5. **Report upstream** — cubek-matmul should detect hardware limits and
   select a compatible algorithm

## Goal (blocked on above)

Optimize burn+WGPU embedding throughput. The current 64-chunk batch size
was chosen conservatively for CPU memory bounds during the ort era. With
GPU acceleration working, re-evaluate batch size and identify other
throughput bottlenecks.

## Agenda (deferred until blocker resolved)

1. **Batch size sweep.** Benchmark EMBED_BATCH_SIZE at 64, 128, 256, 512
   on ripgrep (3K chunks) and a larger dep corpus. Measure wall time and
   peak memory. Find the sweet spot where GPU compute saturates without
   OOM.

2. **First-batch warmup cost.** WGPU/CubeCL compiles shaders on first
   inference. Quantify the one-time cost and decide whether a dummy
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
