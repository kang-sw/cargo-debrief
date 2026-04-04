# Store — Mental Model

## Entry Points

- `src/store.rs` — `IndexData`, `save_index`, `load_index`

## Module Contracts

- `load_index` returns `Ok(None)` for both **missing file** and **version mismatch**. Callers cannot distinguish the two cases. Both are treated as "start fresh."
- `save_index` creates all parent directories automatically (`create_dir_all`). Callers do not need to pre-create the index directory.
- `IndexData::version` is always `INDEX_VERSION` (currently `1`) on write; on read, any mismatch silently discards the file.
- `IndexData::chunks` maps `PathBuf` → `Vec<Chunk>`. The key is the file path as provided by the caller — no normalization occurs inside `store`.

## Coupling

- **Bincode field order is the serialization format.** The `version: u32` field must remain the first field in `IndexData`; the version-mismatch test patches bytes at offset `0..4`. Reordering fields breaks the format silently.
- **`Chunk` struct changes require a version bump.** Because bincode is not self-describing, any addition, removal, or reordering of fields in `chunk::Chunk` or `ChunkMetadata` produces silent deserialization corruption on existing indexes. Always bump `INDEX_VERSION` when changing those types.

## Extension Points & Change Recipes

**Adding a field to `IndexData`:**

1. Add the field to the struct.
2. Bump `INDEX_VERSION`.
3. Old indexes are silently discarded and reindexed on next run.

## Common Mistakes

- **Not bumping `INDEX_VERSION` after changing `Chunk` fields** — existing indexes will deserialize with wrong field values or panic, with no error at the `load_index` call site (deserialization succeeds but data is corrupt).
- **Treating `Ok(None)` as an error** — callers must handle `None` as the normal "no valid index" case and proceed with full reindexing.
