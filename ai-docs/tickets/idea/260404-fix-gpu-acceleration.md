---
title: "GPU Acceleration — Investigate and Fix CoreML Memory Bug"
category: fix
priority: medium
parent: null
plans: null
related:
  - 260404-feat-dependency-chunking  # GPU speeds up dep indexing; volume increase amplifies value
---

# GPU Acceleration — Investigate and Fix CoreML Memory Bug

## Goal

Investigate the CoreML execution provider's 41 GB RSS / context leak,
fix it, and enable GPU-accelerated embedding as default on macOS.
GPU acceleration reduces embedding time (theoretical 2.17x speedup),
which becomes a meaningful bottleneck once dependency chunking increases
index volume.

## Background

CoreML EP was registered in `Embedder::load` behind feature flags
(`gpu`, `cuda` in `Cargo.toml`) as part of the Phase 1 batch embedding
fix (commit b58d6f8). Execution provider priority: CoreML → CUDA → CPU.

CPU path verified stable: 3070 chunks, 100 files, 9m37s on ripgrep
(full repo, no crash).

CoreML path on the same workload: 41 GB RSS before SIGKILL at 4m25s.
`ort` emitted "Context leak detected" warnings during the run. The
CoreML EP appears not to release Neural Engine context between batch
calls. CPU-only path unaffected. The `gpu` feature flag carries a
warning in the README until this is resolved.

CUDA path exists for Linux/Windows but has not been tested; may have
similar lifecycle issues.

## Phases

### Phase 1 — Reproduce and Diagnose

Reproduce the CoreML RSS growth under controlled conditions:
- Minimal repro: single-session loop over N `embed_batch` calls,
  measure RSS after each iteration.
- Enable `RUST_LOG=debug` and `ORT_LOG_LEVEL=verbose` to surface
  session/context lifecycle messages.
- Profile with Instruments (macOS) to identify the allocation site.
- Determine whether the leak is in ort 2.0-rc's CoreML binding,
  in our `Session` object management, or in the CoreML/Neural Engine
  runtime itself.

### Phase 2 — Fix or Workaround

Depending on Phase 1 findings:

- **Session pooling**: Re-create the ort `Session` every N batches to
  force context release. Simple, low-risk; trades throughput for
  stability.
- **Batch isolation**: Spawn a subprocess per batch (heavy) or use
  `unsafe` session teardown APIs if ort exposes them.
- **ort version pin/upgrade**: If the leak is a known ort bug, pin to
  a fixed release or apply a source patch.
- **Fallback to Metal via `candle`**: If CoreML EP is unfixable in ort,
  evaluate `candle` (Hugging Face) with Metal backend as an alternative
  inference path.

Update `Embedder::load` and document the chosen approach.

### Phase 3 — Validate GPU Path on Real Workload

- Run `rebuild-index` on full ripgrep with GPU feature enabled.
- Verify RSS stays bounded (target: < 2 GB peak, no SIGKILL).
- Compare wall-clock time against CPU baseline (9m37s) to confirm
  speedup.
- Run search quality eval (same 24-query suite) to confirm no
  regression in embedding quality.
- Remove the GPU feature flag warning from the README.

## Open Questions

- Is the leak in ort's CoreML binding or in our session management?
  (Phase 1 will answer this.)
- Does the CUDA path have similar lifecycle issues? (Untested.)
- If CoreML remains unfixable, is `candle` + Metal a viable replacement
  for the ort ONNX pipeline, or does it require a new model format?
- Should GPU be enabled by default in release builds once stable, or
  remain opt-in via feature flag?
