# Embedder — Mental Model

## Entry Points

- `src/embedder.rs` — `ModelRegistry`, `ModelSpec`, `Embedder`

## Module Contracts

- `ModelRegistry::lookup` returns `None` for any name not in the hardcoded `KNOWN_MODELS` slice. There is no runtime registration; adding a model requires editing the slice in source.
- `Embedder::load` downloads missing model files from HuggingFace to `<cache_dir>/<model_name>/`. If the `.onnx` or `tokenizer.json` file already exists, download is skipped. A partial download leaves a `*.tmp` file; completion atomically renames it. Interrupting mid-download leaves an orphaned `.tmp` that is not cleaned up automatically.
- `Embedder::load` registers GPU execution providers before the session is created. On macOS with `--features gpu`, CoreML is attempted; on other platforms with `--features cuda`, CUDA is attempted. If provider registration fails for any reason (missing runtime, unsupported hardware), the builder is recovered via `unwrap_or_else(|e| e.recover())` and the session falls back to CPU **silently** — no warning is emitted.
- `embed_batch` always returns L2-normalized vectors (unit length). Callers that apply their own normalization will double-normalize — silently producing the same unit vector, but wasted work.
- `embed_batch` truncates inputs exceeding `ModelSpec::max_length` tokens using `TruncationStrategy::LongestFirst`. No error is raised; long inputs are silently shortened.
- `Embedder` wraps its ONNX `Session` in a `Mutex`. The `Session` is not `Send`; the mutex makes `Embedder` usable from async contexts but serializes all inference — concurrent calls block.

## Coupling

- `service.rs` imports `ModelRegistry` directly to validate model names in `set_embedding_model`. Any new model added to `KNOWN_MODELS` is immediately accepted by the service without additional changes.
- `search.rs` consumes `Embedder` by reference in `SearchIndex::search`. The embedding dimension embedded in the `Chunk` must match the dimension the `Embedder` produces; a mismatch causes an HNSW cosine distance computation on mismatched-length slices (runtime panic in hnsw_rs, not a graceful error).
- The model cache path is `dirs::data_dir()/debrief/models/` by convention (used in tests and expected by callers). No function enforces this path — callers must construct it themselves.

## Common Mistakes

- **Passing absolute `cache_dir` and expecting cross-platform portability** — `dirs::data_dir()` returns `None` in stripped environments (CI without a home dir). Callers must handle the `None` case or the `unwrap` panics before `Embedder::load` is even called.
- **Embedding chunks with one `Embedder`, searching with another of different dimension** — produces a silent dimension mismatch at HNSW search time (runtime panic), not at index build time.
- **Treating `embed_batch(&[])` as an error** — it returns `Ok(vec![])` for an empty input slice. Callers expecting at least one result must guard on empty input themselves.
- **Assuming GPU is active because the `gpu` feature is enabled** — the CoreML/CUDA provider registration failure is fully silent. There is no runtime indicator of which execution provider is actually in use. Profiling or ort's own logging is required to confirm GPU execution.
