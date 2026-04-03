# cargo-debrief — Project Index

## Architecture

Planned module layout (not yet implemented):

```
src/
  main.rs       — CLI entrypoint (clap): index, search, set-embedding-model, daemon
  service.rs    — DebriefService trait (service boundary) + InProcessService
  chunker.rs    — Chunker trait + tree-sitter AST-aware chunking (Rust-first)
  embedder.rs   — ONNX Runtime embedding inference + model management
  search.rs     — hybrid search (vector cosine similarity + BM25)
  git.rs        — git diff tracking, incremental re-indexing
  store.rs      — index serialization/deserialization (serde + bincode, versioned)
  daemon.rs     — (Phase 2) background service + DaemonClient transport
```

CLI talks only through `DebriefService` trait. Phase 1 uses `InProcessService`
(direct library calls). Phase 2 adds `DaemonClient` (IPC to daemon process).

## Key Design Decisions

- **CLI-first with daemon**: Primary interface is CLI. Background daemon
  is lazy-spawned on first use, auto-expires on idle, serves all requests
  on the machine. MCP server mode layered on top later.
- **No external DB**: vectors stored in-memory as `Vec<[f32; N]>`, serialized
  to disk with bincode (versioned format). Brute-force cosine similarity
  is fast enough for ~20K chunks.
- **Hybrid search**: BM25 for exact symbol/keyword matching + vector
  similarity for semantic/natural-language queries.
- **Hierarchical chunking**: level 0 (struct skeletons — signatures
  only), level 1 (function bodies), level 2 (referenced type declarations).
  Search hits at level 1 auto-attach level 0 context.
- **Git-based incremental indexing**: store last-indexed commit hash, diff
  against HEAD to find changed files. Prioritize validating this early.
- **Rust-first, language-extensible**: Start with tree-sitter-rust. Chunker
  trait allows adding more languages later.
- **Embedding model management**: auto-download default model, configurable
  per-project or globally via `set-embedding-model`.

## Conventions

- Tickets: `ai-docs/tickets/<status>/YYMMDD-<type>-<name>.md`
- Reference by stem only: `260403-research-rag-architecture`

## Build / Test

```bash
cargo build
cargo test
cargo run -- index [<path>]                          # index current directory
cargo run -- search "query" [--top-k N]              # hybrid search
cargo run -- set-embedding-model [--global] <name>   # configure model
cargo run -- daemon status                           # check daemon
```

## Session Notes

- Initial project setup. Research ticket captures architecture discussion.
