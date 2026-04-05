# Embedder — Mental Model

## Entry Points

- `src/embedder.rs` — `ModelRegistry`, `ModelSpec`, `ModelKind`, `EmbedderModel`, `Embedder`

## Module Contracts

- `ModelRegistry::lookup` returns `None` for any name not in the hardcoded `KNOWN_MODELS` slice. There is no runtime registration; adding a model requires editing the slice in source.
- `Embedder::load` downloads missing model files from HuggingFace to `<cache_dir>/<model_name>/`. Three files are required per model: `model.safetensors`, `config.json`, `tokenizer.json`. If a file already exists on disk, the download is skipped. A partial download leaves a `*.tmp` file; completion atomically renames it. Interrupting mid-download leaves an orphaned `.tmp` that is not cleaned up automatically.
- `Embedder::load` selects the compute device in feature-flag priority order: `metal` > `cuda` > CPU. `Device::new_metal(0)` and `Device::new_cuda(0)` both use `unwrap_or(Device::Cpu)` — if the accelerator is unavailable, execution falls back to CPU **silently** with no warning emitted.
- `embed_batch` always returns L2-normalized vectors (unit length). Callers that apply their own normalization will double-normalize — silently producing the same unit vector but wasting work.
- `embed_batch` truncates inputs exceeding `ModelSpec::max_length` tokens using `TruncationStrategy::LongestFirst`. No error is raised; long inputs are silently shortened.
- `Embedder` wraps `EmbedderModel` in a `Mutex`. Inference is serialized — concurrent calls block on the mutex.
- `mean_pooling` and `l2_normalize` are imported from `candle_transformers::models::nomic_bert` and applied to both `NomicBert` and `Bert` model outputs. They operate on generic `Tensor` values and are not architecture-specific.
- `EmbedderModel::Bert` forward pass requires a token-type-ids tensor; a zero tensor of the same shape as `input_ids` is synthesized internally. There is no `has_token_type_ids` field on `ModelSpec` — the dispatch is done purely via the `EmbedderModel` enum variant.

## Coupling

- `service.rs` imports `ModelRegistry` directly to validate model names in `set_embedding_model`. Any new model added to `KNOWN_MODELS` is immediately accepted by the service without additional changes.
- `search.rs` consumes `Embedder` by reference in `SearchIndex::search`. The embedding dimension embedded in the `Chunk` must match the dimension the `Embedder` produces; a mismatch causes an HNSW cosine distance computation on mismatched-length slices (runtime panic in hnsw_rs, not a graceful error).
- The model cache path is `dirs::data_dir()/debrief/models/` by convention (used in tests and expected by callers). No function enforces this path — callers must construct it themselves.
- Token tensors are `u32`, not `i64`. Any code that constructs `Tensor` values for direct injection into `embed_batch`-equivalent logic must use `u32`.

## Extension Points & Change Recipes

**Adding a new model architecture:**

1. Add a variant to `ModelKind` and `EmbedderModel`.
2. Add the forward-pass arm in `embed_batch` (the `match &*model` block).
3. Add the load arm in `Embedder::load` (the `match spec.model_kind` block).
4. Add an entry to `KNOWN_MODELS`.

If the new architecture does not produce standard last-hidden-state output compatible with `mean_pooling` (shape `[batch, seq, hidden]`), you must add a separate pooling path — using the nomic_bert `mean_pooling` on incompatible shapes will silently produce incorrect vectors.

**Adding `metal` or `cuda` feature support:**

Feature flags controlling device selection are `metal` and `cuda` (not `gpu`). The priority order in `Embedder::load` is: `#[cfg(feature = "metal")]` checked first, then `#[cfg(all(not(feature = "metal"), feature = "cuda"))]`, then CPU. Enabling both `metal` and `cuda` at once will silently use Metal only.

## Common Mistakes

- **Passing absolute `cache_dir` and expecting cross-platform portability** — `dirs::data_dir()` returns `None` in stripped environments (CI without a home dir). Callers must handle the `None` case or the `unwrap` panics before `Embedder::load` is even called.
- **Embedding chunks with one `Embedder`, searching with another of different dimension** — produces a silent dimension mismatch at HNSW search time (runtime panic), not at index build time.
- **Treating `embed_batch(&[])` as an error** — it returns `Ok(vec![])` for an empty input slice. Callers expecting at least one result must guard on empty input themselves.
- **Assuming GPU is active because the `metal` or `cuda` feature is enabled** — the `Device::new_metal(0).unwrap_or(Device::Cpu)` fallback is fully silent. There is no runtime indicator confirming which device is actually in use.
- **Using the old `gpu` feature flag** — the feature was renamed to `metal`. Code or CI config referencing `--features gpu` will compile CPU-only without error.
