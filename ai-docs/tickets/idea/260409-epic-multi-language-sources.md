---
title: "Multi-language source registration — config-driven indexing with C++ support"
category: epic
priority: high
related:
  - 260405-feat-cpp-chunker              # C++ chunker, absorbed into this epic
  - 260405-feat-cpp-deps-discovery       # C++ deps, absorbed (add cpp "path" --dep)
  - 260405-feat-on-demand-dep-indexing   # on-demand deps, absorbed into sources model
  - 260404-feat-dependency-chunking      # existing all-deps approach, to be superseded
---

# Multi-Language Source Registration

## Goal

Replace the Rust-hardcoded indexing pipeline with a config-driven model
where users (or AI agents) explicitly register source directories with
language and dep/project classification. `config.toml` becomes the single
source of truth for "what to index."

## Motivation

- The tool currently only indexes Rust via implicit Cargo.toml detection.
- C++ projects have no standard project definition — auto-discovery is
  unreliable. Explicit registration is the right model.
- AI agents will handle setup, so the UX cost of explicit registration
  is low.
- Unifies project source and dependency indexing under one model.
- Primary driver: user's production C++ codebase at work.

## Implementation Strategy: Glue Layer Rewrite

Blast radius analysis (session 260409) identified two distinct layers:

**Domain modules — keep as-is (~2,500 LOC):**

| Module | LOC | Notes |
|--------|-----|-------|
| embedder.rs + nomic_bert_burn.rs | ~870 | GPU validated, burn+WGPU |
| chunker/mod.rs + rust.rs | ~450 | Chunker trait is language-agnostic |
| search.rs | ~200 | HNSW + metadata boosting, language-agnostic |
| store.rs | ~150 | bincode serialization, generic |
| chunk.rs | ~100 | Data model |
| git.rs | ~100 | Git tracking |
| daemon.rs + ipc/ | ~650 | Complex, battle-tested, language-agnostic |

**Orchestration layer — rewrite from scratch (~600-700 new LOC):**

| Module | Current LOC | Action |
|--------|-------------|--------|
| service.rs | ~500 | Rewrite — config-driven pipeline |
| config.rs | ~150 | Rewrite — [[sources]] model |
| main.rs | ~100 | Rewrite — add/sources/remove commands |
| deps.rs | ~200 | Delete — cargo_metadata BFS not needed |

**Why rewrite over refactor:**
- service.rs is the Rust-hardcoding epicenter; incremental migration
  requires backward compat at every step — more complex than a clean write
- deps.rs + DepsIndexData + cargo_metadata are dead weight in the new
  model; easier to not include than to carefully remove
- config.rs needs [[sources]] from the ground up, not bolted onto
  existing 3-layer merge
- Domain modules have clean boundaries — no coupling to the glue layer
  that would force coordinated changes
- daemon.rs + ipc/ are NOT rewritten — too complex, already correct

## Architecture

### Config as ground truth

```toml
# .debrief/config.toml

[[sources]]
language = "rust"
root = "."

[[sources]]
language = "cpp"
root = "src/"

[[sources]]
language = "cpp"
root = "include/"

[[sources]]
language = "cpp"
root = "third_party/asio"
dep = true

[[sources]]
language = "cpp"
root = "third_party/fmt"
dep = true
```

Each source defines: language (chunker selection), root directory
(scope), and optional `dep = true` (dependency classification).

### CLI

```bash
cargo debrief add rust               # auto-generate from Cargo.toml: root = "."
cargo debrief add cpp "src/"         # explicit directory
cargo debrief add cpp "lib/" --dep   # dependency source
cargo debrief sources                # list registered sources
cargo debrief remove <index>         # remove a source entry
```

`add rust` is a convenience — parses Cargo.toml, writes `root = "."`
with `language = "rust"`. No magic beyond that.

### Indexing pipeline

```
rebuild-index / ensure_index_fresh
  → read config.toml
  → for each source:
      → git ls-files under root (filtered by language extensions)
      → git diff last_indexed_commit..HEAD for change detection
      → select chunker by language
      → chunk changed files
      → embed (burn+wgpu, batched)
  → dep sources: separate index namespace (not mixed with project results)
  → project sources: merged into single project index
```

File additions/deletions detected by git — no config re-run needed.
config.toml defines scopes (directories + language); git provides
the file-level delta on each operation.

### Dep sources

Sources with `dep = true`:
- Stored in separate per-dep index files (`.debrief/deps/<key>.bin`)
- Not mixed into project search results by default
- Searched when explicitly requested (TBD: `--dep` at search time, or
  always include with lower weight, or configurable)
- Staleness: for git-tracked deps, git diff works. For external paths
  (outside repo), fall back to file mtime check.

### Non-git dep directories

Dep roots outside the git repo (e.g., system headers, vcpkg installs)
need alternative change detection since git diff doesn't cover them.
Options: directory mtime scan, content hash cache, manual `--reindex`
flag. Design in detail during implementation.

## Phases

### Phase 1: Glue layer rewrite + config schema

Rewrite service.rs, config.rs, main.rs from scratch with config-driven
design. Delete deps.rs. CppChunker wired in from the start (Phase 2
is concurrent).

- config.rs: `[[sources]]` schema (language, root, dep, extensions),
  load/save, `add`/`remove` manipulation functions
- main.rs: `add`, `sources`, `remove` subcommands alongside existing
  `rebuild-index`, `search`, `overview`, `config`, `daemon`
- service.rs: config-driven `rebuild_index` / `ensure_index_fresh`,
  chunker dispatch by language, scoped `git ls-files` per source root
- `tracing` instrumentation: `info_span!` per pipeline stage
  (file_discovery, chunking, tokenization, embedding, index_build).
  `tracing-subscriber` `fmt` layer for console. Provides per-stage
  timing for GPU optimization work (`260409-feat-gpu-performance-tuning`).
- Delete deps.rs, remove `cargo_metadata` from Cargo.toml
- Backward compat: if no config.toml, auto-detect Cargo.toml and
  behave as current (equivalent to implicit `add rust`)

### Phase 2: CppChunker (concurrent with Phase 1)

- `src/chunker/cpp.rs` — tree-sitter-cpp, two-pass design
- File/class/struct skeletons, function body chunks
- Namespace-qualified symbol names
- Preprocessor directives in file skeleton
- ChunkKind extension for C++ node types
- Unit tests with representative C++ samples
- tree-sitter-cpp dependency in Cargo.toml

### Phase 3: Dep source indexing

- Separate index storage for dep sources (`.debrief/deps/<key>.bin`)
- Dep namespace in search (dep results separate from project results)
- Staleness for non-git paths (mtime or hash-based)
- Legacy cleanup complete (DepsIndexData, --no-deps, DEP_ORIGIN_PENALTY
  already gone from Phase 1 rewrite)

### Phase 4: Validation

- Index a Rust project (verify backward compat)
- Index a C++ project
- Index a mixed Rust+C++ project
- Index C++ project with header deps (ASIO, Boost)
- Search quality evaluation across languages
- User's production C++ codebase

## Absorbed tickets

- `260405-feat-cpp-chunker` → Phase 2
- `260405-feat-cpp-deps-discovery` → Phase 3 (just `add cpp "path" --dep`)
- `260405-feat-on-demand-dep-indexing` → Phase 1 + Phase 3

## Rejected approaches

- **Incremental refactoring of service.rs**: Phase 2 of original plan rated
  "high complexity" due to backward compat constraints at every step.
  Rewriting the ~600 LOC glue layer is simpler than migrating it.
- **Minimal C++ bolt-on** (just add extension dispatch to existing pipeline):
  Works short-term but leaves Rust-hardcoded architecture intact. Config
  model is the right design for multi-language; building it from scratch
  is cheaper than retrofitting.
- **Full project rewrite**: Unnecessary — domain modules (embedder, chunker,
  search, store, daemon, ipc) are clean and language-agnostic. Only the
  orchestration layer needs replacement.

## Open Questions

- Should `rebuild-index` without config.toml auto-run `add rust` if
  Cargo.toml is present? (backward compat vs explicit-only)
- Dep search UX: always include deps with lower weight? Require explicit
  flag? Configurable per-source?
- Should config.toml be `.debrief/config.toml` or project-root
  `.debrief.toml`? Former is consistent with existing `.debrief/` usage.
- INDEX_VERSION bump strategy: batch Phase 2 (ChunkKind C++ variants)
  and Phase 3 (ChunkOrigin changes) into a single bump?
