# Chunker — Mental Model

## Entry Points

- `src/chunker/mod.rs` — `Chunker` trait (the extension point)
- `src/chunker/rust.rs` — `RustChunker`: tree-sitter walk, `ChunkCollector`, chunk generation

## Module Contracts

- `Chunker::chunk` takes a `file_path` and source `&str`; returns `Vec<Chunk>` with `embedding` always `None` — embeddings are filled downstream, never by the chunker.
- `RustChunker` produces up to three chunk types per file:
  - `ChunkType::Overview` with `kind: ChunkKind::Struct/Enum/Trait/Impl` — one per named type/impl group.
  - `ChunkType::Overview` with `kind: ChunkKind::Module` — one per file that has any free functions; aggregates all free functions for that file. Produced only when free functions exist.
  - `ChunkType::Function` — only for methods or free functions **exceeding** `MIN_METHOD_CHUNK_LINES` (currently 5). Functions at or below the threshold are inlined into their parent overview chunk (full body, not signature) and produce no separate chunk.
- Small methods (≤5 lines) appear in the type overview as full body text. Large methods (>5 lines) appear as signature-only in the overview and also get a standalone `ChunkType::Function` chunk.
- Small free functions (≤5 lines) are inlined fully into the module overview chunk; large free functions appear as signature-only in module overview and also get a standalone `ChunkType::Function` chunk.
- Impl blocks are aggregated by the **bare type name** (generics stripped). An `impl Display for Foo` and `impl Foo` both merge under `"Foo"`. An `impl ExternalType` with no corresponding struct/enum definition in the same file produces an overview chunk with `kind: ChunkKind::Impl` (not `Struct`/`Enum`) — this is the orphan-impl case.

## Coupling

- `RustChunker` takes `file_path: &Path` **relative to the crate root** (e.g. `src/foo.rs`). `derive_module_path` strips the `src/` prefix to build the Rust module path. Passing an absolute path silently produces a wrong `crate::` module name in `embedding_text` with no error.
- `chunk::Chunk` is the output contract. Adding fields to `Chunk` requires updating both `build_overview_chunk` and `build_method_chunk`/`build_free_function_chunk` in `rust.rs`.
- All chunks emitted by `RustChunker` carry `origin: ChunkOrigin::Project` (the `Default`). A separate code path in `run_deps_index` (see `service.rs`) handles dependency sources and sets `ChunkOrigin::Dependency`; do not set it inside `RustChunker` directly.

## Extension Points & Change Recipes

**Adding a new language chunker:**

1. Create `src/chunker/<lang>.rs` implementing `Chunker`.
2. `pub use` it from `src/chunker/mod.rs`.
3. Declare the module in `src/lib.rs`.

**Adding a new top-level Rust item type (e.g. `type` alias, `const`):**

1. Add an arm in `ChunkCollector::collect_top_level` and `collect_inline_module_body`.
2. Add the corresponding `build_*_chunk` method.
3. Extend `ChunkKind` in `src/chunk.rs` and bump `store::INDEX_VERSION`.
4. Decide whether the new item is subject to the `MIN_METHOD_CHUNK_LINES` merge threshold or always gets its own chunk.

**Two-pass design — must handle new items in both passes:**

- Pass 1: `collect_top_level` → populates `types` and `free_functions`.
- Pass 2: `into_chunks` → calls `build_*_chunk` for each collected item.
- Forgetting pass 1 = item silently skipped. Forgetting pass 2 = item collected but never emitted.

## Common Mistakes

- **Absolute file path to `chunk()`** — `derive_module_path` silently produces a wrong module name because the `src/` strip fails. Always pass a path relative to the crate root.
- **Expecting `embedding` to be filled** — `Chunk::embedding` is always `None` out of `RustChunker`. Callers that check for a populated embedding immediately after chunking will see `None`.
- **Orphan impl kind** — a type that appears only in `impl` blocks (no `struct`/`enum` def in file) gets `kind: Impl`, not its semantic kind. Filtering chunks by `ChunkKind::Struct` will miss orphan impls.
- **Expecting one chunk per free function** — free functions at or below 5 lines produce no `ChunkType::Function` chunk. They appear only inside the `ChunkKind::Module` overview chunk for the file. Code that searches for a free function by `symbol_name` equal to the function name will find `None`; the module overview `symbol_name` is the module's leaf name, not the function name.
- **Counting total chunks to predict index size** — chunk count is no longer `(types + methods + free_fns)`; small items are merged. Tests or estimates based on the old 1:1 correspondence will be wrong.
