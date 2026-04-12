---
title: "Add project Rust source to .debrief config for smoke test coverage"
---

# Add project Rust source to .debrief config for smoke test coverage

## Background

Smoke test TC4 (DebriefService search quality, symbol name boost) could not be evaluated because `.debrief/config.toml` only lists C++ test repos. The main cargo-debrief Rust source is missing from the indexed sources. Auto-detect only fires when `config.sources` is empty, so the Rust source is never indexed in the current config.

## Phases

### Phase 1: Add Rust source entry and verify search quality

Add a Rust `SourceEntry` for the project root (`src/`) to `.debrief/config.toml`. After re-indexing, run `search "DebriefService"` and verify that the trait definition appears as result #1 with a score gap of at least 0.2 over result #2.
