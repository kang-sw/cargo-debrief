# Embedder — Mental Model

## Entry Points

- `src/embedder.rs` — `ModelRegistry`, `ModelSpec`, `ModelKind`, `Embedder`

## Module Contracts

- `ModelRegistry::lookup` returns `None` for any name not in the hardcoded `KNOWN_MODELS` slice. There is no runtime registration; adding a model requires editing the slice in source.
- **Two valid build configurations exist, enforced at compile time:**
  - `--features wgpu` (default) — activates `burn`/`burn-store` deps and the burn-based `Embedder`.
  - `--no-default-features --features ort-cpu` — activates `ort` dep and the ort-cpu `Embedder` stub.
  - Building with both features simultaneously, or with neither, triggers a `compile_error!` in `src/lib.rs` with a distinct message for each case. There is no runtime check — the error is caught at compile time.
- **`Embedder` is not a single struct.** Two `#[cfg]`-gated definitions exist in `src/embedder.rs`:
  - `#[cfg(feature = "wgpu")]` — full burn+WGPU inference path with `BurnNomicBertModel<Wgpu>` and a `Mutex`.
  - `#[cfg(feature = "ort-cpu")]` — stub with matching public API (`load`, `embed_batch`, `embed`); all method bodies call `unimplemented!()`. This is a Phase 1 placeholder; Phase 2 will implement ONNX inference.
- The `wgpu` `Embedder::load` downloads missing model files from HuggingFace to `<cache_dir>/<model_name>/`. Three files are required: `model.safetensors`, `config.json`, `tokenizer.json`. Partial downloads leave `*.tmp` files; completion atomically renames them. Interrupting mid-download leaves an orphaned `.tmp` that is not cleaned up automatically.
- `embed_batch` (wgpu path) always returns L2-normalized vectors (unit length). Callers that apply their own normalization will double-normalize — silently producing the same unit vector but wasting work.
- `embed_batch` (wgpu path) truncates inputs exceeding `ModelSpec::max_length` tokens using `TruncationStrategy::LongestFirst`. No error is raised; long inputs are silently shortened.
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

**Implementing the ort-cpu Embedder (Phase 2):**

The `#[cfg(feature = "ort-cpu")]` `Embedder` block in `src/embedder.rs` is a stub. Phase 2 must implement `load()` (ONNX model loading with CPU execution provider only), `embed_batch()` (inference + mean pooling + L2 norm), and update `ensure_model_files()` with the ONNX download URL list and a `burn/` vs `ort/` subdir layout in the model cache.

## Common Mistakes

- **Passing absolute `cache_dir` and expecting cross-platform portability** — `dirs::data_dir()` returns `None` in stripped environments (CI without a home dir). Callers must handle the `None` case or the `unwrap` panics before `Embedder::load` is even called.
- **Embedding chunks with one `Embedder`, searching with another of different dimension** — produces a silent dimension mismatch at HNSW search time (runtime panic), not at index build time.
- **Treating `embed_batch(&[])` as an error** — it returns `Ok(vec![])` for an empty input slice. Callers expecting at least one result must guard on empty input themselves.
- **Building with both `wgpu` and `ort-cpu` features enabled** — the `compile_error!` in `src/lib.rs` fires immediately; there is no fallback or silent behavior.
- **Building with no feature flags** — `--no-default-features` without specifying either `wgpu` or `ort-cpu` also triggers a `compile_error!`. The default feature set is `wgpu`.
- **Calling `Embedder::load` or `embed_batch` on the ort-cpu build** — all method bodies are `unimplemented!()` in Phase 1. Any call panics at runtime.
- **Expecting `rotary_emb_fraction < 1.0` to work with `BurnNomicBert`** — the burn attention implementation asserts `rotary_emb_dim == head_dim` at load time. Any config where `rotary_emb_fraction != 1.0` causes a panic during `Embedder::load`, not at inference time.
