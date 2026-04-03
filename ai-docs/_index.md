# cargo-debrief — Project Index

## Architecture

Planned module layout (not yet implemented):

```
src/
  main.rs       — CLI entrypoint (clap): index, serve, search subcommands
  mcp.rs        — MCP server (tool definitions, JSON-RPC)
  indexer.rs    — tree-sitter parsing + AST-aware chunking
  embedder.rs   — ONNX Runtime embedding inference
  search.rs     — hybrid search (vector cosine similarity + BM25)
  git.rs        — git diff tracking, incremental re-indexing
  store.rs      — index serialization/deserialization (serde + bincode)
```

## Key Design Decisions

- **No external DB**: vectors stored in-memory as `Vec<[f32; N]>`, serialized
  to disk with bincode. Brute-force cosine similarity is fast enough for
  ~20K chunks.
- **Hybrid search**: BM25 for exact symbol/keyword matching + vector
  similarity for semantic/natural-language queries.
- **Hierarchical chunking**: level 0 (class/struct skeletons — signatures
  only), level 1 (function bodies), level 2 (referenced type declarations).
  Search hits at level 1 auto-attach level 0 context.
- **Git-based incremental indexing**: store last-indexed commit hash, diff
  against HEAD to find changed files.
- **Embedding model**: targeting ~137M param models (e.g., nomic-embed-code)
  via ONNX Runtime. Runs on CPU with acceptable latency.

## Conventions

- Tickets: `ai-docs/tickets/<status>/YYMMDD-<type>-<name>.md`
- Reference by stem only: `260403-research-rag-architecture`

## Build / Test

```bash
cargo build
cargo test
cargo run -- index .          # index current directory
cargo run -- serve            # start MCP server
cargo run -- search "query"   # CLI search (debug)
```

## Session Notes

- Initial project setup. Research ticket captures architecture discussion.
