---
title: "Fix usability test findings — batch embedding, chunker quality, UX"
category: bug
priority: critical
parent: null
started: 2026-04-04
plans: null
related:
  - 260404-idea-usability-test-repos  # test that produced these findings
  - 260404-feat-rust-chunking-population  # P1 overlaps with chunking improvements
  - 260404-feat-dependency-chunking  # GPU pre-req shared
---

# Fix Usability Test Findings

Results from ripgrep usability test (`ai-docs/usability-test-ripgrep.md`).
Full ripgrep (100 files, ~26K chunks) triggers SIGKILL during embedding.
5-file subset shows 58% top-3 relevance — symbol lookup excellent,
structural semantic queries weak (embedding model limitation, not
actionable here).

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

## Phase 2 — P1: Micro-Chunk Merging

Flag-heavy files (e.g., ripgrep's `defs.rs`, 7.8K lines) produce
hundreds of 2-3 line chunks — one per `impl Flag for X` single-method
block. These crowd search results with low-information content.

**Approach options (evaluate at plan time):**

1. **Minimum chunk size threshold** — skip or merge chunks below N
   lines into the parent overview chunk.
2. **Same-type consecutive impl merging** — if multiple `impl T`
   blocks for the same type are adjacent, merge into one function chunk.
3. **De-duplicate near-identical short methods** — detect accessor
   patterns (`fn name_long() -> &str { "foo" }`) and collapse.

Option 1 is simplest and addresses the symptom directly.

### Success criteria

- `defs.rs` produces fewer than 50 chunks (currently ~266 per file avg)
- "command line argument parsing" query returns structural results
  (Flag trait overview, HiArgs) not micro-impl accessors

## Phase 3 — P2 + P3: Overview Ordering + Progress Feedback

**P2: Overview ordering.** `overview` output shows private types before
public API. Sort: `pub` items first, then `pub(crate)`, then private.
Change is in the overview rendering path, not the chunker.

**P3: Progress feedback.** Add minimal, LLM-friendly progress output.
No CLI animation (progress bars, spinners). Print `indexing` once,
then append a `.` every ~3 seconds (no newline), then newline + done:

```
indexing..........
done. 1332 chunks, 100 files.
```

Single growing line via `eprint!(".")` + `flush()`.
Output to stderr to keep stdout clean for piping.

### Success criteria

- `overview src/standard.rs` shows `Standard<W>` before `Config`
- `rebuild-index` on ripgrep shows per-file or per-batch progress

## Known Limitations (Not Addressed)

**Structural semantic queries** (e.g., "how does argument parsing work")
perform poorly. This is a fundamental limitation of small embedding
models (137M-335M params) which embed tokens, not architectural concepts.
Potential future mitigations:
- BM25 hybrid search (keyword matching)
- LLM-generated chunk summaries at index time
- Larger code-specialized embedding models

Documented as a known constraint, not a bug.
