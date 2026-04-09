---
title: "Fix usability test findings — batch embedding, chunker quality, UX"
category: bug
priority: critical
started: 2026-04-04
related:
  - 260404-idea-usability-test-repos  # test that produced these findings
  - 260404-feat-rust-chunking-population  # P1 overlaps with chunking improvements
  - 260404-feat-dependency-chunking  # GPU pre-req shared
  - 260404-feat-llm-chunk-summarization  # structural query mitigation
---

# Fix Usability Test Findings

Results from ripgrep usability test (`ai-docs/usability-test-ripgrep.md`).
Full ripgrep (100 files, 3070 chunks) triggered SIGKILL during embedding
before P0 fix. 5-file subset showed 58% top-3 relevance; full-repo
post-fix shows 62.5%. Symbol lookup excellent, structural semantic
queries weak (micro-chunk dilution + embedding model limitation).

## Phase 1 — P0: Batch Embedding Split + GPU Acceleration

**Blocker.** `run_index` in `service.rs` passes all chunks to
`embed_batch` in a single call. For ~26K chunks the ONNX hidden-state
tensor is ~26 GB → process killed.

**Fix:** Split `embed_batch` calls into batches of 64-128 chunks.
Loop in `run_index`, accumulate embeddings. Optionally save partial
progress (index after each batch) to survive crashes.

**GPU acceleration** (same phase, small scope): Add execution provider
priority to `Embedder::load` session builder — CoreML (macOS), CUDA
(Linux/Windows), CPU fallback. Feature-flag gated (`gpu`, `cuda` in
Cargo.toml). This reduces indexing time for the larger batch counts.

### Success criteria

- `rebuild-index` completes on full ripgrep (100 files, ~26K chunks)
- Memory usage stays bounded (~500MB peak, not 26GB)
- GPU provider used when available (verify via log or env var)

### Result (b58d6f8) — 26-04-04

Phase 1 implemented: batch split (64 chunks per `embed_batch` call) +
GPU execution provider registration (CoreML/CUDA behind feature flags)
+ progress dots to stderr.

Full ripgrep CPU result: **3070 chunks, 100 files, 9m37s** — no crash.
Previous chunk estimate (~26K) was wrong — based on 5-file subset of
largest files. Real average is ~30 chunks/file, not 266.

Search quality: 15/24 (62.5%) vs baseline 14/24 (58%).
R1, R2 improved (more files indexed). S1, T3 regressed (micro-chunk
dilution — addressed by Phase 2).

**GPU/CoreML memory issue discovered:** `cargo-debrief-gpu rebuild-index`
on full ripgrep consumed 41 GB RSS before being killed. The CoreML
execution provider appears to accumulate memory across batches rather
than releasing intermediate state. CPU-only path unaffected.
`gpu` feature flag should carry a warning until this is investigated.
Possible causes: ort 2.0-rc CoreML binding leak, CoreML compilation
cache, or macOS unified memory accounting.
GPU/CoreML: SIGTERM at 4m25s with "Context leak detected" warnings.
CoreML EP not releasing Neural Engine context between batches.
Theoretical 2.17x speedup if stable, but unusable as-is.

## Phase 2 — P1: Micro-Chunk Merging

Full-repo eval (3070 chunks, 100 files) confirmed micro-chunk problem:
S1 "Searcher" regressed from 0.90 to 0.77 (overview diluted by method
chunks), T3 "printer output formatting" failed 0/3 (same cause).
Flag-heavy files remain the primary offender.

**Chosen approach: minimum body threshold with overview inlining.**

`const MIN_METHOD_CHUNK_LINES: usize = 5;`

Methods ≤ threshold: full body inlined in type overview chunk, no
separate function chunk generated. Methods > threshold: signature in
overview, separate function chunk (current behavior).

Changes in `chunker/rust.rs`:

1. `build_overview_chunk()` — for small methods, use full text instead
   of `signature_without_body()` in the overview's impl block rendering.
2. `into_chunks()` — skip `build_method_chunk()` for methods below
   threshold.

**Module overview chunk (new).** Free functions currently have no
parent to merge into. Add a per-file module overview chunk:

- Aggregates small free functions (≤ threshold) as full bodies
- Large free functions as signatures only (function chunk still generated)
- `symbol_name`: module path (e.g., `defs`)
- `kind`: `ChunkKind::Module` (new variant)
- `chunk_type`: `ChunkType::Overview`
- `parent`: `None`

This also creates a searchable module-level entry that describes
"what's in this file" — improves structural query matching.

**Chunk model change:** Add `ChunkKind::Module` variant to `chunk.rs`.
Bump `INDEX_VERSION`.

### Success criteria

- `defs.rs` produces fewer than 50 chunks (currently ~266 per file avg)
- "Searcher" query top-1 score recovers to > 0.85
- "printer output formatting" returns `Standard<W>` in top-3
- "command line argument parsing" returns structural results
  (Flag trait overview, HiArgs) not micro-impl accessors

### Result (941a1d8) — 26-04-04

Phase 2 implemented: methods ≤5 lines inlined (full body) into type
overview chunks instead of separate function chunks. Free functions
aggregated into new module overview chunk (`ChunkKind::Module`).
`INDEX_VERSION` bumped to 3. All tests pass (37 unit + 8 integration).

**Success criteria evaluation pending** — implementation complete but
post-merge search quality eval against ripgrep not yet run. Expected:
significant chunk count reduction in flag-heavy files (e.g., `defs.rs`)
and improved overview chunk ranking.

## Phase 3 — P2: Overview Ordering + Search Output UX

**P3 (progress feedback) completed in Phase 1** — `eprint!(".")`
dots per batch + summary line. No further work needed.

**P2: Overview ordering.** `overview` output shows private types before
public API. Sort: `pub` items first, then `pub(crate)`, then private.
Change is in the overview rendering path, not the chunker.

**P4: Search output module context line.** Add a `// in crate::module`
line to search results for module path context. Data already exists in
`embedding_text`; expose in `display_text` output. Informed by
cargo-brief output format comparison.

### Success criteria

- `overview src/standard.rs` shows `Standard<W>` before `Config`
- Search results include module path context line

### Result (3151029) — 26-04-04

Phase 3 implemented across two sub-tasks:

**P2 (overview ordering):** Overview chunks sorted by visibility —
`Pub` → `PubCrate` → `PubSuper` → `Private`. Stable sort preserves
file order within each tier.

**P4 (search module context):** `SearchResult` gains `module_path`
field, populated by `extract_module_path()` parsing `embedding_text`
first line. CLI prints `// in {module}` before each result's
display_text. Code review caught wrong `strip_prefix` (`"/ "` vs
`"// "`) — fixed before merge (8178e03). 3 unit tests added for
`extract_module_path`.

**Success criteria evaluation pending** — requires ripgrep test run.

## Known Limitations (Not Addressed)

**Structural semantic queries** (e.g., "how does argument parsing work")
perform poorly. This is a fundamental limitation of small embedding
models (137M-335M params) which embed tokens, not architectural concepts.
Potential future mitigations:
- LLM-generated chunk summaries at index time (`260404-feat-llm-chunk-summarization`)
- Larger code-specialized embedding models

**CoreML GPU acceleration** causes excessive memory usage (41 GB for
26K chunks). The `gpu` feature flag is not safe for production use
until the ort CoreML EP memory behavior is investigated.

Documented as known constraints, not bugs.
