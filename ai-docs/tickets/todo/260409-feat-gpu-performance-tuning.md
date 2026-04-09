---
title: "GPU performance tuning — batch size, throughput optimization"
related:
  - 260409-refactor-burn-backend-unification  # burn+wgpu now sole backend
  - 260405-epic-gpu-embedding-acceleration    # parent effort
---

# GPU Performance Tuning

## Goal

Optimize burn+WGPU embedding throughput. The current 64-chunk batch size
was chosen conservatively for CPU memory bounds during the ort era. With
GPU acceleration working, re-evaluate batch size and identify other
throughput bottlenecks.

## Agenda

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
