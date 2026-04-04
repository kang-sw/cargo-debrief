---
title: "Dependency Chunking — Index Transitive Dependencies (Rust)"
category: feat
priority: high
parent: null
plans: null
related:
  - 260403-research-rag-architecture  # vector search + metadata boosting baseline
  - 260404-feat-rust-chunking-population  # chunker quality affects dep indexing
  - 260404-feat-daemon-mode  # daemon can cache dep indexes across sessions
---

# Dependency Chunking — Index Transitive Dependencies (Rust)

## Goal

Index public API surfaces of crate dependencies so that AI agents can
search for dependency types, traits, and functions alongside project code.
Rust-first implementation; the pattern generalizes to C++ (headers/submodules)
and Python (site-packages) later.

## Key Design Decisions

### Scope: all transitive deps, public API only

Indexing all transitive dependencies avoids the need to resolve `pub use`
re-export chains. Facade crates like `bevy` re-export from deep transitive
deps (`bevy_ecs`, `bevy_reflect`, etc.) — if all transitive deps are
indexed, vector search naturally matches regardless of re-export path.

Public API filtering uses the existing `Visibility` field on `Chunk`.
Only `pub` items are indexed from dependencies. This keeps scale
manageable while covering the useful surface.

Scale estimates (public API only):

| Project size | Transitive deps | Est. chunks | Embedding time (CPU) |
|-------------|-----------------|-------------|---------------------|
| Small (~30 deps) | ~30 | ~1,500 | ~8s |
| Medium (~60 deps) | ~60 | ~3,000 | ~15s |
| Large (~200 deps) | ~200 | ~10,000 | ~50s |

One-time cost; re-index only when `Cargo.lock` changes.

### Source discovery: `cargo metadata`

`cargo metadata --format-version 1` provides resolved dependency paths
(registry sources, git checkouts, path deps). Each package entry includes
`manifest_path`, from which `src/` is derived.

### Embedding text enrichment: root dependency annotation

For each transitive dep, compute which of the user's direct dependencies
it is reachable from (BFS on the dependency graph). Prepend a `[dependency]`
tag line to `embedding_text`:

```
[dependency] bevy_ecs (dependency of: bevy)
// bevy_ecs::query (src/query/mod.rs:15-42)
pub struct Query<Q, F = ()> { ... }
```

The `[dependency]` tag creates a clear signal in the embedding vector
itself, complementing origin-based score boosting. This bridges the
vocabulary gap — a query mentioning "bevy" matches chunks in `bevy_ecs`
because "bevy" appears in the embedding text.

### Staleness: `Cargo.lock` content hash

Store a hash of `Cargo.lock` contents alongside the dependency index.
If the hash matches, the dep index is fresh. If not, re-index all deps.

Future optimization: per-package version tracking to re-index only
changed/added packages. Not needed for MVP of this feature.

### Storage: separate index file

Dependency index stored in `.git/debrief/deps-index.bin` (or equivalent),
separate from the project index. Search merges both at query time.

### Search: unified query with project boost

Search queries hit both project and dependency indexes. Project chunks
receive a score boost (or dependency chunks receive a penalty) to prevent
dependency results from crowding out project code.

The existing metadata boosting mechanism in `search.rs` extends to
support origin-based boosting.

CLI flag: `--no-deps` to disable (default: ON — dependency search is included unless explicitly disabled).

### Chunk model: `ChunkOrigin` enum

Add to `Chunk`:

```rust
pub enum ChunkOrigin {
    Project,
    Dependency {
        crate_name: String,
        crate_version: String,
        root_deps: Vec<String>,  // direct deps this is reachable from
    },
}
```

Populated at service level after chunking — the `Chunker` trait does not
change. Origin context is injected when assembling chunks in the indexing
pipeline.

### Chunker trait: unchanged

`Chunker::chunk(file_path, source) -> Vec<Chunk>` remains pure text
processing. The chunker does not need to know whether it is processing
project or dependency code. Origin metadata is attached by the caller.

### Git submodules

Treated as dependencies (not project source). Details deferred to C++
chunker stage where submodule-as-dependency is the primary pattern.
Current `git ls-files` does not recurse into submodules, so they are
already excluded from project indexing.

## GPU Acceleration Note

GPU acceleration for the ONNX embedder is tracked in a separate ticket
(`260404-fix-gpu-acceleration`). The CPU path is sufficient for initial
implementation — the scale estimates in this ticket (8–50s) are based
on CPU. GPU will naturally reduce those times when the fix lands, but
does not block this feature.

## Phases

### Phase 1 — Chunk model + dependency discovery

- Add `ChunkOrigin` to `Chunk` data model
- Implement `cargo metadata` parsing for dependency source paths
- Implement dependency graph BFS for root-dep annotation
- Bump `INDEX_VERSION`

### Phase 2 — Dependency indexing pipeline

- Walk dependency source files, filter to `.rs`
- Chunk with `RustChunker`, filter to `pub` items
- Inject `ChunkOrigin::Dependency` + embedding text annotation
- Serialize to separate `deps-index.bin`
- Staleness check via `Cargo.lock` hash

### Phase 3 — Unified search + config

- Merge project + dependency indexes at search time
- Origin-based score boosting
- CLI `--no-deps` flag (default ON; flag disables dependency search)
- Config: dependency exclude list
- Update `overview` to optionally show dependency types

## Open Questions

- **RESOLVED** — `--include-deps` default: dependency search is ON by
  default; `--no-deps` disables it.
- Should `overview` work on dependency files? (Useful but path resolution
  is different — keep open for now.)
- Per-package version tracking for incremental dep re-indexing: future
  optimization; note in spec when implemented.

## Config

```toml
[dependencies]
# Exclude specific crates from dependency indexing
exclude = ["syn", "proc-macro2"]  # large, rarely searched directly

# Override: index only direct deps (disables transitive)
# transitive = false
```
