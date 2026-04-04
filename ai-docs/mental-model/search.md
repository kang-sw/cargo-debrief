# Search — Mental Model

## Entry Points

- `src/search.rs` — `SearchIndex`

## Module Contracts

- `SearchIndex::build` silently skips chunks with `embedding == None`. An index built from all-unembedded chunks is valid and returns empty results from all searches — no error is raised.
- `SearchIndex` holds a `'static`-lifetime `Hnsw` (via `hnsw_rs`). The index is built once from a `Vec<(PathBuf, Chunk)>` and is immutable afterward; incremental insertion is not supported.
- `search_by_vector` over-fetches `max(top_k * 2, top_k + 20)` ANN candidates (capped at index size) before applying symbol-name boosting and truncating to `top_k`. A chunk outside the raw top-k can surface after boosting, but only if it was within the over-fetch window.
- Score boosting is additive and unbounded above 1.0: exact symbol match adds `0.3`, partial match adds `0.1`. Callers must not assume scores are in `[0, 1]`.
- `search` is a thin wrapper over `search_by_vector` that calls `embedder.embed(query)` first. Any error from embedding propagates as-is.

## Coupling

- `SearchIndex` stores a parallel `Vec<(PathBuf, Chunk)>` indexed by the integer IDs returned by HNSW. The integer ID is an implicit contract with `hnsw_rs` — HNSW assigns IDs sequentially from the insertion order. If that assumption breaks (e.g., on a library version upgrade), results silently map to the wrong chunks.
- `SearchResult` is defined in `service.rs`, not `search.rs`. `search.rs` imports from `crate::service`. Any rename or restructuring of `SearchResult` fields requires updating `search.rs`.
- The embedding dimension used at index build time must exactly match the query vector dimension at search time. `DistCosine` in `hnsw_rs` does not validate dimension; a mismatch causes a panic inside the HNSW distance computation.

## Common Mistakes

- **Building the index before embedding chunks** — `SearchIndex::build` checks `chunk.embedding.is_some()` and silently drops chunks without embeddings. Calling `build` on chunks that have not yet been embedded produces an empty index with no error.
- **Assuming scores are capped at 1.0** — boosted scores exceed 1.0. Downstream rendering or threshold logic that clamps to `[0, 1]` silently misranks boosted results.
- **Calling `build` with mismatched embedding dimensions across chunks** — HNSW inserts all vectors regardless of length. The cosine distance function will compute incorrect distances or panic if dimensions differ between inserted vectors and the query.
