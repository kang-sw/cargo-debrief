---
title: "Usability Test Repos — Real-World Codebase Testing for Search Quality"
parent: 260403-epic-mvp-implementation
related:
  260404-feat-rust-chunking-population:
category: idea
priority: medium
---

# Usability Test Repos — Real-World Codebase Testing for Search Quality

## Problem

Unit and integration tests verify correctness (no crashes, round-trips work) but
cannot measure search quality — whether results are actually useful for an AI agent
working on unfamiliar code. RAG tool quality is ultimately about precision/recall on
real queries, which requires real codebases.

## Proposal

Clone real-world repositories locally for non-automated usability testing. An AI
agent (Claude) runs cargo-debrief against these repos and evaluates search result
quality.

**Storage location:** `test-repos/` at project root, gitignored. NOT submodules —
just shallow clones pinned to specific commits for reproducibility.

**Candidate repositories:**

| Language | Repo | Rationale |
|----------|------|-----------|
| Rust | `BurntSushi/ripgrep` | Well-structured, moderate size, practical CLI tool |
| C++ | `fmtlib/fmt` | Clean C++ library, template-heavy |
| Python | `psf/requests` | Well-known, clean structure |
| Cross-language | `protocolbuffers/protobuf` | C++/Python/Rust in one repo |

**Phasing:**

- Phase 1 (now): Rust-only usability test after Phase 1D integration
- Phase 2: C++/Python repos become targets when those chunkers are developed
- Cross-language repo useful for testing cross-reference patterns

**Test protocol (to be defined):**

- Predefined query set per repo (e.g., "find regex matching logic in ripgrep")
- Success criteria: relevant chunk in top-3/top-5
- AI agent executes tests and records results in a structured format
- Regression detection: re-run after refactoring to catch quality degradation

**Setup script:** A simple shell script that clones repos at pinned commits:

```bash
#!/bin/bash
# test-repos/setup.sh
mkdir -p test-repos
git clone --depth 1 --branch <tag> <url> test-repos/<name>
```

## Open Questions

- Exact commit pins for each repo
- Query set definition — needs manual curation per repo
- How to structure the AI agent test protocol (script? checklist? automated harness?)
- Whether to track test results in git (e.g., `test-repos/results/`) or keep ephemeral

## Value

- Only way to validate "does search actually help?" before shipping
- Provides regression baseline for search quality across refactors
- Ready-made targets when C++/Python chunkers are implemented
- Low cost: just disk space for shallow clones
