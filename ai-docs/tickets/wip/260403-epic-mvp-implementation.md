---
category: epic
priority: high
parent: null
plans: null
related:
  - 260404-refactor-service-trait-multi-workspace  # cross-cutting: multi-workspace trait shape
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
- CLI commands: `index`, `search`, `get-skeleton`, `set-embedding-model`

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

8. **End-to-end wiring**
   - `index` command: git tracking → chunking → embedding → store
   - `search` command: query embedding → hnsw search → display results
   - `get-skeleton` command: retrieve type overview chunks for a file
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

- `cargo debrief index .` successfully indexes this project's Rust source
- `cargo debrief search "chunking"` returns relevant chunks with metadata
- `cargo debrief search "DebriefService"` returns the trait definition
  (metadata boost places it at #1)
- `cargo debrief get-skeleton src/main.rs` returns file overview
- Incremental re-index after a code change re-embeds only changed files
- Index persists to `.git/debrief/` and reloads on next invocation

## Result

(to be filled on completion)
