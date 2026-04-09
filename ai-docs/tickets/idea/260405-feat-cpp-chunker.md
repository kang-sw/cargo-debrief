---
title: "C++ Chunker — tree-sitter-based AST-aware chunking for C/C++"
category: feat
priority: high
related:
  - 260404-feat-rust-chunking-population  # parallel language expansion
---

# C++ Chunker

## Goal

Implement a `CppChunker` following the `Chunker` trait pattern established
by `RustChunker`. Enables RAG indexing of C/C++ codebases — the primary
target is production C++20 code (Visual Studio projects) with significant
macro usage.

## Motivation

The tool currently only indexes Rust. The user's production codebase is
C++-based, making C++ chunking a prerequisite for real-world utility.
tree-sitter-cpp parses both C and C++ (C is largely a subset), so a
single chunker covers both.

## Fundamental Limitation: C++ and tree-sitter

tree-sitter parses pre-preprocessor source. In C++ codebases with heavy
macro usage, this means:

- Macro invocations that expand to declarations appear as `preproc_call`
  nodes, not as struct/function/namespace nodes.
- `extern "C"` blocks, namespace declarations, and type definitions
  behind `#ifdef` gates are invisible or misrepresented.
- Macro expansion requires build system integration (include paths,
  defines, toolchain) — unrealistic for a standalone tool.

**Decision:** Accept tree-sitter's limitations. Best-effort parsing of
what's visible in the source text. This is pragmatic — even imperfect
chunking provides useful search context for RAG.

## Design Decisions

### Simplified chunk strategy

Focus on declaration/definition separation where tree-sitter can reliably
distinguish them:

| Level | What it captures |
|-------|-----------------|
| File-level skeleton | Struct/class/enum **declarations** (forward decls + type signatures), `#include` directives, top-level `#define` macros |
| Class/struct skeleton | Method **declarations** (signatures only), member variables, nested type declarations |
| Function body chunk | Implementation bodies exceeding `MIN_METHOD_CHUNK_LINES` — best-effort |

Small functions (≤ threshold) inlined into parent skeleton, same as Rust.
Implementation bodies in `.cpp` files are chunked as standalone functions
with best-effort class/namespace qualification from context.

### Namespace handling

Best-effort: extract namespace qualification from enclosing `namespace_definition`
nodes where tree-sitter sees them. Namespaces behind macro gates or
conditional compilation are invisible — accept this limitation.

### Preprocessor directives

- `#include` directives: include in file skeleton (useful search context).
- `#define` macros: include in file skeleton verbatim.
- `#ifdef`/`#ifndef` blocks: ignore the conditional structure; parse
  the visible branch as-is (tree-sitter's default behavior).
- Header guards (`#ifndef FOO_H` / `#pragma once`): skip — no information value.
- `extern "C"` blocks: skip (low ROI, often behind macro gates, cross-file).

### Header vs source files

Both `.h`/`.hpp` and `.c`/`.cpp`/`.cc`/`.cxx` files are indexed.
No header-vs-source linking — each file chunked independently.

### Template handling

Template declarations chunked like non-template counterparts. Template
parameter list included in signatures. No special treatment for
specializations.

### Anonymous structs/unions

Inline into parent type skeleton. No separate chunk — keep simple.

## Phases

### Phase 1: Core CppChunker

Implement `CppChunker` in `src/chunker/cpp.rs`:

- tree-sitter-cpp parser setup
- Two-pass design mirroring RustChunker:
  - Pass 1: collect top-level items (classes, structs, enums, unions,
    free functions, global variables, typedefs, type aliases, namespace
    contents, preprocessor directives)
  - Pass 2: generate Overview and Function chunks
- File-level skeleton (global scope overview chunk)
- Class/struct overview chunks with member signatures
- Function body chunks for large functions/methods
- Namespace-qualified `symbol_name` and `embedding_text`
- Macro nodes included in file skeleton
- Unit tests with representative C++ samples

### Phase 2: Integration and validation

- Wire CppChunker into service dispatch (file extension → chunker selection)
- Integration tests with real C++ code samples
- Validate search quality on ASIO headers/source (template-heavy C++)
- Edge cases: anonymous namespaces, nested class definitions, enum class

## Resolved Questions

- **`extern "C"` blocks** → Skip. Low ROI, often behind macro gates,
  cross-file scope makes it unreliable.
- **Header guards** → Skip. No information value for search.
- **Anonymous structs/unions** → Inline into parent. No separate chunk.
- **Macro expansion** → Not feasible. Requires build system integration.
  Accept tree-sitter's pre-preprocessor view as-is.

## Validation Target

ASIO (Boost.Asio or standalone asio) headers and source — representative
of template-heavy, well-structured C++ with moderate macro usage. Success
means meaningful chunks that support RAG search over this codebase.
