---
category: research
priority: high
parent: null
plans: null
---

# RAG Architecture Research for cargo-debrief

## Problem

When LLMs work on large C++ (or other) codebases, reading source files
consumes enormous amounts of context window. A single class implementation
can span thousands of lines across headers and source files, but typically
only a fraction is relevant to the current task.

## Goal

Build an MCP server that provides RAG-based code retrieval — feeding the
LLM only the relevant code fragments instead of entire files.

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

Hierarchical chunking strategy:
- Level 0 (skeleton): class/struct declarations — signatures only, no bodies (~10 lines)
- Level 1 (function): individual function/method bodies (~20-100 lines)
- Level 2 (reference): declarations of types referenced by level 1 chunks

On a search hit at level 1, auto-attach the level 0 skeleton of the
containing class. This gives the LLM structural context without the
full file.

**2. Hybrid search: BM25 + vector similarity**

- BM25 (keyword): exact symbol name matching, e.g., `"ResourceManager::release"`
- Vector (semantic): natural language queries, e.g., "memory deallocation logic"
- Neither alone is sufficient for code search — combine both.

**3. Embedding model selection**

Target: ~137M-500M parameter models running locally.
- `nomic-embed-code` (137M): code-specialized, ~300MB VRAM, runs on CPU
- `bge-large-en-v1.5` (335M): strong general-purpose, ~700MB VRAM
- No need for GPU — embedding a single query is ~5ms on CPU.
  GPU only matters for bulk initial indexing.

Inference via ONNX Runtime (`ort` crate) for Rust-native execution.

**4. Vector storage — keep it simple**

For project-scale data (~20K chunks max):
- In-memory `Vec<[f32; 768]>`, brute-force cosine similarity
- Serialized to disk with `serde` + `bincode`
- 20K chunks * 768 dims * 4 bytes = ~60MB — trivially fits in memory
- No external vector DB needed (ChromaDB, Qdrant, etc. are overkill)

**5. Git-based incremental re-indexing**

- Store `last_indexed_commit` hash in the index file
- On re-index: `git diff --name-only <last_commit> HEAD -- '*.cpp' '*.h'`
- Re-parse and re-embed only changed files
- Far more reliable than filesystem mtime watching

**6. MCP server lifecycle**

Option A (chosen): MCP server IS the process. Claude Code spawns it,
it lives for the session, index loads from disk on startup (seconds).

Option B (rejected): separate daemon + thin MCP client. Added IPC
complexity for marginal benefit (avoiding ~2s index reload).

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
cargo-debrief index <path>           # initial/incremental indexing
cargo-debrief serve                  # MCP server mode
cargo-debrief search <query>         # direct search (debug/testing)
```

### Planned MCP Tools

- `index_project(root_path)` — trigger indexing
- `search_code(query, top_k)` — hybrid search, return ranked chunks
- `get_symbol(name)` — exact symbol lookup (signature + body)
- `get_skeleton(file)` — file-level declaration overview
- `get_references(symbol)` — call sites / usage locations

### Technology Stack

- Language: Rust (2024 edition)
- Parsing: `tree-sitter` + `tree-sitter-cpp` (extensible to other languages)
- Embedding: `ort` (ONNX Runtime) with nomic-embed-code or similar
- Search: custom hybrid (cosine similarity + BM25)
- Serialization: `serde` + `bincode`
- CLI: `clap`
- MCP: community Rust MCP SDK (evaluate maturity) or raw JSON-RPC
- Async: `tokio`

## Open Questions

- Which Rust MCP SDK to use? Evaluate `mcp-rs`, `rmcp`, or roll minimal
  JSON-RPC over stdio.
- Tokenizer handling for ONNX models — bundle tokenizer.json or use
  `tokenizers` crate?
- Should the index file format be versioned from the start?
- Multi-language support: start C++-only or design for language-agnostic
  chunking from day one?

## Next Steps

- [ ] Evaluate Rust MCP SDK options
- [ ] Prototype tree-sitter C++ chunking (extract functions, classes, signatures)
- [ ] Set up ort-based embedding pipeline with a test model
- [ ] Implement basic hybrid search
- [ ] Wire up as MCP server
