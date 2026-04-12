---
title: "Investigate search score saturation — all results return score 1.0"
---

# Investigate search score saturation — all results return score 1.0

## Background

Smoke test (2026-04-12) on C++ repos (asio, fmtlib, nlohmann-json — 6412 chunks) showed all search results returning score 1.0 regardless of query relevance. It is unclear whether this is a `SearchIndex` normalization bug or expected behavior when all indexed content is in a tight embedding cluster (C++ code).

## Phases

### Phase 1: Reproduce

Run `cargo run -- search "template specialization"` against a C++-only config and verify the score distribution. Then add Rust source (mixed content) to the index and check whether scores vary. Goal: determine whether saturation is content-dependent or always present.

### Phase 2: Investigate normalization

If the bug is confirmed, inspect `SearchIndex::search_by_vector` score computation. `hnsw_rs` raw cosine similarity returns values in [-1, 1]. Check whether the +1 normalization offset or the metadata boost is causing saturation to 1.0, and apply a fix that preserves relative ranking.
