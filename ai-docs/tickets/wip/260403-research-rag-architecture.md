---
category: research
priority: high
parent: null
plans: null
related:
  - 260404-refactor-service-trait-multi-workspace  # refines the daemon/service design from section 6
---

# RAG Architecture Research for cargo-debrief

## Problem

When LLMs work on large C++ (or other) codebases, reading source files
consumes enormous amounts of context window. A single class implementation
can span thousands of lines across headers and source files, but typically
only a fraction is relevant to the current task.

## Goal

Build a CLI tool (with future MCP server mode) that provides RAG-based
code retrieval — feeding the LLM only the relevant code fragments
instead of entire files.

## Research Summary

### RAG Concept

Retrieval-Augmented Generation: index documents as chunks, convert to
vector embeddings, retrieve top-k relevant chunks for a given query,
and inject them into the LLM prompt.

### Core Architecture Decisions

**1. Tree-sitter for AST-aware chunking**

Fixed-length chunking (e.g., 512 tokens) breaks code at arbitrary
boundaries. Tree-sitter parses source into an AST, enabling chunking
at semantic boundaries: functions, classes, structs, namespaces.

Two concrete chunk types:

- **Type overview chunk** (per type): struct/enum/trait definition +
  all method signatures (no bodies). Aggregates `impl` blocks from
  the same file. Answers "what can this type do?" queries.
- **Function chunk** (per function): single function body wrapped in
  `impl Type { fn ... { body } }` context. Self-contained — the impl
  wrapper gives embedding models type context without separate assembly.

Cross-file skeleton assembly follows a conservative policy:
- Same-file `impl` blocks → merged into one skeleton. Safe.
- Different-file `impl` blocks → kept as separate chunks. Not merged
  unless fully qualified path is unambiguously identical.
- Ambiguous cases (name collision, proc macro) → never merged.
- Rationale: false negatives (incomplete skeleton) are acceptable;
  false positives (wrong type merged) are not.

Each chunk stores dual text representations:
- `display_text`: clean code shown to the LLM in search results.
- `embedding_text`: `display_text` + contextual metadata (module path,
  file path, doc comments, parent type signature). The two can be
  tuned independently for retrieval quality vs LLM readability.

**2. Vector search with metadata score boosting**

Vector-only search (no BM25). Exact symbol matching handled via
chunk metadata (`symbol_name` field) score boosting instead of a
separate keyword index.

- Vector (semantic): natural language + identifier queries
- Metadata boost: query matches `symbol_name` → score boost
- BM25 (tantivy) available as future fallback if needed

Rationale: BM25's strength (exact keyword matching) overlaps with
metadata boosting for symbol names. Vector search handles semantic
discovery. Separate `get-symbol` command was also deferred — search
with metadata boost covers its use case.

**3. Embedding model selection**

Target: ~137M-500M parameter models running locally.
- `nomic-embed-code` (137M): code-specialized, ~300MB VRAM, runs on CPU
- `bge-large-en-v1.5` (335M): strong general-purpose, ~700MB VRAM
- No need for GPU — embedding a single query is ~5ms on CPU.
  GPU only matters for bulk initial indexing.

Inference via ONNX Runtime (`ort` crate) for Rust-native execution.

Model management:
- Auto-download on first use with a reasonable default model
- `cargo debrief set-embedding-model [--global] <model-name>` to reconfigure
- Per-project or global model selection
- Models cached in a standard user data directory

**4. Vector storage — hnsw_rs**

`hnsw_rs` for ANN vector search. Pure Rust, lightweight, mmap for
data vectors.

Scale analysis:
- ~20K chunks (most single projects): ~45 MB total (graph + vectors)
- ~100K chunks (compiler-scale): ~210 MB total
- Brute-force would also work at 20K scale (<10ms), but hnsw_rs
  future-proofs for larger codebases at negligible cost.

Evaluated alternatives:
- `usearch`: full mmap (graph+data) but requires C++ compiler.
- `lancedb`: BM25+vectors but massive dependency tree (Arrow/DataFusion).
- `qdrant`: requires separate server process.
- Brute-force: sufficient for 20K but no growth headroom.

**5. Git-based incremental re-indexing**

- Store `last_indexed_commit` hash in the index file
- On re-index: `git diff --name-only <last_commit> HEAD -- '*.cpp' '*.h'`
- Re-parse and re-embed only changed files
- Far more reliable than filesystem mtime watching

**6. CLI-first with daemon architecture**

Option A (rejected): MCP server IS the process. Claude Code spawns it,
it lives for the session.

Option B (chosen): CLI-first with lazy-spawned daemon.
- Primary interface is the CLI (`cargo debrief index`, `search`, etc.)
- First CLI invocation transparently spawns a background daemon if not running
- Daemon holds indexes in memory, serves all requests on the machine
- Daemon expires after configurable idle timeout (no interaction → auto-exit)
- Per-machine singleton: one daemon serves all CLI invocations and sessions
- MCP server mode can be layered on top of the daemon later
- Rationale: AI CLI tool usage is strong, debugging is easier, no special
  setup needed — the daemon lifecycle is invisible to the user

Implementation phasing:
- Phase 1: `DebriefService` trait as service boundary. `InProcessService`
  impl — CLI calls library directly, no IPC. All core logic developed here.
- Phase 2: `DaemonClient` impl — CLI sends requests over IPC to daemon
  process. Daemon hosts `InProcessService` internally.
- The trait surface mirrors CLI commands (index, search,
  get_skeleton). Internal modules (chunker, embedder, search) are NOT
  exposed through the trait — they remain implementation details.
- Transport abstraction is cheap (trait is small, in-process impl is
  trivial delegation) and eliminates refactoring cost at daemon extraction.
- **Updated (see `260404-refactor-service-trait-multi-workspace`):**
  Each trait method accepts `project_root: &Path` explicitly instead of
  binding a workspace at construction time. This makes the trait
  multi-workspace-capable by default: a single daemon instance serves N
  workspaces by dispatching on the root path each call receives.

**7. LLM-based chunk summarization — deferred**

Using a local LLM (13B) to generate natural-language summaries of each
chunk at indexing time was considered. Deferred because:
- Indexing cost is high (hours for large projects)
- Embedding models already bridge code↔query gap reasonably well
- Can be added later selectively for complex functions (template
  metaprogramming, bitwise logic, etc.) where code alone is opaque

**8. RAG metadata stored separately, not as code comments**

Embedding descriptions as inline comments was rejected:
- Comments rot when code changes but comments don't get updated
- Stale comments can cause RAG to confidently return wrong results
- Better: store descriptions as separate index fields, regenerated
  on each re-indexing pass

**9. Descriptive naming as natural RAG boost**

Self-documenting identifiers (function names, variable names) contain
natural-language tokens that embedding models pick up automatically.
This is the most maintenance-free way to improve retrieval quality —
names ARE code, so they never go stale.

### Planned CLI Interface

```
cargo debrief index [<path>]                          # initial/incremental indexing
cargo debrief search <query> [--top-k N]              # vector search + metadata boosting
cargo debrief get-skeleton <file>                     # file-level overview
cargo debrief set-embedding-model [--global] <name>   # configure model
cargo debrief daemon status                           # check daemon state
```

The daemon is spawned transparently by any command that needs it.
It auto-expires after idle timeout.

### Planned MCP Tools (deferred)

MCP server mode will be layered on the daemon later, exposing the
same capabilities as the CLI:
- `search_code(query, top_k)` — vector search, return ranked chunks
- `get_skeleton(file)` — file-level declaration overview
- `index_project(root_path)` — trigger indexing

### Technology Stack

- Language: Rust (2024 edition)
- Parsing: `tree-sitter` + `tree-sitter-rust` (start with Rust; `Chunker` trait for language extensibility)
- Embedding: `ort` (ONNX Runtime) with nomic-embed-code or similar
- Vector search: `hnsw_rs` (ANN) with metadata-based score boosting
- Serialization: `serde` + `bincode` (version field in index header from day one)
- CLI: `clap`
- IPC: CLI ↔ daemon communication (Unix socket or similar)
- Async: `tokio`

## Open Questions

- Tokenizer handling for ONNX models — bundle tokenizer.json or use
  `tokenizers` crate?
- Daemon IPC mechanism — Unix domain socket? Named pipe? HTTP on localhost?
- Default embedding model selection — which model ships as the default?

## Resolved Questions

- ~~Which Rust MCP SDK to use?~~ → Deferred. CLI-first, MCP later.
- ~~Index file format versioned from the start?~~ → Yes. Version u32 in header.
- ~~Multi-language: C++-only or language-agnostic?~~ → Rust-first, `Chunker`
  trait for extensibility. Validate with git diff/file tracking before
  expanding to other languages.
- ~~BM25 needed alongside vector search?~~ → No. Vector search + metadata
  score boosting replaces BM25. Exact symbol matching handled via
  `symbol_name` metadata boost. BM25 (tantivy) can be added later if needed.
- ~~Separate `get-symbol` command?~~ → Deferred. Metadata-boosted search
  covers exact symbol lookup. Dedicated command adds complexity without
  sufficient benefit for MVP.
- ~~Search library?~~ → `hnsw_rs` for vector ANN search. Pure Rust,
  lightweight, mmap for data vectors. BM25/tantivy deferred.

## Next Steps

- [ ] Define `DebriefService` trait + `InProcessService` scaffold
- [ ] Implement git file tracking (diff-based changed file detection)
- [ ] Prototype tree-sitter Rust chunking (Chunker trait + chunk metadata)
- [ ] Set up ort-based embedding pipeline with a test model
- [ ] Implement vector search with hnsw_rs + metadata score boosting
- [ ] Versioned index serialization (store module)
- [ ] CLI commands (clap) wired through DebriefService
- [ ] Daemon extraction (Phase 2)
