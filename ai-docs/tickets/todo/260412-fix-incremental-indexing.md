---
title: "Fix incremental indexing — git diff-based partial re-index instead of full rebuild"
category: bug
priority: high
related:
  - 260412-fix-external-source-file-discovery
---

# Fix Incremental Indexing

## Problem

`load_or_rebuild_index` uses `last_indexed_commit` only as a freshness
gate ("should we rebuild at all?"), not as an input to partial re-indexing.
Any HEAD change triggers a full re-index of all files across all sources.
On a codebase with external C++ repos registered (~1700 files), a single
doc commit causes a 7+ minute re-index.

This was a Key Design Decision from the project outset — "Incremental
re-indexing tracks git diff between last-indexed commit and HEAD. Never
re-index unchanged files." — but the actual merge logic was never
implemented. The infra stubs exist (`git::changed_files(from)`,
`last_indexed_commit` on IndexData) but are not wired together.

## Root Cause

`service.rs` `load_or_rebuild_index` always calls
`run_index_for_sources(project_root, &sources, embedder)` with no
`from` parameter. `run_index_for_sources` in turn always calls
`git::changed_files(project_root, None)` (full ls-files).
`IndexData.chunks` is a `HashMap<PathBuf, Vec<Chunk>>` — per-file
structure that already supports surgical updates — but is never patched.

## Fix

### Phase 1 — Incremental indexing for project-root sources

**Data flow change:**

```
load_or_rebuild_index
  if stale:
    from = existing.last_indexed_commit   // was: None always
    → run_index_for_sources(root, sources, embedder, from)

run_index_for_sources(…, from: Option<&str>)
  if from.is_some():
    changes = git::changed_files(root, from)  // diff only
    load existing IndexData from disk
    patch: remove chunks for changes.removed
    patch: re-chunk + re-embed changes.added ∪ changes.modified
    return patched IndexData
  else (force_full or no prior index):
    existing behavior (full walk)
```

**IndexData patching:**
`IndexData.chunks` is `HashMap<PathBuf, Vec<Chunk>>`. Surgical update:
```rust
// remove deleted files
for path in &changes.removed { data.chunks.remove(path); }
// re-index changed/added files
for path in changed_and_added {
    let chunks = chunk_file(path)?;
    let embedded = embed(chunks)?;
    data.chunks.insert(path, embedded);
}
```

**Rebuild the HNSW index** after patching — the in-memory search index
must be rebuilt from the updated `data.chunks`. This is already done
on every load; no extra work needed.

**`force_full` path** (`rebuild-index` CLI command) bypasses incremental
and always does a full rebuild, as today.

**External sources** (walkdir path): always do a full walk — they have
no git reference to diff against. This is consistent with the Phase 2
note in `260412-fix-external-source-file-discovery`.

### Phase 2 — Incremental for external git sources (deferred)

Per-source `last_indexed_commit` so external repos can diff their own
HEAD. Requires extending `SourceEntry` or `IndexData` with a
per-source commit map. Out of scope for Phase 1.

## Existing Infrastructure

- `git::changed_files(root, from: Option<&str>)` — already supports
  diff mode; returns `FileChanges { added, modified, removed }`.
- `IndexData.chunks: HashMap<PathBuf, Vec<Chunk>>` — per-file,
  supports surgical remove/insert.
- `last_indexed_commit: Option<String>` on `IndexData` — already
  stored and read.

The missing piece is wiring these together and loading the existing
IndexData before patching it (currently the load result is discarded
when the freshness check fails).

## Constraints

- `rebuild-index` CLI command must remain force-full (bypass incremental).
- External sources (walkdir) stay full-walk in Phase 1.
- HNSW index is always rebuilt from the full `data.chunks` map after
  patching — no attempt to incrementally update the ANN structure.
- `git::changed_files` `removed` list: verify it is populated for
  deleted files before relying on it. If not, treat missing files
  as "removed" during patching.
