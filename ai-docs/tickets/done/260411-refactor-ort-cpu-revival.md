---
title: "Reintroduce ort CPU backend as the non-GPU build path"
related:
  260409-refactor-burn-backend-unification: prior consolidation that removed ort/candle
  260404-fix-gpu-acceleration: "original ort → candle migration"
  260409-feat-gpu-performance-tuning: NdArray fair benchmark that drove this decision
plans:
  - ai-docs/plans/2026-04/11-1033.ort-cpu-phase2.md  # Phase 2: ort inference path
---

# Reintroduce ort CPU backend as the non-GPU build path

## Goal

Bring `ort` (ONNX Runtime) back as the **CPU-only build path**, replacing
burn's NdArray fallback entirely. The default GPU build (burn + WGPU) stays
unchanged. The two backends are **compile-time mutually exclusive** via
cargo features — no runtime selection, no dual dispatch overhead in either
binary.

End state:

- `cargo build` → burn + WGPU (GPU, default for desktop dev)
- `cargo build --no-default-features --features ort-cpu` → ort CPU only
  (CI, Docker, headless servers, GPU-less environments)
- NdArray code path **removed**.

## Background

After consolidating on burn in `260409-refactor-burn-backend-unification`,
the CPU fallback path used burn's NdArray backend. The burn-unification
ticket left an open question: how does this perform on real workloads?

The fair NdArray benchmark in `260409-feat-gpu-performance-tuning` answered
that question:

| Backend | Throughput | Wall time (3,070 chunks) | Peak RSS |
|---------|-----------|--------------------------|----------|
| ort CPU (historical baseline) | 5.3 chunks/s | 9m 37s | not problematic |
| burn NdArray CPU | **0.58 chunks/s** | **~89 min (extrapolated)** | **3.0–4.4 GB cycling, OOM-prone** |

NdArray is **~9× slower** than ort with a memory profile that OOM-killed
on the same workload. It is unusable for any practical CPU embedding scenario.

The natural question was whether to drop NdArray entirely. The risk: real
GPU-less environments (Docker without `--gpus`, GitHub Actions runners,
WSL2 without WSLg, headless servers without iGPU) cannot use the default
build. wgpu has a software-adapter fallback path (Lavapipe / llvmpipe), but:

1. It is not enabled or installed by default in most CPU-only environments
2. Even if available, shader-routed matmul on llvmpipe is unlikely to beat
   ort's hand-optimized oneDNN / AVX2-AVX512 CPU kernels
3. We have zero data on wgpu-software embedding throughput

So a real CPU path is needed, and NdArray is not it. ort is the only mature,
fast CPU embedding backend in the Rust ecosystem.

## Why ort is viable now

Verified against crates.io API (2026-04-11):

- All `ort = "2.0.0-rc.X"` versions (rc.0 through rc.12) are **not yanked**
- Latest non-yanked: `2.0.0-rc.12` (published 2026-03-05)
- All `1.16.x` and earlier versions are yanked
- No `2.0.0-stable` exists yet

The session note "All ort stable versions yanked" was technically true but
misleading — there is no stable 2.0.0 release, but rc versions are
publishable as cargo dependencies when pinned explicitly with `=`. crates.io
allows publishing crates that depend on pre-release versions.

The CoreML 40 GB RSS bug from `260404-fix-gpu-acceleration` was specific to
the **CoreML execution provider**, not the CPU EP. Using `ort` with CPU EP
only avoids this entirely.

## Decisions

### D1: Compile-time mutually exclusive features

Two cargo features, exactly one must be active:

```toml
[features]
default = ["wgpu"]
wgpu = [...]                  # current burn + WGPU stack
ort-cpu = ["dep:ort"]         # new path
```

Build script or `compile_error!` macro enforces "exactly one" — building
with both or neither fails at compile time.

Rationale: runtime backend selection was explicitly listed as a non-goal in
`260409-feat-gpu-performance-tuning`. Compile-time selection keeps each
binary single-purpose, avoids dual-backend code in both binaries, and lets
each environment build the right path.

### D2: NdArray removed entirely

The `#[cfg(not(feature = "wgpu"))] type ActiveBackend = burn::backend::NdArray;`
path in `embedder.rs` is deleted. `cargo build --no-default-features` (with
no features at all) will fail explicitly via the mutual-exclusion check —
users must pick `wgpu` or `ort-cpu`.

Rationale: NdArray is the worst of all worlds (9× slower than ort, OOM
profile, no GPU). Keeping it as a "third option" only confuses the build
matrix.

### D3: Backend tag in index header (option B)

Index files (`index.bin` and `deps-index.bin`) carry a backend identifier
in their header. On load, mismatch triggers staleness → forced re-index.

`INDEX_VERSION` 6 → 7 and `DEPS_INDEX_VERSION` 3 → 4.

Rationale (from session 2026-04-11 discussion):

- Embeddings produced by burn-wgpu and ort CPU may differ at the floating
  point level even with the same weights, due to kernel-level rounding,
  fused vs unfused ops, etc.
- The drift may be small enough that search quality is unaffected, but we
  have no data, and silently mixing embeddings of different provenance in
  the same HNSW index is a quality risk we should not take.
- Real users will not switch backends in practice (CPU users stay CPU,
  GPU users stay GPU), so the cost of forced re-index on backend change
  is theoretical, not operational.
- Strict isolation is the safe default; relaxation can come later if cross-
  backend equivalence is empirically verified.

### D4: Dual model file format with separate cache paths

ort uses ONNX, burn uses safetensors. The model cache layout:

```
<cache_dir>/
  nomic-embed-text-v1.5/
    burn/
      model.safetensors
      config.json
      tokenizer.json
    ort/
      model.onnx
      config.json
      tokenizer.json
```

`config.json` and `tokenizer.json` are identical between formats but
duplicated for self-containedness per backend.

Download logic is feature-gated: `wgpu` build downloads safetensors,
`ort-cpu` build downloads ONNX. The HuggingFace repo for nomic-embed-text-v1.5
hosts both formats already.

## Phases

### Phase 1: Cargo feature flags + ort dependency

- `Cargo.toml`: add `ort = "=2.0.0-rc.12"` with `download-binaries` feature,
  declare `ort-cpu` feature, gate `ort` dep on `ort-cpu`
- `compile_error!` guard: exactly one of `wgpu` / `ort-cpu` must be active
- No code changes to embedder yet — just dependency wiring
- Verify: `cargo build` (default = wgpu) and `cargo build --no-default-features --features ort-cpu` both succeed (the latter with placeholder embedder code that compiles but is unimplemented)

Success criteria: both feature configurations build cleanly. Mutual-exclusion
guard fires correctly when both or neither feature is set.

### Phase 2: ort inference path + dual model file format

Tightly coupled — split would force both halves to land together anyway.

- Restructure `embedder.rs` to feature-gate the model loading and inference
  paths: `#[cfg(feature = "wgpu")]` for the existing burn path,
  `#[cfg(feature = "ort-cpu")]` for the new ort path
- Implement `Embedder::load` and `embed_batch` for the ort path:
  - Load ONNX model with CPU EP only (no CoreML, no CUDA, no DirectML)
  - Run inference, mean pooling, L2 normalize — produce identical-shape
    output (`Vec<[f32; 768]>`) to the burn path
- The `Embedder` public API stays the same — both backends produce the
  same `Embedder` type signature, but only one is compiled per build
- Update model cache layout to `<cache>/<model-name>/<backend>/` so both
  backends can coexist on disk without collision (`burn/` subdir for
  safetensors, `ort/` subdir for ONNX)
- Feature-gate the download URL list: ONNX file for ort builds, safetensors
  for burn builds
- Existing top-level burn safetensors caches become orphaned (~250 MB).
  Acceptable — users can prune manually; add a migration utility later
  only if it matters.

Success criteria: `cargo build --no-default-features --features ort-cpu`
produces a working binary that downloads the ONNX model into the new cache
layout and produces embeddings of correct shape and approximate magnitude.
Both backends can coexist in the cache without collision.

### Phase 3: Index header backend tag + version bumps

- `store.rs`: extend `IndexData` and `DepsIndexData` headers with a
  `backend: BackendTag` field (string or enum: `"wgpu"` / `"ort-cpu"`)
- Bump `INDEX_VERSION` 6 → 7, `DEPS_INDEX_VERSION` 3 → 4
- On load: if header backend tag does not match the current build's
  backend, return `None` (treat as stale, trigger re-index)
- The version bump alone would invalidate old indexes, but the backend
  tag is needed for future cross-build invalidation (e.g., user has both
  builds on the same machine, switches between them)

Success criteria: existing indexes from version 6 are correctly recognized
as stale and re-built. A wgpu-built index is correctly invalidated when
loaded by an ort-cpu build, and vice versa.

### Phase 4: NdArray removal

- Remove `#[cfg(not(feature = "wgpu"))] type ActiveBackend = burn::backend::NdArray`
  from `embedder.rs`
- Remove any `burn::backend::NdArray` references throughout
- Remove NdArray from `Cargo.toml` if it's a separate feature/dep
- Update any tests or comments that reference the NdArray fallback
- Verify: `cargo build --no-default-features` (with no `ort-cpu` either)
  fails with the mutual-exclusion compile_error from Phase 1

Success criteria: NdArray code is fully gone. Only two valid build
configurations remain: default `wgpu` and explicit `ort-cpu`.

### Phase 5: Validation + smoke tests

- `cargo test --lib` on both feature configurations
- `cargo test --test integration` on both
- Smoke test on `test-repos/ripgrep`:
  - GPU build: rebuild-index + sample search, confirm results match prior
    behavior (sanity check, not regression test)
  - ort-cpu build: rebuild-index + sample search, confirm wall time is
    in the expected ~10 minute range (matching the historical 9m37s baseline)
  - Cross-build invalidation: run GPU rebuild, then run ort-cpu binary on
    the same project, confirm the wgpu-tagged index is invalidated and
    re-indexed
- Update `ai-docs/_index.md` to reflect the dual-build model and remove
  any NdArray references
- Update `ai-docs/mental-model/embedder.md` for the new structure

Success criteria: both builds work end-to-end, cross-build invalidation
works, smoke test passes for both paths.

## Non-goals

- **Runtime backend selection.** Compile-time only. Same rationale as the
  GPU perf tuning ticket — keeps each binary single-purpose.
- **GPU execution providers in ort.** The ort CPU path uses CPU EP only.
  CUDA, CoreML, DirectML, ROCm — none of them. The wgpu path is the
  GPU story; ort exists exclusively for CPU.
- **Reintroducing candle.** The ort revival is a CPU path only. burn-wgpu
  remains the GPU path. candle is not coming back.
- **Cross-backend embedding equivalence verification.** Not needed for
  this refactor — the backend tag means each index is single-provenance.
  If empirical verification later shows the embeddings are equivalent
  enough that cross-backend reuse is safe, that's a separate ticket to
  relax the strict isolation.
- **Migration of orphaned safetensors caches.** Users may have old top-
  level safetensors files in their model cache from before Phase 3.
  Leaving them as-is is fine — they take ~250 MB but cause no errors.
  A cleanup utility could be added later if it matters.

## Open questions

None. The decisions D1–D4 are settled per session 2026-04-11. Remaining
unknowns are implementation details that the plan/skeleton phases will
resolve.

## Notes on supersession

This ticket supersedes the NdArray retention recommendation in
`260409-feat-gpu-performance-tuning` ("Retention decision: NdArray should
be retained as the `--no-default-features` build path"). When this ticket
lands, that recommendation is void — NdArray is removed entirely and
ort-cpu replaces its role.
