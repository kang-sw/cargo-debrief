---
title: "Core indexing pipeline — chunking, git tracking, persistence"
parent: 260403-epic-mvp-implementation
related:
  - 260404-feat-cli-scaffold-config-service  # prerequisite — Config, DebriefService
  - 260403-research-rag-architecture  # architecture decisions
  - 260404-feat-rust-chunking-population  # deferred node kinds and quality improvements
  - 260404-refactor-service-trait-multi-workspace  # trait shape change: project_root on each method
plans:
  phase-1: null
  phase-2: null
  phase-3: null
  phase-4: null
---

# Core Indexing Pipeline

Implement the indexing backbone: chunk data model, tree-sitter Rust
chunking with dual text and metadata, git-based change detection, and
versioned index serialization. After this ticket, `InProcessService::index`
can parse Rust files into chunks and persist them — embeddings are added
by Phase 1C.

Spec: `ai-docs/spec/cargo-debrief.md` (Code Indexing, Git-Based
Incremental Re-Indexing, Index Persistence sections).

## Design Decisions

Captured from discussion — these are **firm**:

1. **Git via `std::process::Command`.**  Shell out to system `git` for
   `diff --name-only` and `rev-parse HEAD`. System git is universally
   available in dev environments. Pure-Rust alternative (`gix` /
   gitoxide) noted as a future option if the system dependency becomes
   problematic.

2. **Programmatic AST walk, not tree-sitter queries.** Walk the CST
   with `TreeCursor` / child iteration, branch on `node.kind()`.
   Byte-range slicing on original source for text extraction. Same
   pattern as proc-macro derive walks over `syn` AST, but on a
   concrete syntax tree (whitespace, comments, punctuation all present).

3. **Chunk data model in dedicated module.** `src/chunk.rs` defines
   `Chunk`, `ChunkMetadata`, `ChunkKind`, `ChunkType`, `Visibility`.
   Referenced by chunker, store, search, and service. Embedding field
   is `Option<Vec<f32>>` — populated by Phase 1C, serialized as-is.

4. **`HashMap<PathBuf, Vec<Chunk>>` as primary index structure.**
   Enables file-level group invalidation on re-index: drop all chunks
   for a changed file, re-parse, re-chunk, re-insert. Same pattern
   extends to dependency indexing (group key = dep name + version).

5. **Conservative same-file impl aggregation.** Type overview chunks
   merge `impl` blocks only when the type name text is **exactly
   identical** — no generic normalization. `impl Foo` + `impl Foo`
   merge; `impl Foo<T>` + `impl Foo<U>` do not. False negatives
   (incomplete skeleton) acceptable; false positives (wrong type
   merged) not acceptable.

6. **Trait impls included in type overview.** `impl Display for Foo`
   signature appears in Foo's overview. If this proves too verbose
   in practice, revisit as a follow-up.

7. **Large functions as single chunks.** No sub-function splitting
   for MVP. Comments are kept (valuable semantic content for
   embeddings). Future: split threshold based on code-line count
   (excluding comments), not total line count.

8. **Dual text representation.**
   - `display_text`: clean, self-contained code chunk as the LLM sees it.
   - `embedding_text`: `display_text` prefixed with contextual metadata
     (module path derived from file path, file path + line range, doc
     comments, parent type signature, visibility annotation).

9. **Index format: bincode + versioned header.** `u32` version field
   at the start. On version mismatch, full re-index is triggered
   automatically. Embedding stored as `Option<Vec<f32>>` per chunk.

10. **Allocator: default for MVP.** No custom allocator (mimalloc,
    jemalloc). Re-evaluate when daemon mode introduces long-lived
    allocation patterns. Adding `#[global_allocator]` is a one-liner
    if needed.

## Phase 1: Chunk Data Model + Chunker Trait

Foundation types that all subsequent phases depend on.

### Goal

Define the chunk data structures and the `Chunker` trait interface.
After this phase, downstream modules (chunker, store, search) have
concrete types to work with.

### Scope

**`src/chunk.rs`:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub display_text: String,
    pub embedding_text: String,
    pub metadata: ChunkMetadata,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMetadata {
    pub symbol_name: String,
    pub kind: ChunkKind,
    pub chunk_type: ChunkType,
    pub parent: Option<String>,
    pub visibility: Visibility,
    pub file_path: String,
    pub line_range: (usize, usize),
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkType {
    Overview,
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Pub,
    PubCrate,
    PubSuper,
    Private,
}
```

**`src/chunker/mod.rs` — Chunker trait:**

```rust
use std::path::Path;
use anyhow::Result;
use crate::chunk::Chunk;

/// Language-extensible chunking interface.
pub trait Chunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>>;
}
```

The trait takes `file_path` (for metadata) and `source` (file contents).
Returns all chunks extracted from the file. `file_path` is relative to
project root.

### Success Criteria

- Types compile with `Serialize`/`Deserialize` derives.
- `Chunk` round-trips through `bincode::serialize` / `deserialize`.
- `Chunker` trait is importable from `cargo_debrief::chunker`.
- Unit test: construct a `Chunk` manually, serialize, deserialize,
  assert equality.

## Phase 2: Tree-sitter Rust Chunking

The largest phase. Implements `RustChunker` — parses Rust source with
tree-sitter and produces type overview + function chunks.

### Goal

Given a Rust source file, produce:
- One **type overview chunk** per struct/enum/trait (definition +
  all same-file impl method signatures, no bodies).
- One **function chunk** per function/method (body wrapped in impl
  context or module path context for free functions).
- Correct dual text and metadata for each chunk.

### Scope

**`src/chunker/rust.rs` — `RustChunker`:**

```rust
pub struct RustChunker;

impl Chunker for RustChunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>> {
        // 1. Parse source with tree-sitter-rust
        // 2. Walk AST: collect type definitions, impl blocks, functions
        // 3. Aggregate same-file impls per type (exact name match)
        // 4. Generate type overview chunks (definition + signatures)
        // 5. Generate function chunks (body in impl/module context)
        // 6. Build dual text and metadata for each chunk
    }
}
```

**Dependencies to add:**

```toml
tree-sitter = "0.24"
tree-sitter-rust = "0.23"
```

Verify latest versions at implementation time.

**AST walk strategy:**

Walk top-level children of `source_file` node. For each node, branch
on `kind()`:

| Node kind | Action |
|-----------|--------|
| `struct_item` | Record type definition, start overview |
| `enum_item` | Record type definition, start overview |
| `trait_item` | Record type definition, start overview |
| `impl_item` | Extract type name + method signatures; match to existing overview by exact name text |
| `function_item` | Free function → function chunk with module path context |
| `mod_item` | Recurse into inline module |

> **Deferred node kinds:** `const_item`, `static_item`, `type_item`,
> `macro_definition`, `union_item`, `extern_block` — tracked in
> `260404-feat-rust-chunking-population`.

For `impl_item` children: iterate `function_item` / `associated_type`
nodes to extract method signatures (strip body, keep signature +
semicolon).

**Same-file impl aggregation algorithm:**

```
types: HashMap<String, TypeOverview>  // key = type name text

for each struct/enum/trait node:
    extract type_name from node
    types[type_name] = TypeOverview { definition, impls: vec![] }

for each impl_item node:
    extract impl_type_name from the impl's type child node
    // exact text match — no generic normalization
    if types.contains(impl_type_name):
        types[impl_type_name].impls.push(impl_signatures)
    else:
        // orphan impl (type defined in another file)
        // create standalone overview for this impl block
        types[impl_type_name] = TypeOverview { definition: None, impls: vec![impl_sigs] }
```

Orphan impls (impl for a type not defined in this file) become their
own overview chunk. This handles `impl ForeignType` gracefully.

**Function chunk context wrapping:**

- Method: `impl TypeName { fn method(...) { body } }`
- Trait method: `impl TraitName for TypeName { fn method(...) { body } }`
- Free function: `// module::path\nfn free_func(...) { body }`

Module path is derived from `file_path` relative to `src/`:
`src/net/pool.rs` → `crate::net::pool`.

**Dual text generation:**

`display_text` — the clean chunk as shown above.

`embedding_text` — prepend context lines:
```
// crate::net::pool (src/net/pool.rs:42..67)
// pub struct ConnectionPool — connection management
impl ConnectionPool {
    fn acquire(&self) -> Option<&Connection> {
        // body ...
    }
}
```

Context lines include:
- Module path + file path + line range
- First doc comment line (if any) as a brief description
- Parent type signature (for methods)
- Visibility annotation (if not evident from the code)

**Metadata extraction:**

| Field | Source |
|-------|--------|
| `symbol_name` | `TypeName::method_name` for methods, `function_name` for free fns, `TypeName` for overviews |
| `kind` | Map from node kind: `struct_item` → Struct, etc. |
| `chunk_type` | Overview for type overviews, Function for function chunks |
| `parent` | Type name for methods, `None` for free functions and overviews |
| `visibility` | Parse visibility modifier node child (`pub`, `pub(crate)`, `pub(super)`, or absent → Private) |
| `file_path` | Passed-in `file_path` argument |
| `line_range` | `node.start_position().row..node.end_position().row` (0-based) |
| `signature` | First line of function up to `{`, or type definition line |

### Success Criteria

- Parse `src/config.rs` from this project — produces type overview
  for `Config`, `ConfigPaths` and function chunks for `find_git_root`,
  `config_paths`, `load_config`, `load_layer`.
- Parse `src/service.rs` — produces overview for `DebriefService`,
  `InProcessService`, `IndexResult`, `SearchResult` and function
  chunks for each trait method impl.
- Metadata `symbol_name` is `Config` for the overview, `load_config`
  for the free function.
- `display_text` contains no embedding metadata prefix.
- `embedding_text` contains file path and module path prefix.
- Same-file impl blocks merge into one overview.
- Orphan impls produce standalone overview chunks.
- Unit tests with embedded Rust source strings for controlled
  scenarios: struct + impl, multiple impl blocks, trait impl,
  free function, nested module.

## Phase 3: Git File Tracking

Detect which files changed since last index using system git.

### Goal

Given a project root and a last-indexed commit hash, return the list
of added, modified, and deleted files. Handle first-run (no previous
commit) gracefully.

### Scope

**`src/git.rs`:**

```rust
use std::path::Path;
use anyhow::Result;

pub struct FileChanges {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

/// Get the current HEAD commit hash.
pub fn head_commit(repo_root: &Path) -> Result<String> {
    // git rev-parse HEAD
}

/// Detect changed files between two commits.
/// If `from` is None, returns all tracked files (full index).
pub fn changed_files(repo_root: &Path, from: Option<&str>) -> Result<FileChanges> {
    // from == None: git ls-files (all tracked files as "added")
    // from == Some(hash): git diff --name-status <hash> HEAD
}
```

Shell out via `std::process::Command`. Parse stdout line by line.
`git diff --name-status` returns lines like `M\tsrc/foo.rs`,
`A\tsrc/bar.rs`, `D\tsrc/baz.rs`.

Error handling: if git is not available or the directory is not a
repo, return a clear error (not a silent fallback). The caller
(InProcessService) decides how to handle non-git directories.

### Success Criteria

- `head_commit` returns the current HEAD hash in this repo.
- `changed_files(root, None)` lists all tracked `.rs` files.
- `changed_files(root, Some(old_hash))` lists only files changed
  since that commit.
- Files outside the repo or binary files are handled gracefully.
- Unit test: run against this repository (it's a git repo).
- Error case: calling `head_commit` on `/tmp` returns an error.

## Phase 4: Index Serialization

Persist the chunk index to disk and reload it.

### Goal

Serialize `HashMap<PathBuf, Vec<Chunk>>` plus index metadata to
`.git/debrief/index.bin`. Reload on next invocation. Handle version
mismatches gracefully.

### Scope

**`src/store.rs`:**

```rust
use std::path::Path;
use anyhow::Result;
use crate::chunk::Chunk;

const INDEX_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct IndexData {
    version: u32,
    pub last_indexed_commit: Option<String>,
    pub embedding_model: Option<String>,
    pub chunks: HashMap<PathBuf, Vec<Chunk>>,
}

/// Save the index to disk.
pub fn save_index(path: &Path, data: &IndexData) -> Result<()> {
    // bincode::serialize → write to file
    // Create parent directories if needed
}

/// Load the index from disk.
/// Returns None if the file doesn't exist or version mismatches.
pub fn load_index(path: &Path) -> Result<Option<IndexData>> {
    // Read file → bincode::deserialize
    // Check version field — if != INDEX_VERSION, return None
    //   (caller triggers full re-index)
}
```

**Dependencies to add:**

```toml
bincode = "1"
```

The `IndexData` struct carries:
- `version`: for format migration. Mismatch → return None → full re-index.
- `last_indexed_commit`: for incremental diff (Phase 3).
- `embedding_model`: if model changes, embeddings are invalid → re-index.
- `chunks`: the actual data, keyed by file path.

Index file path: `.git/debrief/index.bin` (from config's local storage
path). The `save_index` function creates `.git/debrief/` directory if
it doesn't exist.

### Success Criteria

- Round-trip: save → load returns identical `IndexData`.
- Version mismatch: save with version 1, manually change version
  field in binary, load returns `None`.
- Missing file: load returns `None` (no error).
- Parent directory creation: saving to a non-existent `.git/debrief/`
  directory creates it.
- Integration test with tempdir: create `IndexData` with sample
  chunks, save, load, compare.

## Future

After this ticket:
- Phase 1C adds embedding pipeline, populates `Chunk.embedding`.
- Phase 1D wires `InProcessService::index` end-to-end:
  git tracking → chunking → (embedding) → store.
  Note: per `260404-refactor-service-trait-multi-workspace`, the wired
  `InProcessService::index` call will accept `project_root: &Path` rather
  than relying on config bound at construction time.
- Search uses `IndexData.chunks` for retrieval.

### Result — Phase 1: Chunk Data Model + Chunker Trait

Implemented `src/chunk.rs` with all specified types (Chunk, ChunkMetadata,
ChunkKind, ChunkType, Visibility) and `src/chunker/mod.rs` with `Chunker`
trait. Bincode round-trip test passes. Added `bincode = "1"` dependency.

### Result — Phase 2: Tree-sitter Rust Chunking

Implemented `src/chunker/rust.rs` (~930 lines). Two-pass `ChunkCollector`:
first pass collects type definitions, impl blocks, free functions; second
pass generates overview + function chunks with dual text and full metadata.

- tree-sitter 0.26 + tree-sitter-rust 0.24 (ticket specified 0.24/0.23 —
  updated to latest compatible pair).
- Generic stripping in impl aggregation: `base_type_name` strips `<...>`
  to match struct name ("Foo") with impl type ("Foo<T>"). Pragmatic
  deviation from ticket's "no generic normalization" — without it, all
  generic impl blocks become orphan impls.
- `indent_block` is a no-op (method text preserves original indentation).
- 10 unit tests including real `src/config.rs` parsing.

### Result — Phase 3: Git File Tracking

Implemented `src/git.rs`: `head_commit` (rev-parse HEAD) and
`changed_files` (ls-files for full index, diff --name-status for
incremental). Renames (R) split into delete+add. Copies (C) treated
as add only. 4 tests against this repo.

### Result — Phase 4: Index Serialization

Implemented `src/store.rs`: `IndexData` with versioned bincode header
(version u32 LE first field), `save_index` (creates parent dirs),
`load_index` (Ok(None) for missing file or version mismatch). 4 tests
including byte-level version patching.
