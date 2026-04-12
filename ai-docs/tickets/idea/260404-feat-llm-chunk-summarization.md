---
title: "LLM Chunk Summarization — External LLM for Embedding Enrichment"
category: feat
priority: medium
related:
  260404-fix-usability-test-findings: structural query weakness motivates this
  260404-feat-dependency-chunking: dep chunks also benefit from summaries
---

# LLM Chunk Summarization

## Goal

Use an external OpenAI-compatible LLM endpoint to generate architectural
summaries for type overview chunks. Summaries are prepended to
`embedding_text`, bridging the vocabulary gap between natural-language
queries ("command line argument parsing") and code structure (`Flag`
trait, `HiArgs` struct).

## Motivation

Usability test showed structural semantic queries scoring 0/3 relevance.
Root cause: small embedding models (137M-335M params) encode tokens, not
architectural concepts. Embedding model upgrades yield marginal gains
(+5-10%). LLM-generated summaries inject the missing vocabulary directly
into the embedding text, giving the embedding model something to match.

Combined with P1 micro-chunk merging (which ensures the right chunks
exist as candidates), LLM summaries ensure those chunks rank highly
for structural queries.

## Design

### External endpoint, not local inference

Uses any OpenAI-compatible `/v1/chat/completions` endpoint: vLLM,
ollama, llama.cpp server, OpenAI, etc. No LLM inference code in
cargo-debrief — HTTP client only (reqwest already in deps).

### Configuration via unified `config` subcommand

```bash
cargo debrief config llm.endpoint "https://vllm.internal/v1" --global
cargo debrief config llm.token "sk-..." --global
cargo debrief config llm.model "qwen2.5-coder-7b" --global
```

Stored in config.toml `[llm]` section. Resolution follows standard
local → project → global → default chain. No `[llm]` config = feature
disabled.

### Scope: overview chunks only

Only type overview chunks are summarized. Function chunks are skipped
(too numerous, low ROI). This keeps API call volume manageable:

- 100-file project: ~300 overview chunks, ~90s at 250 tok/s
- Incremental: only changed files re-summarized

### Summary injection

Generated summary prepended to `embedding_text`:

```
// Summary: Defines CLI flag types implementing the Flag trait
// for command-line argument parsing and option dispatch.
// crate::core::flags::defs (crates/core/flags/defs.rs:1..7779)
struct Flag { ... }
impl Flag for LineRegexp { ... }
```

### Graceful degradation

- Endpoint unreachable → warning to stderr, index without summaries
- Individual chunk summary fails → skip that chunk's summary, continue
- No `[llm]` config → feature entirely disabled, no overhead

### Caching

Summaries stored in the index alongside chunks. On incremental
re-index, only chunks from changed files are re-summarized. Existing
summaries for unchanged files are preserved.

## Pre-requisites

- Unified `config` subcommand (replaces `set-embedding-model`)
- P1 micro-chunk merging (ensures overview chunks are quality candidates)

## Phases

### Phase 1 — Config subcommand unification

Replace `set-embedding-model` CLI command with unified `config`
subcommand. Git-config-style key-value interface:

```
cargo debrief config <key> [value] [--global]
cargo debrief config --list
```

Keys: `embedding.model`, `llm.endpoint`, `llm.token`, `llm.model`.
Config.toml schema: `[embedding]` section (migrated from current
format), `[llm]` section (new).

Update clap command definitions in `main.rs`. Migrate existing
`set_embedding_model` in service.rs to use new config paths.

### Phase 2 — LLM summarization pipeline

- Add `Summarizer` module: HTTP client to `/v1/chat/completions`
- Prompt template: file overview → 2-3 sentence architectural summary
- Integration in `run_index`: after chunking, before embedding,
  summarize overview chunks if `[llm]` config present
- Summary prepended to `embedding_text`
- Error handling: per-chunk fallback, endpoint-level warning

### Phase 3 — Caching and incremental support

- Store summaries in `Chunk` (new optional field)
- On incremental re-index, preserve summaries for unchanged files
- Re-summarize only chunks from changed files
- Bump `INDEX_VERSION`

## Open Questions

- Prompt template: how much context to include? Full overview chunk
  text, or just signatures + doc comments?
- Rate limiting: should cargo-debrief respect rate limits from the
  endpoint, or leave that to the server?
- Batch API: use `/v1/chat/completions` per-chunk, or batch endpoint
  if available?
- Summary validation: any heuristic to detect hallucinated summaries?
