# Store — Mental Model

## Entry Points

- `src/store.rs` — `IndexData`, `save_index`, `load_index`, `DepsIndexData`, `save_deps_index`, `load_deps_index`

## Module Contracts

- `load_index` returns `Ok(None)` for both **missing file** and **version mismatch**. Callers cannot distinguish the two cases. Both are treated as "start fresh."
- `save_index` creates all parent directories automatically (`create_dir_all`). Callers do not need to pre-create the index directory.
- `IndexData::version` is always `INDEX_VERSION` (currently `5`) on write; on read, any mismatch silently discards the file.
- `IndexData::chunks` maps `PathBuf` → `Vec<Chunk>`. The key is the file path as provided by the caller — no normalization occurs inside `store`.
- `load_deps_index` returns `Ok(None)` for missing file, deserialization failure, or `DEPS_INDEX_VERSION` (currently `2`) mismatch — same semantics as `load_index`. Callers treat `None` as "stale; reindex."
- `save_deps_index` also calls `create_dir_all` on the parent directory.

## Coupling

- **Bincode field order is the serialization format.** The `version: u32` field must remain the first field in both `IndexData` and `DepsIndexData`; the version-mismatch tests patch bytes at offset `0..4`. Reordering fields breaks the format silently.
- **`Chunk` struct changes require bumping both version constants.** Because bincode is not self-describing, any addition, removal, or reordering of fields in `chunk::Chunk` or `ChunkMetadata` produces silent deserialization corruption on both indexes. Bump `INDEX_VERSION` **and** `DEPS_INDEX_VERSION` when changing those types.
- `DepsIndexData` stores chunks with `ChunkOrigin::Dependency` (set by `service.rs`). The `chunk::ChunkOrigin` enum is part of the on-disk format for both index files.

## Extension Points & Change Recipes

**Adding a field to `IndexData` or `DepsIndexData`:**

1. Add the field to the struct.
2. Bump the corresponding version constant (`INDEX_VERSION` or `DEPS_INDEX_VERSION`).
3. Old indexes are silently discarded and reindexed on next run.

## Common Mistakes

- **Not bumping both version constants after changing `Chunk` fields** — `IndexData` and `DepsIndexData` share the same `Chunk` type. Bumping only `INDEX_VERSION` leaves `deps-index.bin` silently corrupt.
- **Treating `Ok(None)` as an error** — callers must handle `None` as the normal "no valid index" case and proceed with full reindexing.
