---
title: "On-Demand Dependency Indexing — --dep flag with per-dep cached indexes"
category: feat
priority: high
related:
  - 260404-feat-dependency-chunking    # existing all-deps approach (to be superseded)
  - 260405-feat-cpp-chunker            # C++ chunker needed for C++ dep indexing
  - 260405-feat-cpp-deps-discovery     # C++ dep discovery (pivoted to on-demand)
---

# On-Demand Dependency Indexing

## Goal

Replace the all-transitive-deps pre-indexing approach with on-demand,
per-dependency indexing via a `--dep` CLI flag. Indexes are cached per
workspace and namespace-separated — deps only appear in search results
when explicitly requested.

## Motivation

The existing deps pipeline (`run_deps_index`) pre-indexes all transitive
dependencies: 354 packages → 206,921 chunks → ~24h CPU. This is
impractical and architecturally wrong:

- **Scale mismatch**: GPU acceleration needed just to index deps
- **Discovery complexity**: requires cargo metadata BFS, and for C++
  would require build system parsing (vcxproj, CMake, #include resolution)
- **Noise**: most deps are irrelevant to a given query
- **LLM alternative**: an LLM agent can explore deps directly — reading
  source files with intent understanding that vector search lacks

On-demand indexing flips the model: user points to a specific dep, it gets
indexed once and cached, searched only when explicitly requested.

## Rejected Alternative

**Full transitive pre-indexing** (current `deps-index.bin` approach):
- 206K chunks for a single project — impractical without GPU
- Build system parsing for C++ (vcxproj/CMake) adds massive complexity
- Most deps never get queried

**LLM-only dep exploration** (no RAG for deps at all):
- Viable for small deps, but a large framework (ASIO, Boost, tokio) has
  thousands of files — exceeds LLM context even with haiku
- On-demand RAG is the sweet spot: index just the dep you need, when
  you need it

## Design Decisions

### CLI interface

```bash
# Rust: resolve by crate name from Cargo.lock
debrief --dep tokio search "spawn"

# Any language: explicit path
debrief --dep /path/to/asio/include search "async_read"

# Multiple deps
debrief --dep tokio --dep hyper search "Connection"

# Without --dep: project index only (unchanged behavior)
debrief search "my_function"
```

### Namespace separation

`--dep` creates a strict namespace boundary:
- `debrief search "query"` → project index only
- `debrief --dep X search "query"` → dep X index only
- `debrief --dep X --dep Y search "query"` → union of X and Y dep indexes

Project results and dep results are never mixed in a single search unless
both namespaces are explicitly requested. This prevents dep chunks from
diluting project results (the `DEP_ORIGIN_PENALTY` problem disappears).

### Cache structure

```
.debrief/
  index.bin              # project index (unchanged)
  deps-index.bin         # legacy all-deps index (deprecated, to be removed)
  deps/
    <dep-key>.bin        # per-dep cached index
    <dep-key>.meta       # metadata: source path, timestamp, chunk count
```

**Dep key derivation:**
- Rust crate from Cargo.lock: `<name>-<version>` (e.g., `tokio-1.38.0`)
  — immutable, never stale
- Explicit path: `path-<sha256-first-16>` with the original path stored
  in `.meta` for display

**Staleness check:**
- Versioned Rust crates (from registry): always fresh — version in name
  guarantees immutability
- Path deps / git deps: store newest file mtime in `.meta` header;
  re-index if any file in the dep directory is newer
- Manual override: `debrief --dep X --reindex search "query"` forces
  re-index

### Dep resolution (Rust-specific)

When `--dep <name>` is given without a path:
1. Look for `Cargo.lock` in workspace
2. Find the package entry matching `<name>`
3. Resolve to `~/.cargo/registry/src/<name>-<version>/`
4. If not found in registry, check `[patch]` and path dependencies
5. Fall back to error with suggestion to use explicit path

This is much simpler than the current `deps.rs` BFS — it's a single
lookup in Cargo.lock, not a full dependency graph traversal.

### Indexing scope within a dep

- **Rust**: pub items only (existing behavior from `run_deps_index`)
- **C++**: headers only by default (`.h`, `.hpp`, `.hxx`), configurable
  to include source files
- **Embedding text annotation**: `[dep: <name>]` prefix in embedding text
  (existing convention, keep it)

### Index format

Reuse existing `IndexData` serialization (bincode + versioned header).
Each per-dep `.bin` file is a standalone `IndexData` with its own chunk
list and embeddings. At search time, a `SearchIndex` is built from the
requested dep's cached `IndexData`.

## Phases

### Phase 1: CLI + cache infrastructure

- Add `--dep <path-or-name>` CLI argument (repeatable)
- Dep key derivation (name-version for Rust, path-hash for explicit)
- Cache directory management (`.debrief/deps/`)
- `.meta` file format (source path, mtime, chunk count, index version)
- Staleness check logic
- Load cached index or trigger re-index
- Unit tests for key derivation, staleness logic

### Phase 2: On-demand indexing pipeline

- Single-dep chunking: walk dep directory, apply appropriate chunker
  (RustChunker for .rs, CppChunker for .cpp/.h when available)
- Pub-only filtering for Rust deps (reuse existing logic)
- Embedding + serialization to `.debrief/deps/<key>.bin`
- Progress indicator for first-time indexing
- Integration test: index a small crate, verify cache, search

### Phase 3: Rust crate name resolution

- Parse `Cargo.lock` to resolve crate name → registry source path
- Handle path dependencies and git dependencies
- `--dep tokio` resolves automatically
- Error messages with suggestions when resolution fails

## Migration

The existing `deps-index.bin` (all-deps index) can coexist during
migration. Eventually:
- Remove `run_deps_index` pipeline from `service.rs`
- Remove `DepsIndexData` from `store.rs`
- Remove `--no-deps` flag (no longer needed — deps are opt-in)
- Deprecate then remove `DEP_ORIGIN_PENALTY` from search scoring
