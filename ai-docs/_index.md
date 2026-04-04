# cargo-debrief — Project Index

## Architecture

Module layout (`lib.rs` + `main.rs` split — `main.rs` is thin clap
wrapper, all logic behind `lib.rs`):

```
src/
  main.rs       — CLI entrypoint (clap): index, search, get-skeleton, set-embedding-model
  lib.rs        — module re-exports
  config.rs     — 3-layer config resolution (local → project → global → default)
  service.rs    — DebriefService trait (async RPITIT, project_root per method) + InProcessService (zero-sized)
  chunk.rs      — Chunk data model (Chunk, ChunkMetadata, ChunkKind, ChunkType, Visibility)
  chunker/      — Chunker trait + RustChunker (tree-sitter AST-aware chunking)
    mod.rs      — Chunker trait definition
    rust.rs     — RustChunker: two-pass AST walk, impl aggregation, dual text generation
  git.rs        — Git file tracking (head_commit, changed_files via Command shellout)
  store.rs      — Index serialization (IndexData, bincode + versioned header)
  embedder.rs   — (planned) ONNX Runtime embedding inference + model management
  search.rs     — (planned) vector search + metadata score boosting
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
cargo test
cargo run -- index [<path>]                          # index current directory
cargo run -- search "query" [--top-k N]              # vector search + metadata boosting
cargo run -- get-skeleton <file>                     # file-level overview
cargo run -- set-embedding-model [--global] <name>   # configure model
cargo run -- daemon status                           # check daemon
```

## Mental Model

See `ai-docs/mental-model/` for operational knowledge:
- `overview.md` — crate structure, module map, coupling notes
- `config.md` — 3-layer resolution, merge semantics, known limitations
- `service.md` — DebriefService trait, RPITIT non-object-safety, dispatch options
- `chunker.md` — two-pass design, impl aggregation, orphan impl handling
- `store.md` — bincode serialization, version mismatch semantics
- `git.md` — Command shellout, changed_files contract

## Session Notes

- Initial project setup. Research ticket captures architecture discussion.
- Phase 1A scaffold implemented: CLI, config, service trait.
- Phase 1B core indexing pipeline implemented: chunk model, tree-sitter Rust chunking, git tracking, index serialization.
- Service trait refactored: `project_root: &Path` added to all `DebriefService` methods; `InProcessService` is now zero-sized; config loading removed from `main.rs`.
