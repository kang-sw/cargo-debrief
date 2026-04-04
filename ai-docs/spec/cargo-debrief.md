---
title: cargo-debrief
summary: RAG-based code retrieval tool — AST-aware chunking and hybrid search for LLM context
features:
  - 🚧 Project Configuration
    - 🚧 Shared Configuration (`.debrief/`)
    - 🚧 Local Storage (`.git/debrief/`)
    - 🚧 Global Configuration
  - 🚧 Code Indexing
    - 🚧 AST-Aware Chunking
    - 🚧 Cross-File Skeleton Assembly
    - 🚧 Dual Text Representation
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
  - 🚧 Dependency Indexing
    - 🚧 Language-Specific Dependency Detection
---

# cargo-debrief

A CLI tool providing RAG (Retrieval-Augmented Generation) over codebases.
Parses source code into metadata-rich, AST-aware chunks, embeds them as
vectors, and serves search with metadata-boosted scoring so LLMs receive
only the relevant code fragments instead of entire files.

## 🚧 Project Configuration

Two-layer configuration model separating shared and local data.

### 🚧 Shared Configuration (`.debrief/`)

Git-tracked project configuration shared across the team.

```
.debrief/
  config.toml           # language settings, chunking options
```

- Committed to the repository — all team members share the same settings.
- Contains: language configuration, dependency include paths (shared),
  embedding model preference, chunking options.
- Analogous to `.vscode/settings.json`.

### 🚧 Local Storage (`.git/debrief/`)

Machine-local data not tracked by git. Stored inside `.git/` so it is
automatically invisible to git status.

```
.git/debrief/
  index.bin             # vector index (binary, large)
  local-config.toml     # machine-specific overrides
```

- Contains: index files, local dependency paths, user-specific
  overrides, cached embedding model reference.
- Analogous to `.git/info/exclude`.
- Index files and embedding model cache live here — they are
  machine-specific and should never be committed.

### 🚧 Global Configuration

User-level defaults stored in a platform-standard config directory
(e.g., `~/.config/debrief/`).

- Global embedding model preference (`set-embedding-model --global`)
- Default chunking options
- Project-level config (`.debrief/config.toml`) overrides global.
  Local config (`.git/debrief/local-config.toml`) overrides both.

> [!note] Constraints
> - Git root detection looks for `.git` as a **directory** only. Git
>   worktrees and submodules use `.git` as a file — project and local
>   config paths will not be resolved in those cases (falls back to
>   global config only).

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
- Index is stored in `.git/debrief/index.bin`.

### 🚧 AST-Aware Chunking

Source files are parsed with tree-sitter into chunks at semantic
boundaries rather than fixed token counts. Two chunk types:

**Type overview chunk** (per type, one per struct/enum/trait):

```rust
// src/pool.rs
struct ConnectionPool {
    connections: Vec<Connection>,
    max_size: usize,
}

impl ConnectionPool {
    fn new(max_size: usize) -> Self;
    fn acquire(&self) -> Option<&Connection>;
    fn release(&mut self, conn: Connection);
}

impl Drop for ConnectionPool {
    fn drop(&mut self);
}
```

Contains the type definition and all method signatures (no bodies).
Multiple `impl` blocks within the same file are aggregated.

**Function chunk** (per function/method):

```rust
// src/pool.rs
impl ConnectionPool {
    fn acquire(&self) -> Option<&Connection> {
        // actual function body ...
    }
}
```

Contains a single function body wrapped in its parent `impl` context.
Free functions include module path context instead.

> [!note] Constraints
> - Chunking quality depends on tree-sitter grammar availability for
>   the target language.
> - Very large functions (>200 lines) may need sub-function splitting
>   — strategy TBD.

### 🚧 Cross-File Skeleton Assembly

Type overview chunks aggregate `impl` blocks found in the same file.
Cross-file assembly (e.g., trait impls in another module) follows a
**conservative policy**:

- **Same file**: all `impl Type` blocks are merged into one skeleton.
  Safe — same file guarantees same type.
- **Different file**: kept as separate chunks. Not merged unless the
  fully qualified type path is unambiguously identical.
- **Ambiguous cases** (name collision across modules, proc macro
  generated code): never merged.

False negatives (incomplete skeleton) are acceptable — the individual
`impl` chunks still exist and are searchable. False positives
(incorrectly merged impls from different types) are not acceptable.

> [!note] Constraints
> - Generic type parameters are stripped when keying impl blocks:
>   `impl Foo<Bar>` and `impl Foo<Baz>` are both aggregated under `Foo`.
>   Methods from all specializations appear in the same overview chunk.

### 🚧 Dual Text Representation

Each chunk stores two text fields, optimized for different consumers:

| Field | Consumer | Contents |
|-------|----------|----------|
| `display_text` | LLM (search results) | Clean, self-contained code chunk |
| `embedding_text` | Embedding model | `display_text` + contextual metadata |

`embedding_text` prepends additional context to improve retrieval:

```
// crate::net::pool (src/pool.rs:42..67)
// pub struct ConnectionPool — connection management
impl ConnectionPool {
    fn acquire(&self) -> Option<&Connection> {
        // body ...
    }
}
```

Additional context in `embedding_text` may include:
- Fully qualified module path
- File path and line range
- Doc comments
- Parent type signature
- Visibility and kind annotations

The two fields can be tuned independently — `embedding_text` for
retrieval quality, `display_text` for LLM readability.

### 🚧 Chunk Metadata

Every chunk carries structured metadata extracted at parse time.
Metadata enables search filtering, score boosting, and result
presentation without relying on a separate keyword index.

| Field | Example | Purpose |
|-------|---------|---------|
| `symbol_name` | `ConnectionPool::new` | Exact-match score boosting |
| `kind` | function, struct, trait, impl, enum, module | Filtering by symbol kind |
| `parent` | `ConnectionPool` | Skeleton linkage |
| `visibility` | pub, pub(crate), pub(super), private | Filter by API surface |
| `file_path` | `src/pool.rs` | Source location |
| `line_range` | `42..87` | Source reference |
| `chunk_type` | overview, function | Chunk kind |
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
> - Git worktrees and submodules (`.git` file, not directory) are not
>   detected — treated as non-git directories until worktree support is
>   added.
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
- Each result returns the chunk's `display_text`. Embeddings are
  computed from `embedding_text` (which includes additional context).

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
- `--global` sets the model in global config (`~/.config/debrief/`).
- Without `--global`, sets the model in `.debrief/config.toml`
  (shared with team).
- Model binary files are cached in global data directory
  (e.g., `~/.local/share/debrief/models/`). Never stored per-project.
- Active model reference recorded in `.git/debrief/local-config.toml`.
- Resolution order: local → project → global → default.

> [!note] Constraints
> - Changing the model invalidates the existing index — a full re-index
>   is required after switching models.
> - Default model selection is TBD (candidates: nomic-embed-code 137M,
>   bge-large-en-v1.5 335M).

## 🚧 Index Persistence

The search index is serialized to disk for fast reload across sessions.
Stored in `.git/debrief/` (local, not committed).

- Format: bincode-serialized, with a version field in the header.
- On version mismatch (e.g., after a tool upgrade), the index is
  invalidated and a full re-index is triggered automatically.
- Index includes: chunk display_text, chunk embedding_text, chunk
  metadata, vector embeddings, file metadata, last-indexed commit
  hash, embedding model identifier.

> [!note] Constraints
> - No external database. The index is a single file (or small set of
>   files) in `.git/debrief/`.
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

## 🚧 Dependency Indexing

Index public API skeletons of project dependencies for improved
search coverage. Deferred — not part of initial implementation.

```
cargo debrief index . --with-deps
```

- Indexes **direct dependencies only** (not transitive).
- **Skeleton only** — public type signatures, no function bodies.
- Dependency chunks receive a lower base score than project code,
  so project results always rank higher by default.

### 🚧 Language-Specific Dependency Detection

| Language | Auto-detection | Fallback |
|----------|---------------|----------|
| **Rust** | `cargo metadata` → dep locations + public API | — |
| **C++** | `compile_commands.json` → include paths | `debrief dep cpp-include <path>` |
| **Python** | Active venv `site-packages` + `.pyi` stubs | `debrief dep py-packages <venv-path>` |

- Rust: fully automatic via `cargo metadata`. Rustdoc JSON output
  may provide more accurate public API extraction than tree-sitter.
- C++: `compile_commands.json` (generated by CMake) is the primary
  source. Visual Studio `.vcxproj` parsing is possible but complex.
  Manual fallback via CLI command for unsupported build systems.
- Python: detect active virtualenv and index installed packages.
  `.pyi` type stubs are preferred over source when available.

Shared dependency paths go in `.debrief/config.toml` (git-tracked).
Machine-specific paths go in `.git/debrief/local-config.toml`.

> [!note] Constraints
> - Dependency skeletons can add ~5K-10K chunks to the index.
>   At project scale (~20K total) this is within hnsw_rs capacity.
> - Proc macro generated APIs are not visible to tree-sitter and
>   will not be indexed.
> - Deferred until core project-code search quality is validated.
