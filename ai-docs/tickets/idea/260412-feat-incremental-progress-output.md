---
title: "Add progress output to incremental re-index path"
---

# Add progress output to incremental re-index path

## Background

Full rebuild (`rebuild-index`) emits `[discovery]`, `[chunking]`, and `[embedding]` progress lines. The incremental path (`apply_incremental_updates` in `service.rs`) is silent — no indication of which files are being re-indexed or how many. First observed in smoke test 2026-04-12.

## Phases

### Phase 1: Emit progress from apply_incremental_updates

Add progress output to `apply_incremental_updates`: emit a summary line like `[incremental] N files changed (M added, K removed), re-indexing...` at the start of the operation, then reuse the existing `[chunking]` and `[embedding]` progress lines for the re-indexed files. The output contract should match the style of `rebuild-index` so callers and log parsers see consistent format.
