---
title: "Rust chunking population improvement — additional node kinds and quality"
category: feat
priority: medium
related:
  260404-feat-core-indexing-pipeline: MVP chunking baseline
  260403-research-rag-architecture:
  260404-feat-dependency-chunking: dep indexing amplifies population gaps
  260404-idea-usability-test-repos: test results inform priority of each node kind
---

# Rust Chunking Population Improvement

Phase 1B (`260404-feat-core-indexing-pipeline`) implements chunking for
the core node kinds: struct, enum, trait, impl, function, module. This
ticket tracks additional node kinds and quality improvements deferred
from the MVP.

## Deferred Node Kinds

These are valid chunking targets skipped in the MVP baseline:

| Node kind | Why it matters for RAG |
|-----------|----------------------|
| `const_item` | Configuration values, magic numbers, capacity limits — frequently searched |
| `static_item` | Global state, lazy-initialized singletons |
| `type_item` (type alias) | Essential for understanding API signatures; callers search by alias name |
| `macro_definition` (`macro_rules!`) | Significant code surface in many Rust crates; generates types/functions |
| `union_item` | Structurally identical to struct; rare but exists in FFI-heavy code |
| `extern_block` / `foreign_mod` | FFI declarations — relevant for C interop crates |

Each should produce either an overview chunk (for type-like items) or a
standalone chunk (for const/static/macro), following the same dual text
and metadata conventions as the MVP chunker.

## Quality Improvements

| Item | Description |
|------|-------------|
| Attribute preservation | Include `#[derive(...)]`, `#[cfg(...)]`, doc comments in chunk text — currently implicit |
| Large function splitting | Split functions exceeding a code-line threshold (excluding comments) into logical sub-chunks |
| `use` statement context | Leverage `use` declarations to resolve abbreviated paths in embedding_text |
| Proc-macro awareness | Handle proc-macro crate patterns (attribute macros, derive macros) |
| Cross-file skeleton assembly | Merge impl blocks across files for a complete type overview (conservative same-file only in MVP) |

## Priority

Medium — sequenced after usability testing (A) and dependency chunking (C)
in the post-MVP roadmap. Usability test results on ripgrep will inform
which node kinds and quality improvements have the highest real-world
impact. Dependency chunking amplifies population gaps because missing
node kinds in deps reduce the value of dep indexing.
