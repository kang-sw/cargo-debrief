---
title: cargo-debrief
summary: RAG-based code retrieval tool — AST-aware chunking and hybrid search for LLM context
features:
  - 🚧 Project Configuration
    - 🚧 Shared Configuration (`.debrief/`)
    - 🚧 Local Storage (`.git/debrief/`)
    - 🚧 Global Configuration
  - Code Indexing
    - AST-Aware Chunking
    - 🚧 Cross-File Skeleton Assembly
    - 🚧 Dual Text Representation
    - 🚧 Chunk Metadata
    - Git-Based Incremental Re-Indexing
  - Code Search
    - Vector Similarity Search
    - Metadata Score Boosting
  - Overview
  - Configuration
    - Embedding Model Management
    - 🚧 GPU Acceleration
  - 🚧 LLM Chunk Summarization
    - Configuration
    - Behavior
    - Scale
  - Index Persistence
  - 🚧 Daemon Mode
  - 🚧 MCP Server
  - 🚧 Language Support
  - Dependency Indexing
    - Dependency Search Integration
    - Dependency Overview
    - Dependency Exclude Configuration
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

- Global preferences (embedding model, LLM endpoint, etc.) set via
  `config <key> <value> --global`
- Default chunking options
- Project-level config (`.debrief/config.toml`) overrides global.
  Local config (`.git/debrief/local-config.toml`) overrides both.

> [!note] Constraints
> - Git root detection looks for `.git` as a **directory** only. Git
>   worktrees and submodules use `.git` as a file — project and local
>   config paths will not be resolved in those cases (falls back to
>   global config only).

## Code Indexing

Index a codebase for subsequent search. Parses source files with
tree-sitter into semantically meaningful chunks and generates vector
embeddings for each chunk.

```
cargo debrief rebuild-index
```

- Always indexes the full project root (no path argument accepted).
- On first run, performs a full index of all supported source files.
- On subsequent runs, performs incremental re-indexing (see below).
- Project index stored in `.git/debrief/index.bin`; dependency index
  stored in `.git/debrief/deps-index.bin`. Both are rebuilt by this command.
- `rebuild-index` is the explicit manual command. `search` and `overview`
  automatically check index freshness and re-index before executing
  (implicit auto-indexing) — `rebuild-index` is reserved for forced
  full re-index or recovery scenarios.

### AST-Aware Chunking

Source files are parsed with tree-sitter into chunks at semantic
boundaries rather than fixed token counts. Three chunk types:

**Type overview chunk** (per type, one per struct/enum/trait):

```rust
// src/pool.rs
struct ConnectionPool {
    connections: Vec<Connection>,
    max_size: usize,
}

impl ConnectionPool {
    fn new(max_size: usize) -> Self { Self { connections: vec![], max_size } }
    fn acquire(&self) -> Option<&Connection>;
    fn release(&mut self, conn: Connection);
}

impl Drop for ConnectionPool {
    fn drop(&mut self);
}
```

Contains the type definition and all method signatures. Methods with 5 or
fewer lines are inlined with their full body; larger methods appear as
signature-only. Multiple `impl` blocks within the same file are aggregated.

**Module overview chunk** (per file, one per file that has free functions):

```rust
// crate::net::pool
pub fn init_pool(max_size: usize) -> ConnectionPool { ConnectionPool::new(max_size) }
pub fn shutdown_pool(pool: ConnectionPool);
```

Aggregates all free functions in the file. Small free functions (5 lines or
fewer) are inlined with their full body; larger ones appear as
signature-only. The `symbol_name` is the leaf module name; `kind` is
`module`; `chunk_type` is `overview`.

**Function chunk** (per large function/method):

```rust
// src/pool.rs
impl ConnectionPool {
    fn acquire(&self) -> Option<&Connection> {
        // actual function body ...
    }
}
```

Generated only for functions/methods exceeding 5 lines. Contains a single
function body wrapped in its parent `impl` context (for methods) or the
module path header (for free functions).

> [!note] Constraints
> - The 5-line threshold (`MIN_METHOD_CHUNK_LINES`) controls inlining.
>   Functions at or below this threshold are inlined into their parent
>   overview chunk and do not receive a standalone function chunk.
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

### Git-Based Incremental Re-Indexing

On re-index, only files changed since the last indexed commit are
re-parsed and re-embedded.

- The index stores the `last_indexed_commit` hash.
- Changed files are detected via `git diff --name-status <last_commit> HEAD`.
- Deleted files have their chunks removed from the index.
- Non-git directories fall back to full re-indexing.

> [!note] Constraints
> - Requires a git repository. Non-git directories always do full re-index.
> - Git worktrees and submodules (`.git` file, not directory) are not
>   detected — treated as non-git directories until worktree support is
>   added.
> - Tracks the working tree's HEAD, not uncommitted changes (TBD whether
>   to include staged/unstaged changes).

## Code Search

Vector similarity search with metadata-based score boosting.

```
cargo debrief search <query> [--top-k N] [--no-deps]
```

- `--top-k` defaults to 10.
- Returns ranked code chunks from the project and, by default, from
  dependency indexes. Dependency results are labeled `[dep: crate_name]`
  after the score in the output.
- `--no-deps` excludes dependency results entirely (also skips the
  dep-index freshness check).
- Each result is prefixed with a `// in crate::module` context comment
  identifying the containing module, followed by the chunk's `display_text`.
  Embeddings are computed from `embedding_text` (which includes additional
  context).
- Automatically checks index freshness and re-indexes if needed before
  searching (implicit auto-indexing).

### Vector Similarity Search

Cosine similarity between query embedding and chunk embeddings.
Handles both natural-language and identifier-based queries:

- `"memory deallocation logic"` — matches semantically
- `"ConnectionPool"` — embedding model picks up identifier tokens

Uses hnsw_rs for approximate nearest neighbor search with the
following parameters: 16 max connections, 16 max layers,
ef_construction=200, ef_search=50.

The search over-fetches `max(top_k*2, top_k+20)` raw candidates from
HNSW, applies metadata score boosting to all of them, then truncates
to `top_k` — ensuring that a boosted chunk just outside the raw top_k
window is not lost.

### Metadata Score Boosting

Vector similarity scores are adjusted based on chunk metadata matches
using additive boosts (scores may exceed 1.0):

- **Exact symbol name match** (case-insensitive): `+0.3`
- **Partial symbol name match** (substring in either direction,
  case-insensitive): `+0.1`
- **Dependency origin penalty**: `-0.1` applied to all dependency chunks,
  preventing dep results from crowding out project results.

This provides exact-lookup precision within the search command without
a separate symbol lookup command or BM25 keyword index.

> [!note] Constraints
> - Kind/visibility filtering flags (`--kind`, `--visibility`) are
>   scaffolded in the spec but not yet implemented.
> - Designed for project-scale data (~20K chunks, ~60MB vectors).
>   Suitable for most single-project codebases. Not tested at
>   monorepo scale (>100K files).
> - BM25 keyword search may be added later if metadata boosting
>   proves insufficient for exact-match queries.

## Overview

Retrieve a file-level overview showing only declarations and signatures.

```
cargo debrief overview <file>
cargo debrief overview --dep <crate_name>
```

- Shows struct/enum/trait definitions, function signatures, impl blocks —
  but not function bodies.
- Output is ordered by visibility: `pub` items first, then `pub(crate)`,
  then `pub(super)`, then private.
- Useful for understanding a file's API surface without reading the
  full implementation.
- Automatically checks index freshness and re-indexes if needed before
  retrieving the overview (implicit auto-indexing).
- `--dep <crate_name>` retrieves the overview for a dependency crate
  from the deps index (`deps-index.bin`) without re-embedding. Requires
  the dependency index to have been built (via `rebuild-index` or implicit
  auto-indexing on first `search`).

## Configuration

Unified configuration interface using dotted key paths.

```
cargo debrief config <key> [value] [--global]
```

- With `value`: set the key. Without `value`: print current value.
- `--global` reads/writes global config (`~/.config/debrief/`).
  Without `--global`, reads/writes project config (`.debrief/config.toml`).
- `cargo debrief config --list` shows all resolved values with their
  source (global, project, local, default).
- Resolution order: local → project → global → default.

### Embedding Model Management

```
cargo debrief config embedding.model <model-name> [--global]
```

- On first use, the default model is automatically downloaded with a
  streaming progress bar. Partial downloads are written to a `.tmp`
  file and renamed atomically on completion.
- Model binary files are cached at `{data_dir}/debrief/models/{model_name}/`
  where `data_dir` is the platform-standard data directory
  (`dirs::data_dir()`, e.g., `~/.local/share` on Linux,
  `~/Library/Application Support` on macOS). Never stored per-project.
- Validates the name against the known model registry; unknown names
  are rejected with an error.

**Supported models:**

| Name | Dim | Max tokens | HuggingFace repo |
|------|-----|-----------|-----------------|
| `nomic-embed-text-v1.5` (default) | 768 | 512 | `nomic-ai/nomic-embed-text-v1.5` |
| `bge-large-en-v1.5` | 1024 | 512 | `BAAI/bge-large-en-v1.5` |

### 🚧 GPU Acceleration

ONNX Runtime execution provider selection with GPU-first, CPU-fallback:

- **macOS**: CoreML (Neural Engine / GPU)
- **Linux/Windows**: CUDA (NVIDIA GPU)
- **Fallback**: CPU (always available)

Enabled via cargo feature flags (`gpu`, `cuda`). Default build is
CPU-only. Provider selection is automatic — if the GPU provider fails
to initialize, falls back to CPU silently.

> [!note] Constraints
> - Changing the model invalidates the existing index — a full re-index
>   is required after switching models.
> - Only models listed in the built-in registry are accepted.
>   Arbitrary HuggingFace model names are not supported.
> - GPU feature flags add build-time dependencies (CoreML framework,
>   CUDA toolkit). Default build remains dependency-light.

## 🚧 LLM Chunk Summarization

Optional LLM-powered enrichment of overview chunks to improve
structural semantic search quality. Uses an external
OpenAI-compatible API endpoint (vLLM, ollama, etc.).

### Configuration

```
cargo debrief config llm.endpoint "https://vllm.internal/v1" --global
cargo debrief config llm.token "sk-..." --global
cargo debrief config llm.model "qwen2.5-coder-7b" --global
```

```toml
# config.toml
[llm]
endpoint = "https://vllm.internal/v1"
token = "sk-..."
model = "qwen2.5-coder-7b"
```

### Behavior

- When `[llm]` config is present, `rebuild-index` generates a short
  architectural summary for each **type overview chunk** via
  `/v1/chat/completions`.
- The summary is prepended to `embedding_text`, bridging the
  vocabulary gap between user queries and code structure.
- If the endpoint is unreachable or returns an error, indexing
  continues without summaries (warning printed to stderr).
- Summaries are cached in the index. Only chunks from changed files
  are re-summarized on incremental re-index.
- Function-level chunks are not summarized (too many, low ROI).

### Scale

Overview-only summarization keeps volume manageable:

| Project size | Overview chunks | ~Tokens generated | Time @ 250 tok/s |
|-------------|----------------|-------------------|-----------------|
| Small (30 files) | ~60 | ~4,500 | ~18s |
| Medium (100 files) | ~300 | ~22,500 | ~90s |
| Large (500 files) | ~1,500 | ~112,500 | ~7.5 min |

One-time cost per file; incremental re-index re-summarizes only
changed files.

> [!note] Constraints
> - Requires an external LLM server — cargo-debrief does not bundle
>   or run LLM inference locally.
> - Summary quality depends on the model. Hallucinated summaries can
>   degrade search quality. 7B+ code-specialized models recommended.
> - The `/v1/chat/completions` endpoint must be OpenAI-compatible.

## Index Persistence

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

Per-workspace background process that keeps the ONNX model session and
HNSW index loaded in memory, eliminating ~2-4 seconds of startup
overhead on repeated CLI calls. In-process mode is the fallback when
the daemon is unavailable.

- **Per-workspace.** One daemon per project root (not system-wide).
- First CLI invocation transparently spawns the daemon if not running.
- **~3 minute idle expiry.** Short lifespan — purpose is eliminating
  delay during burst usage (e.g., AI agent sessions), not long-term
  persistence.
- **Temp-file-based RPC.** Avoids sandbox restrictions that affect Unix
  sockets and named pipes. Request/response via temp files in a known
  directory.
- **Discovery.** PID file in `.git/debrief/` (workspace-local).
- CLI detects daemon availability and falls back to in-process mode
  if the daemon is not running or cannot be spawned.
- In debug builds, CLI compares binary identity with the running daemon
  and kills/restarts on mismatch to prevent stale daemon processes
  during development.

> [!note] Constraints
> - Phase 1 uses in-process execution (no daemon). The `DebriefService`
>   trait accepts a project root per operation, so the switch to daemon
>   mode is transparent.
> - Temp-file RPC protocol details TBD.

## 🚧 MCP Server

MCP (Model Context Protocol) server exposing the same capabilities as
the CLI for direct LLM integration. Deferred beyond Phase 2.

- Will be layered on the daemon as an additional interface.
- Planned tools: `search_code`, `overview`, `index_project`.

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

## Dependency Indexing

Index public API surfaces of project dependencies so that search covers
dependency types, traits, and functions alongside project code.

- Indexes **all transitive dependencies**, public API items only.
  Indexing all transitive deps avoids the need to resolve `pub use`
  re-export chains — facade crates (e.g., `bevy`) are covered
  naturally because their sub-crates are all in the transitive set.
- Source discovery via `cargo metadata` (Rust). Each package's
  `manifest_path` locates its source files.
- Dependency index stored separately (`.git/debrief/deps-index.bin`).
  Staleness tracked via `Cargo.lock` content hash — re-index only
  when dependencies change.
- Each dependency chunk's `embedding_text` includes a root-dependency
  annotation (e.g., `[dependency] bevy_ecs (dependency of: bevy)`) to
  bridge the vocabulary gap between user queries and transitive dep
  names.
- Git submodules treated as dependencies (not project source). Details
  deferred to C++ chunker stage.

### Dependency Search Integration

Search merges project and dependency indexes by default. Dependency
results are deprioritized by a `-0.1` score penalty (see Metadata Score
Boosting) and labeled `[dep: crate_name]` in the output. Use `--no-deps`
to exclude dependency results entirely.

### Dependency Overview

```
cargo debrief overview --dep <crate_name>
```

Retrieves the public API overview of an indexed dependency crate directly
from `deps-index.bin`, filtered to overview chunks only, ordered by
visibility. No re-embedding required.

### Dependency Exclude Configuration

```toml
# .debrief/config.toml
[dependencies]
exclude = ["syn", "proc-macro2"]  # skip large, rarely searched crates
```

Packages listed in `[dependencies].exclude` are filtered out before
chunking during `run_deps_index`. Matching is by crate name. The field
uses overlay-replace semantics — project config replaces global rather
than appending.

### 🚧 Language-Specific Dependency Detection

| Language | Auto-detection | Fallback | Status |
|----------|---------------|----------|--------|
| **Rust** | `cargo metadata` → dep source paths + public API filtering | — | Implemented |
| **C++** | `compile_commands.json` → include paths | `debrief dep cpp-include <path>` | Planned |
| **Python** | Active venv `site-packages` + `.pyi` stubs | `debrief dep py-packages <venv-path>` | Planned |

- Rust: fully automatic via `cargo metadata`. Public API filtered by
  `pub` visibility on chunks (tree-sitter based). Implemented.
- C++: `compile_commands.json` (generated by CMake) is the primary
  source. Git submodules treated as dependencies. Manual fallback via
  CLI command for unsupported build systems.
- Python: detect active virtualenv and index installed packages.
  `.pyi` type stubs are preferred over source when available.

> [!note] Constraints
> - Transitive dep indexing can produce ~1.5K-10K chunks depending on
>   project size. Within hnsw_rs capacity at all scales.
> - Proc macro generated APIs are not visible to tree-sitter and
>   will not be indexed.
> - `Cargo.lock`-level staleness means all deps are re-indexed on any
>   dependency change. Per-package incremental re-indexing is a future
>   optimization.
