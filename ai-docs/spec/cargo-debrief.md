---
title: cargo-debrief
summary: RAG-based code retrieval tool — AST-aware chunking and hybrid search for LLM context
features:
  - 🚧 Code Indexing
    - 🚧 AST-Aware Chunking
    - 🚧 Chunk Metadata
    - 🚧 Git-Based Incremental Re-Indexing
  - 🚧 Code Search
    - 🚧 Vector Similarity Search
    - 🚧 Metadata Score Boosting
  - 🚧 File Skeleton
  - 🚧 Embedding Model Management
  - 🚧 Index Persistence
  - 🚧 Daemon Mode
  - 🚧 MCP Server
  - 🚧 Language Support
---

# cargo-debrief

A CLI tool providing RAG (Retrieval-Augmented Generation) over codebases.
Parses source code into metadata-rich, AST-aware chunks, embeds them as
vectors, and serves search with metadata-boosted scoring so LLMs receive
only the relevant code fragments instead of entire files.

## 🚧 Code Indexing

Index a codebase for subsequent search. Parses source files with
tree-sitter into semantically meaningful chunks and generates vector
embeddings for each chunk.

```
cargo debrief index [<path>]
```

- `<path>` defaults to the current directory.
- On first run, performs a full index of all supported source files.
- On subsequent runs, performs incremental re-indexing (see below).
- Index is stored on disk alongside the project (location TBD).

### 🚧 AST-Aware Chunking

Source files are parsed with tree-sitter into chunks at semantic
boundaries rather than fixed token counts. Chunks are hierarchical:

| Level | Contents | Typical size |
|-------|----------|-------------|
| 0 — Skeleton | Struct/class/trait declarations, signatures only (no bodies) | ~10 lines |
| 1 — Function | Individual function/method bodies | ~20–100 lines |
| 2 — Reference | Declarations of types referenced by level-1 chunks | varies |

When a search returns a level-1 (function) hit, the level-0 skeleton
of the containing type is automatically attached, giving structural
context without the full file.

> [!note] Constraints
> - Chunking quality depends on tree-sitter grammar availability for
>   the target language.
> - Very large functions (>200 lines) may need sub-function splitting
>   — strategy TBD.

### 🚧 Chunk Metadata

Every chunk carries structured metadata extracted at parse time.
Metadata enables search filtering, score boosting, and result
presentation without relying on a separate keyword index.

| Field | Example | Purpose |
|-------|---------|---------|
| `symbol_name` | `ConnectionPool::new` | Exact-match score boosting |
| `kind` | function, struct, trait, impl, enum, module | Filtering by symbol kind |
| `parent` | `ConnectionPool` | Auto-attach containing type context |
| `visibility` | pub, pub(crate), private | Filter by API surface |
| `file_path` | `src/pool.rs` | Source location |
| `line_range` | `42..87` | Source reference |
| `chunk_level` | 0 (skeleton), 1 (body), 2 (reference) | Context assembly |
| `signature` | `pub fn new(size: usize) -> Self` | Quick preview |

When a query exactly matches a chunk's `symbol_name`, that chunk
receives a score boost — providing the precision of exact symbol
lookup without a separate command or keyword index.

### 🚧 Git-Based Incremental Re-Indexing

On re-index, only files changed since the last indexed commit are
re-parsed and re-embedded.

- The index stores the `last_indexed_commit` hash.
- Changed files are detected via `git diff --name-only <last_commit> HEAD`.
- Deleted files have their chunks removed from the index.
- Non-git directories fall back to full re-indexing.

> [!note] Constraints
> - Requires a git repository. Non-git directories always do full re-index.
> - Tracks the working tree's HEAD, not uncommitted changes (TBD whether
>   to include staged/unstaged changes).

## 🚧 Code Search

Vector similarity search with metadata-based score boosting.

```
cargo debrief search <query> [--top-k N]
```

- `--top-k` defaults to a reasonable value (e.g., 10).
- Returns ranked code chunks with file path, line range, relevance
  score, and chunk metadata.
- Each result includes the chunk text and, for function-level hits,
  the parent skeleton for context.

### 🚧 Vector Similarity Search

Cosine similarity between query embedding and chunk embeddings.
Handles both natural-language and identifier-based queries:

- `"memory deallocation logic"` — matches semantically
- `"ConnectionPool"` — embedding model picks up identifier tokens

Uses hnsw_rs for approximate nearest neighbor search.

### 🚧 Metadata Score Boosting

Vector similarity scores are adjusted based on chunk metadata matches:

- **Exact symbol name match**: query text matches `symbol_name` field
  → significant score boost. Provides exact-lookup precision within
  the search command.
- **Kind/visibility filtering**: optional flags to narrow results
  (e.g., only public functions, only structs).

This replaces the need for a separate BM25 keyword index or a
dedicated symbol lookup command.

> [!note] Constraints
> - Designed for project-scale data (~20K chunks, ~60MB vectors).
>   Suitable for most single-project codebases. Not tested at
>   monorepo scale (>100K files).
> - BM25 keyword search may be added later if metadata boosting
>   proves insufficient for exact-match queries.

## 🚧 File Skeleton

Retrieve a file-level overview showing only declarations and signatures.

```
cargo debrief get-skeleton <file>
```

- Shows struct/enum/trait definitions, function signatures, impl blocks —
  but not function bodies.
- Useful for understanding a file's API surface without reading the
  full implementation.

## 🚧 Embedding Model Management

Manage the embedding model used for vector search.

```
cargo debrief set-embedding-model [--global] <model-name>
```

- On first use, the default model is automatically downloaded.
- `--global` sets the model for all projects (user-level config).
- Without `--global`, sets the model for the current project only.
- Models are cached in a platform-standard user data directory.
- Project-level config overrides global config.

> [!note] Constraints
> - Changing the model invalidates the existing index — a full re-index
>   is required after switching models.
> - Default model selection is TBD (candidates: nomic-embed-code 137M,
>   bge-large-en-v1.5 335M).

## 🚧 Index Persistence

The search index is serialized to disk for fast reload across sessions.

- Format: bincode-serialized, with a version field in the header.
- On version mismatch (e.g., after a tool upgrade), the index is
  invalidated and a full re-index is triggered automatically.
- Index includes: chunk text, chunk metadata, vector embeddings,
  file metadata, last-indexed commit hash, embedding model identifier.

> [!note] Constraints
> - No external database. The index is a single file (or small set of
>   files) stored locally.
> - Index size scales with codebase: ~60MB for ~20K chunks at 768
>   dimensions.

## 🚧 Daemon Mode

Background service that keeps the index loaded in memory for fast
repeated queries. Phase 2 — not part of initial implementation.

- First CLI invocation transparently spawns the daemon if not running.
- Daemon serves all CLI requests on the machine (per-machine singleton).
- Auto-expires after a configurable idle timeout.
- CLI detects daemon availability and falls back to in-process mode
  if the daemon is not running.

> [!note] Constraints
> - IPC mechanism TBD (Unix domain socket, named pipe, or localhost HTTP).
> - Phase 1 uses in-process execution (no daemon). The `DebriefService`
>   trait abstracts the transport so the switch is transparent.

## 🚧 MCP Server

MCP (Model Context Protocol) server exposing the same capabilities as
the CLI for direct LLM integration. Deferred beyond Phase 2.

- Will be layered on the daemon as an additional interface.
- Planned tools: `search_code`, `get_skeleton`, `index_project`.

> [!note] Constraints
> - MCP SDK choice deferred. Will evaluate when implementation begins.

## 🚧 Language Support

Tree-sitter-based parsing with per-language grammar support.

- **Rust**: first supported language.
- Extensible to additional languages (C++, Python, TypeScript, etc.)
  via the `Chunker` trait interface.
- Each language requires a tree-sitter grammar crate and a `Chunker`
  implementation defining how to extract semantic chunks.

> [!note] Constraints
> - Initial release supports Rust only.
> - Adding a language requires implementing the Chunker trait — there
>   is no automatic/generic fallback chunking.
