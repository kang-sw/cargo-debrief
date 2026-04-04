# Smoke Test Protocol

Manual verification procedure for cargo-debrief CLI. Run after any
implementation change that affects indexing, search, or overview.

## Prerequisites

- Working `cargo build`
- A git repository with `.rs` source files (this repo works)
- Embedding model downloaded (~130MB, cached after first run)

## Test Cases

### 1. Semantic search — `search "chunking"`

```bash
cargo run -- search "chunking"
```

**Expect:**
- Results from `src/chunk.rs` and `src/chunker/` in the top 5
- Scores roughly in the 0.6–0.8 range
- Auto-indexing triggers silently on first run (or if index is stale)

### 2. Symbol name boost — `search "DebriefService"`

```bash
cargo run -- search "DebriefService"
```

**Expect:**
- `DebriefService` trait definition is **#1** with a score noticeably
  higher than #2 (boost of +0.3 for exact symbol match)
- Score gap between #1 and #2 should be ≥0.2

### 3. File overview — `overview src/chunk.rs`

```bash
cargo run -- overview src/chunk.rs
```

**Expect:**
- Shows all type definitions: `Chunk`, `ChunkMetadata`, `ChunkKind`,
  `ChunkType`, `Visibility`
- Shows only signatures — no function bodies
- Clean, readable output

### 4. Rebuild index — `rebuild-index .`

```bash
cargo run -- rebuild-index .
```

**Expect:**
- Prints `Indexed N files, M chunks created.`
- N should match the number of `.rs` files tracked by git
- M should be reasonable (roughly 5–15 chunks per file)

### 5. Incremental re-indexing (optional)

```bash
# After any git commit, run search again:
cargo run -- search "chunking"
```

**Expect:**
- Auto-indexing detects the new HEAD and re-indexes only changed files
- Results still relevant

## Baseline (2026-04-04, Phase 1D)

| Test | Key result | Score |
|------|-----------|-------|
| `search "chunking"` | #1 Chunk struct, #2 Chunker trait | 0.76, 0.72 |
| `search "DebriefService"` | #1 trait def (gap: +0.35 over #2) | 0.92 |
| `overview src/chunk.rs` | 5 types shown | — |
| `rebuild-index .` | 13 files, 144 chunks | — |

## When to Run

- After implementing or modifying: service wiring, chunker, embedder,
  search, or store modules
- After changing the embedding model or HNSW parameters
- Before merging a marathon session (as part of the checkpoint)
