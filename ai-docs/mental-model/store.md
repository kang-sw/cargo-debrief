---
domain: store
description: "Versioned bincode index serialization: IndexData, GitRepoState, DirtySnapshot, BackendTag"
sources:
  - src/
related:
  service: "load_or_rebuild_index is the sole reader/writer of IndexData"
  embedder: "BackendTag must match between the stored index and the current build configuration"
  git: "GitRepoState and DirtySnapshot track per-git-root incremental state"
---

# Store — Mental Model

## Entry Points

- `src/store.rs` — `IndexData`, `save_index`, `load_index`, `GitRepoState`, `DirtySnapshot`

## Module Contracts

- `load_index` returns `Ok(None)` for **missing file**, **deserialization failure**, **version mismatch**, or **backend mismatch**. All four are treated as "start fresh." Only I/O errors propagate as `Err`.
- `save_index` creates all parent directories automatically (`create_dir_all`). Callers do not need to pre-create the index directory.
- `IndexData::version` is always `INDEX_VERSION` (currently `8`) on write; on read, any mismatch silently discards the file.
- `IndexData::chunks` maps `PathBuf` → `Vec<Chunk>`. The key is the file path as provided by the caller — no normalization occurs inside `store`.
- `BackendTag` (`Wgpu` | `OrtCpu`) is serialized into `IndexData`. `current_backend()` is a feature-gated `const fn` — exactly one of the `wgpu` or `ort-cpu` features must be enabled at compile time. On load, a tag mismatch causes the index to be discarded so embeddings from different backends are never mixed in the same HNSW graph.

## Coupling

- **Bincode field order is the serialization format.** The `version: u32` field must remain the first field in `IndexData`; the version-mismatch tests patch bytes at offset `0..4`. Reordering fields breaks the format silently.
- **`Chunk` struct changes require bumping `INDEX_VERSION`.** Because bincode is not self-describing, any addition, removal, or reordering of fields in `chunk::Chunk` or `ChunkMetadata` produces silent deserialization corruption. Bump `INDEX_VERSION` when changing those types.

## Extension Points & Change Recipes

**Adding a field to `IndexData`:**

1. Add the field to the struct.
2. Bump `INDEX_VERSION`.
3. Old indexes are silently discarded and reindexed on next run.

## Common Mistakes

- **Not bumping `INDEX_VERSION` after changing `Chunk` fields** — bincode is not self-describing; field changes produce silent deserialization corruption without a version bump.
- **Treating `Ok(None)` as an error** — callers must handle `None` as the normal "no valid index" case and proceed with full reindexing.
