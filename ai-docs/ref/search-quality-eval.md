# Search Quality Evaluation Protocol

Standardized protocol for measuring cargo-debrief search quality.
Run after embedding model changes, chunker modifications, or search
algorithm updates to track quality regressions and improvements.

## Target Repository

**ripgrep** — `test-repos/ripgrep/` (shallow clone, pinned commit).
Must be fully indexed before running queries.

## Query Set

8 queries covering 4 categories. Each query evaluated on top-3 results.

### Symbol Lookup (exact match)

| ID | Query | Expected top-1 | Pass if |
|----|-------|----------------|---------|
| S1 | `Searcher` | `Searcher` struct overview | top-1 score > 0.85 |
| S2 | `BufReader` | (not in codebase) | top-1 score < 0.55 (no false confidence) |

### Semantic — Specific Topic

| ID | Query | Expected in top-3 | Pass if |
|----|-------|-------------------|---------|
| T1 | `ignore rules and gitignore` | `Gitignore` struct, `GitignoreBuilder` | relevance >= 2/3 |
| T2 | `file searching and filtering` | `Searcher::search_file`, `search_path` | relevance >= 2/3 |
| T3 | `printer output formatting` | `Standard<W>`, `print_matches` | relevance >= 1/3 |

### Semantic — Structural

| ID | Query | Expected in top-3 | Pass if |
|----|-------|-------------------|---------|
| R1 | `command line argument parsing` | `Flag` trait, `HiArgs`, parser dispatch | relevance >= 1/3 |
| R2 | `parallel directory walking` | `WalkBuilder`, `WalkParallel` | relevance >= 1/3 |

### Semantic — General

| ID | Query | Expected in top-3 | Pass if |
|----|-------|-------------------|---------|
| G1 | `regex pattern matching` | regex compilation/matching code | relevance >= 1/3 |

## Evaluation Procedure

1. Ensure ripgrep is fully indexed (`rebuild-index` from `test-repos/ripgrep/`)
2. For each query, run `search "<query>" --top-k 5`
3. Record per-result: rank, score, file, symbol name
4. Judge relevance of top-3: 0 (irrelevant), 1 (tangential), 2 (relevant)
5. Compute metrics

## Metrics

### Per-query

| Metric | Formula |
|--------|---------|
| Top-3 relevance | count of relevant results in top 3 (0-3) |
| Top-1 score | similarity score of rank 1 result |
| Precision@3 | relevant / 3 |

### Aggregate

| Metric | Formula |
|--------|---------|
| Overall relevance | sum of all top-3 relevance / (8 * 3) |
| Symbol accuracy | S1 pass + S2 pass / 2 |
| Semantic accuracy | (T1 + T2 + T3 + G1 pass count) / 4 |
| Structural accuracy | (R1 + R2 pass count) / 2 |

## Results Chart

Copy and fill this template for each evaluation run.

```
## Run: <date> — <description>

Model: <model name and version>
Chunker: <notable changes>
Index: <chunk count> chunks, <file count> files

| ID | Query                           | Top-1 Score | Top-3 Rel | Pass |
|----|---------------------------------|-------------|-----------|------|
| S1 | Searcher                        |             |           |      |
| S2 | BufReader                       |             |           |      |
| T1 | ignore rules and gitignore      |             |           |      |
| T2 | file searching and filtering    |             |           |      |
| T3 | printer output formatting       |             |           |      |
| R1 | command line argument parsing   |             |           |      |
| R2 | parallel directory walking      |             |           |      |
| G1 | regex pattern matching          |             |           |      |

Aggregate:
- Overall relevance: __/24 (__%）
- Symbol accuracy: __/2
- Semantic accuracy: __/4
- Structural accuracy: __/2

Notes:
- <observations, regressions, improvements>
```

## Baseline (2026-04-04, 5-file subset)

**Caveat:** Run on 5-file subset only (full index failed, P0 blocker).
R2 invalid — `walk.rs` not in subset.

Model: nomic-embed-text-v1.5
Index: 1,332 chunks, 5 files

| ID | Query                           | Top-1 Score | Top-3 Rel | Pass |
|----|---------------------------------|-------------|-----------|------|
| S1 | Searcher                        | 0.9043      | 3/3       | Y    |
| S2 | BufReader                       | 0.4899      | 1/3       | Y    |
| T1 | ignore rules and gitignore      | 0.7626      | 3/3       | Y    |
| T2 | file searching and filtering    | 0.6501      | 3/3       | Y    |
| T3 | printer output formatting       | 0.5677      | 2/3       | Y    |
| R1 | command line argument parsing   | 0.5741      | 0/3       | N    |
| R2 | parallel directory walking      | 0.5352      | 0/3       | N*   |
| G1 | regex pattern matching          | 0.7250      | 2/3       | Y    |

Aggregate:
- Overall relevance: 14/24 (58%)
- Symbol accuracy: 2/2
- Semantic accuracy: 4/4
- Structural accuracy: 0/2 (R2 invalid due to subset)

Notes:
- Subset test only. Full-repo baseline pending P0 fix.
- R2 failure is coverage gap, not search quality issue.
- R1 failure is micro-chunk problem (P1) + embedding model limitation.
