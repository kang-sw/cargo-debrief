# Chunker — Mental Model

## Entry Points

- `src/chunker/mod.rs` — `Chunker` trait (the extension point)
- `src/chunker/rust.rs` — `RustChunker`: tree-sitter walk, `ChunkCollector`, chunk generation

## Module Contracts

- `Chunker::chunk` takes a `file_path` and source `&str`; returns `Vec<Chunk>` with `embedding` always `None` — embeddings are filled downstream, never by the chunker.
- `RustChunker` produces exactly two chunk types: `ChunkType::Overview` (one per named type/impl group) and `ChunkType::Function` (one per method or free function).
- Impl blocks are aggregated by the **bare type name** (generics stripped). An `impl Display for Foo` and `impl Foo` both merge under `"Foo"`. An `impl ExternalType` with no corresponding struct/enum definition in the same file produces an overview chunk with `kind: ChunkKind::Impl` (not `Struct`/`Enum`) — this is the orphan-impl case.

## Coupling

- `RustChunker` takes `file_path: &Path` **relative to the crate root** (e.g. `src/foo.rs`). `derive_module_path` strips the `src/` prefix to build the Rust module path. Passing an absolute path silently produces a wrong `crate::` module name in `embedding_text` with no error.
- `chunk::Chunk` is the output contract. Adding fields to `Chunk` requires updating both `build_overview_chunk` and `build_method_chunk`/`build_free_function_chunk` in `rust.rs`.

## Extension Points & Change Recipes

**Adding a new language chunker:**

1. Create `src/chunker/<lang>.rs` implementing `Chunker`.
2. `pub use` it from `src/chunker/mod.rs`.
3. Declare the module in `src/lib.rs`.

**Adding a new top-level Rust item type (e.g. `type` alias, `const`):**

1. Add an arm in `ChunkCollector::collect_top_level` and `collect_inline_module_body`.
2. Add the corresponding `build_*_chunk` method.
3. Extend `ChunkKind` in `src/chunk.rs` and bump `store::INDEX_VERSION`.

**Two-pass design — must handle new items in both passes:**

- Pass 1: `collect_top_level` → populates `types` and `free_functions`.
- Pass 2: `into_chunks` → calls `build_*_chunk` for each collected item.
- Forgetting pass 1 = item silently skipped. Forgetting pass 2 = item collected but never emitted.

## Common Mistakes

- **Absolute file path to `chunk()`** — `derive_module_path` silently produces a wrong module name because the `src/` strip fails. Always pass a path relative to the crate root.
- **Expecting `embedding` to be filled** — `Chunk::embedding` is always `None` out of `RustChunker`. Callers that check for a populated embedding immediately after chunking will see `None`.
- **Orphan impl kind** — a type that appears only in `impl` blocks (no `struct`/`enum` def in file) gets `kind: Impl`, not its semantic kind. Filtering chunks by `ChunkKind::Struct` will miss orphan impls.
