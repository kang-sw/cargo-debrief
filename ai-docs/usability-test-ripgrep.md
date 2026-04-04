# Usability Test — ripgrep

**Date:** 2026-04-04  
**Tester:** worker agent  
**Tool version:** cargo-debrief (branch `marathon/post-mvp`, commit `e8e1a5f`)  
**Target repo:** ripgrep commit `4519153` (Feb 27, 2026)

---

## Setup

- ripgrep cloned to `test-repos/ripgrep/` relative to cargo-debrief project root
- Binary: `target/release/cargo-debrief` (release build)
- Embedding model: `nomic-embed-text-v1.5` (526 MB cached at `~/Library/Application Support/debrief/models/`)
- System: macOS Darwin 25.3.0, 128 GB RAM

---

## Critical Finding: Full Indexing Fails (SIGKILL)

**cargo-debrief cannot index the full ripgrep repository.**

### Symptoms

- `rebuild-index` on ripgrep runs 2–5 minutes then exits with code 137 (SIGKILL)
- No stdout or stderr output produced before kill
- No partial index written (index is saved only after full batch embedding completes)
- Reproduced 4 times across debug and release builds

### Root Cause

ripgrep has 100 Rust files and 52,266 lines of Rust. With the chunker producing
approximately 266 chunks per file on average (observed from the 5-file subset),
this yields an estimated **~26,600 chunks total**. `run_index` in `service.rs`
embeds all chunks in a **single batch call** (`embed_batch(&text_refs)`). The
resulting ONNX inference tensors are:

| Tensor | Shape | Size (f32) |
|--------|-------|------------|
| `input_ids` | 26600 × 512 × i64 | ~109 MB |
| `attention_mask` | 26600 × 512 × i64 | ~109 MB |
| `last_hidden_state` | 26600 × 512 × 768 × f32 | ~26 GB |

The hidden state tensor alone requires approximately 26 GB, plus ONNX Runtime
working memory. Although the system has ~117 GB free physical pages, the macOS
Jetsam daemon kills the process before the allocation or inference completes.

### Workaround Used

A 5-file subset of ripgrep's key files was indexed successfully (1332 chunks,
5.5 MB index). All remaining evaluations use this subset.

Files included in the subset:
- `crates/core/flags/defs.rs` (7779 lines)
- `crates/printer/src/standard.rs` (3987 lines)
- `crates/ignore/src/gitignore.rs` (849 lines)
- `crates/searcher/src/mod.rs` (1088 lines)
- `crates/regex/src/literal.rs` (1016 lines)

---

## Search Quality Evaluation

All queries run from the ripgrep subset directory. Scoring: **relevance 0–3**
= how many of top-3 results are genuinely relevant to the query intent.

### Query Results

#### 1. `regex pattern matching`

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.7250 | `crates/regex/src/literal.rs:644` | `tests::e()` — parses a regex pattern in tests |
| 2 | 0.6624 | `crates/core/flags/defs.rs:259` | `AutoHybridRegex::doc_long()` — hybrid regex engine docs |
| 3 | 0.6341 | `crates/core/flags/defs.rs:5781` | `RegexSizeLimit::doc_long()` — compiled regex size limit |
| 4 | 0.6256 | `crates/core/flags/defs.rs:5864` | `Regexp::doc_long()` — -e/--regexp flag docs |
| 5 | 0.6207 | `crates/core/flags/defs.rs:4146` | `Multiline::doc_long()` — multiline mode docs |

**Relevance top-3: 2/3.** Result #1 (test helper for parsing regex patterns) is
on-topic. #2 is relevant as it explains regex engine selection. #3 is marginally
relevant (discusses compiled regex). Missing: actual regex compilation/matching
code from `crates/regex/src/`.

---

#### 2. `command line argument parsing`

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.5741 | `crates/core/flags/defs.rs:3718` | `LineRegexp::name_long()` — returns `"line-regexp"` |
| 2 | 0.5704 | `crates/core/flags/defs.rs:1143` | `ContextSeparator::name_long()` |
| 3 | 0.5616 | `crates/core/flags/defs.rs:1158` | `ContextSeparator::doc_long()` |
| 4 | 0.5608 | `crates/core/flags/defs.rs:1015` | `Context::doc_short()` |
| 5 | 0.5605 | `crates/core/flags/defs.rs:1800` | `FieldContextSeparator::doc_long()` |

**Relevance top-3: 0/3.** All results are isolated `name_long()` or `doc_long()`
single-method impls for individual flags. None represent the argument parsing
structure itself (the `Flag` trait, the dispatch table, or a CLI entry point).
The chunker creates individual chunks per impl block, which here means micro-chunks
of 2–3 lines with very low information density. The overview chunk for `Flag`
trait was not retrieved.

**Notable miss:** The `Flag` trait definition, the parser dispatch, and any
struct like `HiArgs` that collects parsed flags into a cohesive type.

---

#### 3. `file searching and filtering`

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.6501 | `crates/searcher/src/mod.rs:664` | `Searcher::search_file()` |
| 2 | 0.6382 | `crates/searcher/src/mod.rs:642` | `Searcher::search_path()` |
| 3 | 0.6352 | `crates/core/flags/defs.rs:6082` | `SearchZip::doc_long()` — searching compressed files |
| 4 | 0.6187 | `crates/core/flags/defs.rs:444` | `Binary::doc_long()` — binary file filtering |
| 5 | 0.6095 | `crates/core/flags/defs.rs:7227` | `Unrestricted::doc_long()` — filter level control |

**Relevance top-3: 3/3.** `search_file` and `search_path` are the primary entry
points for file-level searching — highly relevant. `SearchZip` docs are on-topic
as a filtering variant. Binary file filtering (#4) is also directly relevant.

---

#### 4. `ignore rules and gitignore`

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.7626 | `crates/ignore/src/gitignore.rs:80` | `Gitignore` struct overview (all methods) |
| 2 | 0.7067 | `crates/ignore/src/gitignore.rs:170` | `Gitignore::num_ignores()` |
| 3 | 0.7047 | `crates/ignore/src/gitignore.rs:102` | `Gitignore::new()` |
| 4 | 0.6984 | `crates/ignore/src/gitignore.rs:557` | `GitignoreBuilder::allow_unclosed_class()` |
| 5 | 0.6962 | `crates/ignore/src/gitignore.rs:830` | test: `case_insensitive` |

**Relevance top-3: 3/3.** All top results are directly on-topic. #1 retrieves
the full type skeleton with all methods, which is the ideal overview result for
an LLM. High confidence scores (0.70+) across the board.

---

#### 5. `printer output formatting`

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.5677 | `crates/printer/src/standard.rs:1502` | `StandardImpl::start_line_highlight()` |
| 2 | 0.5663 | `crates/printer/src/standard.rs:1771` | test helper `printer_contents()` |
| 3 | 0.5633 | `crates/printer/src/standard.rs:1511` | `StandardImpl::end_line_highlight()` |
| 4 | 0.5568 | `crates/printer/src/standard.rs:1467` | `StandardImpl::end_hyperlink()` |
| 5 | 0.5460 | `crates/printer/src/standard.rs:1483` | `StandardImpl::end_color_match()` |

**Relevance top-3: 2/3.** Results are from the right file (`standard.rs`) but are
low-level color/highlight helpers rather than the top-level printing API. The
`Standard<W>` struct overview chunk and `print_matches` / `sink_match` entry points
were not retrieved. Scores are relatively low (0.56 range), suggesting weak
semantic alignment.

---

#### 6. `parallel directory walking`

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.5352 | `crates/core/flags/defs.rs:5093` | `OneFileSystem::doc_short()` |
| 2 | 0.5328 | `crates/core/flags/defs.rs:5096` | `OneFileSystem::doc_long()` |
| 3 | 0.5324 | `crates/core/flags/defs.rs:5191` | `PathSeparator::name_long()` |
| 4 | 0.5261 | `crates/core/flags/defs.rs:5200` | `PathSeparator::doc_short()` |
| 5 | 0.5243 | `crates/core/flags/defs.rs:2361` | `Follow::doc_long()` — follow symlinks |

**Relevance top-3: 0/3.** The actual parallel walking code lives in
`crates/ignore/src/walk.rs` (2494 lines), which was not in the indexed subset.
All retrieved results are flag documentation tangentially related to directory
traversal. This is a subset coverage gap rather than a search quality failure per
se, but it demonstrates that excluding key files from the index makes relevant
queries completely fail.

**Notable miss:** `WalkBuilder`, `WalkParallel`, `DirEntry` in `walk.rs`.

---

#### 7. `Searcher` (exact symbol lookup)

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.9043 | `crates/searcher/src/mod.rs:596` | `Searcher` struct full overview |
| 2 | 0.7593 | `crates/searcher/src/mod.rs:664` | `Searcher::search_file()` |
| 3 | 0.7541 | `crates/searcher/src/mod.rs:631` | `Searcher::new()` |
| 4 | 0.7414 | `crates/searcher/src/mod.rs:642` | `Searcher::search_path()` |
| 5 | 0.7405 | `crates/searcher/src/mod.rs:849` | `Searcher::invert_match()` |

**Relevance top-3: 3/3.** Excellent. Score 0.9043 for the struct overview is the
highest single score observed. The symbol-name boosting in `SearchIndex` is clearly
working — exact type name in the query pulls the overview chunk to the top.
All top-5 results are directly about `Searcher`.

---

#### 8. `BufReader` (exact symbol lookup — not in codebase)

| # | Score | File | Symbol |
|---|-------|------|--------|
| 1 | 0.4899 | `crates/searcher/src/mod.rs:664` | `Searcher::search_file()` |
| 2 | 0.4814 | `crates/searcher/src/mod.rs:726` | `Searcher::search_reader()` — shows LineBufferReader pattern |
| 3 | 0.4812 | `crates/searcher/src/mod.rs:1072` | test: UTF-8 BOM sniffing |
| 4 | 0.4781 | `crates/searcher/src/mod.rs:797` | `Searcher::set_binary_detection()` |
| 5 | 0.4772 | `crates/searcher/src/mod.rs:642` | `Searcher::search_path()` |

**Relevance top-3: 1/3.** `BufReader` is a standard library type not defined in
ripgrep, so a miss is expected. #2 (`search_reader`) is the closest result —
it shows how ripgrep wraps a reader via `LineBufferReader` and `DecodeReaderBytes`.
Scores are low (0.48–0.49), reflecting weak match. The tool correctly falls back
to semantically adjacent content rather than returning nothing.

---

## Overview Test

Tested `overview` on 4 files:

### `crates/ignore/src/gitignore.rs` — Excellent

Shows all 3 public types (`Gitignore`, `GitignoreBuilder`, `Glob`) with complete
public method signatures, field names on structs, and trait implementations. An
LLM receiving this overview can understand the full API without reading the source.

### `crates/searcher/src/mod.rs` — Very Good

Renders `BinaryDetection`, `Config` (with inline field doc comments), `ConfigError`
variants, `Encoding`, `Searcher` with all public and private methods listed.
The inclusion of field-level doc comments is valuable context for an LLM. Comprehensive.

### `crates/printer/src/standard.rs` — Good, but Verbose

Shows private structs `Config`, `PreludeSeparator`, `PreludeWriter` before the
public `Standard<W>` struct. The overview is technically complete but front-loaded
with private implementation details. For a 3987-line file this produces a lengthy
output. The public API surface is buried below internal types.

### `crates/regex/src/literal.rs` — Good

Shows `Extractor`, `InnerLiterals`, `TSeq` with method lists. The internal
types are the actual substance of this file, so showing them is correct.

---

## Scale Metrics (5-file Subset)

| Metric | Value |
|--------|-------|
| Files indexed | 5 |
| Total Rust lines | 14,719 |
| Chunks produced | 1,332 |
| Avg chunks per file | 266 |
| Index file size | 5.5 MB |
| Estimated KB per chunk | ~4.2 KB |
| Search latency (incl. model load) | ~1.0 s |

### Projected for Full ripgrep (100 files, 52,266 lines)

| Metric | Projection |
|--------|------------|
| Expected chunks | ~26,600 |
| Expected index size | ~110 MB |
| Single-batch tensor (hidden state) | ~26 GB |
| **Status** | **FAILS — SIGKILL during embedding** |

---

## Summary Quality Assessment

| Dimension | Rating | Notes |
|-----------|--------|-------|
| Exact symbol lookup | Excellent | "Searcher" scores 0.90; boosting works |
| Semantic search (specific topic) | Good | "ignore rules" scores 0.76; "file searching" scores 0.65 |
| Semantic search (structural) | Poor | "argument parsing" returns micro-chunks; no structural retrieval |
| Overview completeness | Good–Excellent | Full API per file; verbose on large files |
| Scale: 100-file repo | Broken | Single-batch embedding causes SIGKILL |
| Incremental indexing | Not tested | Full indexing failed before incremental was relevant |

**Overall: Promising on small codebases, unusable on medium+ repos.**

---

## Identified Gaps and Improvement Suggestions

### P0 — Batch Splitting (Blocker)

`run_index` (service.rs:207) embeds all chunks in a single `embed_batch` call.
For any repo with more than ~2,000 chunks (roughly 10–15 average Rust files),
this allocates multi-GB tensors and gets killed. **The tool must split embedding
into batches of ≤ 64 or ≤ 128 chunks.**

Additionally, no partial progress is saved. If the process dies, the entire
index must be rebuilt from scratch. Batched embedding enables incremental saves.

### P1 — Chunker Granularity for Flag-Heavy Codebases

`defs.rs` (7779 lines) generates hundreds of micro-chunks — one per `impl Flag for X`
method. Single-method impls like `fn name_long() -> &'static str { "foo" }` produce
near-duplicate chunks that crowd search results. For "command line argument parsing"
the top-5 results were entirely these micro-impls.

Fix options:
1. Merge same-struct consecutive impl blocks into a single level-1 chunk.
2. De-duplicate near-identical short impls (name accessors) at index time.
3. Increase the minimum chunk size threshold.

### P2 — Overview Ordering (Private Before Public)

`standard.rs` overview shows private structs (`Config`, `PreludeWriter`) before
the public API (`Standard<W>`). An LLM trying to understand the file's public
interface has to read past internal details first. The overview renderer should
sort: public types first, then private.

### P3 — No Progress Feedback During Indexing

The binary produces no output while indexing. For a slow operation (2+ minutes
even when it would succeed), the user has no signal that anything is happening.
A progress bar or periodic log line (e.g., "chunking file 45/100") would prevent
users from thinking the tool has hung.

### P4 — Score Calibration

Scores for "printer output formatting" cluster at 0.54–0.57, which is low but
retrieves correct files. "Command line argument parsing" scores 0.55–0.57 but
retrieves wrong chunks. The user cannot distinguish "low confidence, right area"
from "low confidence, wrong area" from the score alone. A confidence threshold
or normalized scoring would help.
