# cargo-debrief — Project Index

## Architecture

Module layout (`lib.rs` + `main.rs` split — `main.rs` is thin clap
wrapper, all logic behind `lib.rs`):

```
src/
  main.rs       — CLI entrypoint (clap): rebuild-index, search, overview, set-embedding-model
  lib.rs        — module re-exports
  config.rs     — 3-layer config resolution (local → project → global → default)
  service.rs    — DebriefService trait (async RPITIT, project_root per method) + InProcessService (zero-sized)
  chunk.rs      — Chunk data model (Chunk, ChunkMetadata, ChunkKind, ChunkType, Visibility)
  chunker/      — Chunker trait + RustChunker (tree-sitter AST-aware chunking)
    mod.rs      — Chunker trait definition
    rust.rs     — RustChunker: two-pass AST walk, impl aggregation, dual text generation
  git.rs        — Git file tracking (head_commit, changed_files via Command shellout)
  store.rs      — Index serialization (IndexData, bincode + versioned header)
  embedder.rs   — ONNX Runtime embedding: ModelRegistry, Embedder (load, embed_batch, mean pooling + L2 norm)
  search.rs     — Vector search: SearchIndex (hnsw_rs ANN + symbol-name metadata boosting)
  daemon.rs     — (Phase 2) daemon mode via CLI subcommand
```

CLI dispatches through `DebriefService` trait. Phase 1 uses `InProcessService`
(direct library calls). Phase 2 adds `DaemonClient` (IPC to daemon process).
Single binary — daemon runs as `cargo debrief daemon`, not a separate executable.

## Key Design Decisions

- **CLI-first with daemon**: Primary interface is CLI. Background daemon
  is lazy-spawned on first use, auto-expires on idle, serves all requests
  on the machine. MCP server mode layered on top later.
- **No external DB**: vectors stored in-memory as `Vec<[f32; N]>`, serialized
  to disk with bincode (versioned format).
- **Vector search + metadata boosting**: cosine similarity with hnsw_rs,
  metadata score boosting for exact symbol name matches.
- **Hierarchical chunking**: level 0 (struct skeletons — signatures
  only), level 1 (function bodies), level 2 (referenced type declarations).
  Search hits at level 1 auto-attach level 0 context.
- **Git-based incremental indexing**: store last-indexed commit hash, diff
  against HEAD to find changed files. Prioritize validating this early.
- **Rust-first, language-extensible**: Start with tree-sitter-rust. Chunker
  trait allows adding more languages later.
- **Embedding model management**: auto-download default model, configurable
  per-project or globally via `set-embedding-model`.

## Spec

- `spec/cargo-debrief.md` — Full feature spec: indexing, search, CLI, daemon, model management

## Conventions

- Tickets: `ai-docs/tickets/<status>/YYMMDD-<type>-<name>.md`
- Reference by stem only: `260403-research-rag-architecture`

## Build / Test

```bash
cargo build
cargo test                                           # unit (37) + offline integration (8) + network integration (3)
CARGO_DEBRIEF_SKIP_NETWORK=1 cargo test              # skip network tests (no model download)
cargo run -- rebuild-index [<path>]                  # full re-index (manual/recovery)
cargo run -- search "query" [--top-k N]              # vector search + metadata boosting (auto-indexes)
cargo run -- overview <file>                         # file-level overview (auto-indexes)
cargo run -- set-embedding-model [--global] <name>   # configure model
cargo run -- daemon status                           # check daemon
```

### Test architecture

| Layer | File(s) | Network | What it covers |
|-------|---------|---------|----------------|
| Unit tests | `src/*.rs` `#[cfg(test)]` | No | Module internals: config merge, chunker AST, store round-trip, model registry, HNSW search with fake vectors |
| Offline integration | `tests/integration.rs` | No | Cross-module boundaries with mock embedder: chunker→store round-trip, search with mock embeddings, config multi-layer merge, git→chunker pipeline |
| Network integration | `tests/integration_network.rs` | Yes (~130MB model download, cached) | Real ONNX embedder + search, chunker→embedder compatibility, semantic search quality smoke tests |

Network tests download `nomic-embed-text-v1.5` on first run to `~/.local/share/debrief/models/` (Linux) or `~/Library/Application Support/debrief/models/` (macOS). Cached after first download. Skip with `CARGO_DEBRIEF_SKIP_NETWORK=1`.

### Smoke test

See `ai-docs/smoke-test.md` for the manual CLI verification protocol.
Run after changes to service wiring, chunker, embedder, search, or store.

## Mental Model

See `ai-docs/mental-model/` for operational knowledge:
- `overview.md` — crate structure, module map, coupling notes
- `config.md` — 3-layer resolution, merge semantics, known limitations
- `service.md` — DebriefService trait, RPITIT non-object-safety, dispatch options
- `chunker.md` — two-pass design, impl aggregation, orphan impl handling
- `store.md` — bincode serialization, version mismatch semantics
- `git.md` — Command shellout, changed_files contract
- `embedder.md` — ModelRegistry, Embedder, ONNX inference, model download
- `search.md` — SearchIndex, hnsw_rs ANN, metadata boosting

## Session Notes

- Initial project setup. Research ticket captures architecture discussion.
- Phase 1A scaffold implemented: CLI, config, service trait.
- Phase 1B core indexing pipeline implemented: chunk model, tree-sitter Rust chunking, git tracking, index serialization.
- Service trait refactored: `project_root: &Path` added to all `DebriefService` methods; `InProcessService` is now zero-sized; config loading removed from `main.rs`.
- Phase 1C search pipeline implemented: embedder.rs (ONNX inference via ort, model registry with nomic-embed-text-v1.5 + bge-large-en-v1.5, streaming download, mean pooling + L2 norm), search.rs (hnsw_rs ANN, metadata symbol-name boosting), config save_config, set_embedding_model wired.
- Phase 1D integration & polish: end-to-end wiring of `index`, `search`, `overview` in InProcessService. Implicit auto-indexing via `ensure_index_fresh`. CLI renames: `index` → `rebuild-index`, `get-skeleton` → `overview`. Smoke test protocol added.
