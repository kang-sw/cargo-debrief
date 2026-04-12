---
domain: embedder
description: "Text embedding inference: ModelRegistry, Embedder, two mutually-exclusive backends (wgpu/burn and ort-cpu/ONNX)"
sources:
  - src/
related:
  service: "build_embedder is the sole Embedder construction point; called before every index or search"
  search: "Embedding dimension from Embedder must match vectors stored in the index at search time"
---

# Embedder — Mental Model

## Entry Points

- `src/embedder.rs` — `ModelRegistry`, `ModelSpec`, `ModelKind`, `Embedder`

## Module Contracts

- `ModelRegistry::lookup` returns `None` for any name not in the hardcoded `KNOWN_MODELS` slice. There is no runtime registration; adding a model requires editing the slice in source.
- **Two valid build configurations exist, enforced at compile time:**
  - `--features wgpu` (default) — activates `burn`/`burn-store` deps and the burn-based `Embedder`.
  - `--no-default-features --features ort-cpu` — activates `ort` dep and the ort-cpu `Embedder` (ONNX Runtime, CPU EP).
  - Building with both features simultaneously, or with neither, triggers a `compile_error!` in `src/lib.rs` with a distinct message for each case. There is no runtime check — the error is caught at compile time.
- **`Embedder` is not a single struct.** Two `#[cfg]`-gated definitions exist in `src/embedder.rs`:
  - `#[cfg(feature = "wgpu")]` — full burn+WGPU inference path with `BurnNomicBertModel<Wgpu>` and a `Mutex`.
  - `#[cfg(feature = "ort-cpu")]` — full ONNX Runtime inference path: `Session` (CPU EP, `GraphOptimizationLevel::Level3`), `Tokenizer`, and `max_length`. Produces identical-shape output (L2-normalized, 768-dim) to the `wgpu` path.
- `ensure_model_files` downloads missing model files from HuggingFace to a backend-specific subdir: `<cache_dir>/<model_name>/burn/` (wgpu) or `<cache_dir>/<model_name>/ort/` (ort-cpu). The old flat layout (`<cache_dir>/<model_name>/`) is orphaned — files there will not be found or cleaned up automatically. Partial downloads leave `*.tmp` files; completion atomically renames them. A concurrent download that completes first causes the other to discard its `.tmp` copy silently.
- `embed_batch` (both backends) always returns L2-normalized vectors (unit length). Callers that apply their own normalization will double-normalize — silently producing the same unit vector but wasting work.
- `embed_batch` (both backends) truncates inputs exceeding `ModelSpec::max_length` tokens using `TruncationStrategy::LongestFirst`. No error is raised; long inputs are silently shortened.
- **ort-cpu `Session::builder()` steps use `.map_err(|e| anyhow::anyhow!("{e}"))` instead of `.context()`** — `ort::Error<SessionBuilder>` in rc.12 is not `Send+Sync`, which makes it incompatible with `anyhow` context chaining. Forgetting this and using `.context()` on those builder steps produces a compile error (`the trait Send is not implemented`).
- **nomic-embed-text-v1.5 requires three ONNX inputs**: `input_ids`, `attention_mask`, `token_type_ids`. The ort-cpu `embed_batch` sends `token_type_ids` as an all-zeros tensor (BERT single-segment convention). Omitting it causes the ONNX session run to fail at runtime.
- ort-cpu throughput on the ripgrep corpus: ~5.5 chunks/s (9 min 3 sec, 2,980 chunks).
- The wgpu `Embedder` wraps `BurnNomicBertModel<ActiveBackend>` in a `Mutex`. Inference is serialized — concurrent calls block on the mutex.
- The `wgpu` path device selection is **compile-time only**: `ActiveBackend = burn::backend::Wgpu`. `get_burn_device()` calls `Default::default()` — WGPU handles device discovery silently with no runtime warning if no GPU is available.
- `token_ids_to_burn_tensor` widens `u32` token IDs to `i64` before building the burn `Int` tensor. This is an implicit contract with the Wgpu backend.

## Coupling

- `service.rs` imports `ModelRegistry` directly to validate model names in `set_embedding_model`. Any new model added to `KNOWN_MODELS` is immediately accepted by the service without additional changes.
- `search.rs` consumes `Embedder` by reference in `SearchIndex::search`. The embedding dimension embedded in the `Chunk` must match the dimension the `Embedder` produces; a mismatch causes an HNSW cosine distance computation on mismatched-length slices (runtime panic in hnsw_rs, not a graceful error).
- The model cache path is `dirs::data_dir()/debrief/models/` by convention (used in tests and expected by callers). No function enforces this path — callers must construct it themselves.
- `src/nomic_bert_burn.rs` is gated behind `#[cfg(feature = "wgpu")]` in `src/lib.rs`. It is not compiled or accessible on the `ort-cpu` path.
- The wgpu burn path token tensors use `i64` internally (widened from `u32` by `token_ids_to_burn_tensor`). The caller provides `u32` from the tokenizer.

## Extension Points & Change Recipes

**Adding a new model architecture (wgpu burn path):**

1. Implement the model in `src/nomic_bert_burn.rs` (or a new file) using `#[derive(Module)]`.
2. Add a variant to `ModelKind` and a corresponding load arm in `Embedder::load`.
3. Add a separate inference block in `embed_batch`.
4. Add an entry to `KNOWN_MODELS`.

If the new architecture does not produce standard last-hidden-state output compatible with `burn_mean_pooling` (shape `[batch, seq, hidden]`), you must add a separate pooling path — using the existing pooling functions on incompatible shapes will silently produce incorrect vectors.

## Common Mistakes

- **Passing absolute `cache_dir` and expecting cross-platform portability** — `dirs::data_dir()` returns `None` in stripped environments (CI without a home dir). Callers must handle the `None` case or the `unwrap` panics before `Embedder::load` is even called.
- **Embedding chunks with one `Embedder`, searching with another of different dimension** — produces a silent dimension mismatch at HNSW search time (runtime panic), not at index build time.
- **Treating `embed_batch(&[])` as an error** — it returns `Ok(vec![])` for an empty input slice. Callers expecting at least one result must guard on empty input themselves.
- **Building with both `wgpu` and `ort-cpu` features enabled** — the `compile_error!` in `src/lib.rs` fires immediately; there is no fallback or silent behavior.
- **Building with no feature flags** — `--no-default-features` without specifying either `wgpu` or `ort-cpu` also triggers a `compile_error!`. The default feature set is `wgpu`.
- **Expecting `rotary_emb_fraction < 1.0` to work with `BurnNomicBert`** — the burn attention implementation asserts `rotary_emb_dim == head_dim` at load time. Any config where `rotary_emb_fraction != 1.0` causes a panic during `Embedder::load`, not at inference time.
