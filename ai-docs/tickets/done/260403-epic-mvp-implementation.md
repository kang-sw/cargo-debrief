---
category: epic
priority: high
related:
  260404-refactor-service-trait-multi-workspace: "cross-cutting: multi-workspace trait shape"
---

# MVP Implementation — cargo-debrief

## Goal

Deliver a working CLI tool that indexes a Rust codebase and serves
vector search with metadata-boosted scoring. Phase 1 (in-process,
no daemon).

Spec: `ai-docs/spec/cargo-debrief.md`
Research: `260403-research-rag-architecture`

## Scope

### In scope (MVP)

- Project configuration (`.debrief/`, `.git/debrief/`)
- `DebriefService` trait + `InProcessService`
- Tree-sitter Rust chunking (type overview + function chunks)
- Dual text representation (display_text / embedding_text)
- Chunk metadata extraction
- Git-based incremental re-indexing
- ONNX embedding pipeline + model auto-download
- hnsw_rs vector search + metadata score boosting
- Versioned index persistence (`.git/debrief/index.bin`)
- CLI commands: `rebuild-index`, `search`, `overview`, `set-embedding-model`
- Implicit auto-indexing on `search` and `overview`

### Out of scope (deferred)

- Daemon mode (Phase 2)
- MCP server (Phase 3)
- Dependency indexing
- Multi-language support (C++, Python, etc.)
- Cross-file skeleton assembly (conservative same-file only for MVP)

## Implementation Phases

### Phase 1A — Scaffold & Infrastructure

1. **Project config & CLI skeleton**
   - clap CLI with subcommands (index, search, get-skeleton, set-embedding-model)
   - `.debrief/config.toml` and `.git/debrief/` path handling
   - Global config (`~/.config/debrief/`) path handling
   - Config resolution: local → project → global → default

2. **DebriefService trait + InProcessService**
   - Service boundary trait (index, search, get_skeleton)
   - InProcessService stub wired to CLI

> **Cross-cutting refactor (see `260404-refactor-service-trait-multi-workspace`):**
> After Phase 1A scaffolds the trait, each method must be updated to accept
> `project_root: &Path` explicitly. This makes the service multi-workspace-capable
> from the start — required before Phase 1C/1D wire up the full service dispatch.

### Phase 1B — Core Indexing Pipeline

3. **Git file tracking**
   - Detect changed files via `git diff --name-only`
   - Track `last_indexed_commit` hash
   - Handle added/modified/deleted files

4. **Tree-sitter Rust chunking**
   - `Chunker` trait (language-extensible interface)
   - Rust implementation: extract type overview + function chunks
   - Same-file `impl` block aggregation for skeletons
   - Dual text generation (display_text / embedding_text)
   - Chunk metadata extraction (symbol_name, kind, parent, etc.)

5. **Versioned index serialization**
   - Store module: serialize/deserialize chunks + metadata to `.git/debrief/`
   - Version field in header for format migration

### Phase 1C — Search Pipeline

6. **Embedding pipeline**
   - ONNX Runtime (`ort`) setup
   - Model auto-download + cache management
   - `set-embedding-model` command (project / global)
   - Tokenization + inference for chunk and query embedding

7. **Vector search**
   - hnsw_rs index build from chunk embeddings
   - Cosine similarity search
   - Metadata score boosting (symbol_name exact match)
   - Kind/visibility filtering

### Phase 1D — Integration & Polish

> **UX decisions (2026-04-04 discussion):**
>
> - **Implicit indexing.** `search` and `overview` auto-check index
>   freshness (compare `head_commit()` vs stored `last_indexed_commit`;
>   check model name match) and perform incremental re-indexing
>   transparently. First run triggers full index + model download.
> - **`index` → `rebuild-index` rename.** The explicit indexing command
>   becomes a manual recovery operation, deliberately named to discourage
>   routine use. Normal workflow never needs it.
> - **`get-skeleton` → `overview` rename.** The two commands serve
>   fundamentally different purposes — `search` performs vector-similarity
>   ranking, `overview` performs file-scoped overview-chunk lookup (no
>   query needed). Kept as separate commands, but `overview` is a cleaner
>   name that matches the internal `ChunkType::Overview` concept.

8. **End-to-end wiring**
   - `rebuild-index` command: git tracking → chunking → embedding → store (explicit full rebuild)
   - `search` command: implicit index check → query embedding → hnsw search → display results
   - `overview` command: implicit index check → filter overview chunks for file → display
   - Implicit indexing: shared `ensure_index_fresh` logic used by search/overview
   - Incremental re-indexing (diff-based update)

## Dependencies (Cargo.toml)

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
bincode = "1"
tokio = { version = "1", features = ["full"] }
tree-sitter = "0.24"
tree-sitter-rust = "0.23"
ort = "2"
hnsw_rs = "0.3"
anyhow = "1"
dirs = "6"
toml = "0.8"
```

Versions are approximate — verify latest at implementation time.

## Success Criteria

- `cargo debrief search "chunking"` auto-indexes on first run, returns relevant chunks
- `cargo debrief search "DebriefService"` returns the trait definition
  (metadata boost places it at #1)
- `cargo debrief overview src/main.rs` returns file API surface
- Incremental re-index after a code change re-embeds only changed files
- `cargo debrief rebuild-index .` forces full re-index
- Index persists to `.git/debrief/` and reloads on next invocation

## Result

### Phase 1C — Search Pipeline (completed 2026-04-04)

**Embedding pipeline** (`src/embedder.rs`, 374 lines):
- `ModelRegistry` with two models: `nomic-embed-text-v1.5` (768-dim, default) and `bge-large-en-v1.5` (1024-dim)
- `Embedder::load` — downloads ONNX model + tokenizer.json from HuggingFace with streaming progress bar, atomic `.tmp` rename
- `Embedder::embed_batch` — tokenization via `tokenizers` crate, ONNX inference via `ort` 2.0.0-rc.12, mean pooling + L2 normalization
- `Mutex<Session>` for `&self` embed calls (serializes concurrent inference — acceptable for Phase 1)
- `config::save_config` added; `load_layer` made public as `load_layer_single`
- `InProcessService::set_embedding_model` implemented: validates against registry, load-set-save config layer

**Vector search** (`src/search.rs`, 262 lines):
- `SearchIndex::build` — filters to embedded chunks, builds `hnsw_rs::Hnsw` with `DistCosine`
- `SearchIndex::search_by_vector` — ANN search with over-fetch (`top_k*2`), additive metadata boosting (+0.3 exact symbol match, +0.1 partial), re-sort and truncate
- `SearchIndex::search` — wraps `search_by_vector` with Embedder query embedding
- HNSW params: 16 connections, 16 layers, ef_construction=200, ef_search=50

**New dependencies**: `ort`, `tokenizers`, `reqwest`, `indicatif`, `futures-util`, `hnsw_rs`

**Tests**: 38 passing, 2 ignored (network-gated model download). 6 new tests added.

**Deviations from ticket**:
- `ort` pinned to `2.0.0-rc.12` (pre-release; cargo rejects `"^2"`)
- `ndarray` not needed — used tuple tensor construction instead
- Model named `nomic-embed-text-v1.5` (no separate `nomic-embed-code` ONNX export exists)

### Phase 1D — Integration & Polish (completed 2026-04-04)

**End-to-end wiring** (`src/service.rs`, +219/-50 lines):
- `InProcessService::index` — full reindex pipeline: config → git changes → RustChunker → embed_batch → save_index
- `InProcessService::search` — ensure_index_fresh → flatten chunks → SearchIndex::build → search
- `InProcessService::overview` — ensure_index_fresh → filter ChunkType::Overview → join display_text
- `ensure_index_fresh` — compares HEAD commit and model name against stored index, triggers incremental or full reindex as needed
- Private helpers: `index_path`, `make_embedder`, `run_index` (shared by explicit and implicit indexing)

**CLI renames**:
- `index` → `rebuild-index` (manual recovery, no path argument)
- `get-skeleton` → `overview` (matches internal ChunkType::Overview)
- Implicit auto-indexing on `search` and `overview`

**Smoke test results** (self-hosted, 13 files, 144 chunks):
- `search "chunking"` → #1 Chunk struct (0.76), #2 Chunker trait (0.72)
- `search "DebriefService"` → #1 trait def (0.92), +0.35 gap from symbol boost
- `overview src/chunk.rs` → 5 type definitions shown

**Tests**: 37 passing, 2 ignored. Net -1 from stub test removal.

**Deviations from plan**:
- `embed_batch` called synchronously (spawn_blocking deferred as tech debt)
- `_path` parameter on `index` silently ignored (removed from CLI, trait parameter kept for future use)
- `SearchIndex::build` clones all chunks from IndexData (optimization deferred)
