<!-- Memory policy: prune aggressively as project advances. Completed
     work belongs in git history, not here. Keep only what an AI session
     needs to orient itself and pick up work. If it's derivable from
     code or git log, delete it from this file. -->

# cargo-debrief — Project Index

## Project Summary

**cargo-debrief** — A Rust CLI tool that provides RAG
(Retrieval-Augmented Generation) over codebases. Uses tree-sitter for
AST-aware chunking and vector search with metadata boosting to feed
LLMs only the relevant code fragments, reducing context window consumption.
CLI-first with a lazy-spawned background daemon for index serving.

## Tech Stack

Rust (2024 edition). Key libs: tree-sitter, burn+wgpu (GPU embedding, default),
ort (ONNX Runtime, CPU-only build via `--features ort-cpu`), clap, serde, tokio.

## Workspace

```
src/           — Main source code
ai-docs/       — Project knowledge and cross-session context
```

## Architecture

Module layout (`lib.rs` + `main.rs` split — `main.rs` is thin clap
wrapper, all logic behind `lib.rs`):

```
src/
  main.rs       — CLI entrypoint (clap): rebuild-index, search, overview, config
  lib.rs        — module re-exports
  config.rs     — 3-layer config resolution (local → project → global → default)
  deps.rs       — Dependency discovery (cargo metadata, BFS root-dep)
  service.rs    — DebriefService trait + InProcessService + DaemonClient + Service enum dispatch
  chunk.rs      — Chunk data model (Chunk, ChunkMetadata, ChunkKind, ChunkType, Visibility)
  chunker/      — Chunker trait + RustChunker (tree-sitter AST-aware chunking)
    mod.rs      — Chunker trait definition
    rust.rs     — RustChunker: two-pass AST walk, impl aggregation, dual text generation
  git.rs        — Git file tracking (head_commit, changed_files via Command shellout)
  store.rs      — Index serialization (IndexData + DepsIndexData, bincode + versioned header)
  embedder.rs   — ONNX Runtime embedding: ModelRegistry, Embedder (load, embed_batch, mean pooling + L2 norm)
  search.rs     — Vector search: SearchIndex (hnsw_rs ANN + symbol-name metadata boosting)
  daemon.rs     — Daemon process lifecycle, PID management, idle timeout, binary identity guard
  ipc/          — Platform-abstracted IPC (Unix FIFO + Windows atomic-rename)
    mod.rs      — cfg-gated re-exports
    protocol.rs — DaemonRequest/Response, length-prefixed JSON framing
    unix.rs     — FIFO transport, poll(2), flock serialization
    windows.rs  — Atomic-rename transport, file polling, LockFileEx
```

CLI dispatches through `DebriefService` trait. Phase 1 uses `InProcessService`
(direct library calls). Phase 2 adds `DaemonClient` (IPC to daemon process).
Single binary — daemon runs as `cargo debrief daemon`, not a separate executable.

## Key Design Decisions

- **CLI-first with per-workspace daemon**: Primary interface is CLI.
  Per-workspace daemon lazy-spawned on first use, ~3 min idle expiry.
  Holds ONNX session + HNSW index in memory (eliminates 2-4s startup).
  Temp-file-based RPC for sandbox compatibility.
- **No external DB**: vectors stored in-memory as `Vec<[f32; N]>`, serialized
  to disk with bincode (versioned format).
- **Vector search + metadata boosting**: cosine similarity with hnsw_rs,
  metadata score boosting for exact symbol name matches.
- **Hierarchical chunking**: level 0 (struct skeletons — signatures
  only), level 1 (function bodies), level 2 (referenced type declarations).
  Search hits at level 1 auto-attach level 0 context.
- **Git-based incremental indexing**: per-git-root `GitRepoState` tracks
  `last_indexed_commit` + `DirtySnapshot` (sha256 per dirty/untracked file).
  `ensure_index_fresh` diffs commit changes + dirty changes + reverts per
  source; patches `IndexData.chunks` surgically. Non-git sources scan once,
  manual-only thereafter (`rebuild-index --source <path>`).
- **Rust-first, language-extensible**: Start with tree-sitter-rust. Chunker
  trait allows adding more languages later.
- **Unified config**: `cargo debrief config <key> [value] [--global]`.
  Replaces `set-embedding-model`. Manages embedding model, LLM endpoint, etc.

## Spec

- `ref/cargo-debrief.md` — Full feature spec: indexing, search, CLI, daemon, model management

## Conventions

- Tickets: `ai-docs/tickets/<status>/YYMMDD-<type>-<name>.md`
- Reference by stem only: `260403-research-rag-architecture`

## Build / Test

```bash
cargo build
cargo test                                           # unit (37) + offline integration (8) + network integration (3)
CARGO_DEBRIEF_SKIP_NETWORK=1 cargo test              # skip network tests (no model download)
cargo run -- rebuild-index [<path>]                  # full re-index (manual/recovery)
cargo run -- search "query" [--top-k N]              # vector search + metadata boosting (auto-indexes)
cargo run -- overview <file>                         # file-level overview (auto-indexes)
cargo run -- config <key> [value] [--global]          # get/set configuration
cargo run -- daemon status                           # check daemon
```

### Test architecture

| Layer | File(s) | Network | What it covers |
|-------|---------|---------|----------------|
| Unit tests | `src/*.rs` `#[cfg(test)]` | No | Module internals: config merge, chunker AST, store round-trip, model registry, HNSW search with fake vectors |
| Offline integration | `tests/integration.rs` | No | Cross-module boundaries with mock embedder: chunker→store round-trip, search with mock embeddings, config multi-layer merge, git→chunker pipeline |
| Network integration | `tests/integration_network.rs` | Yes (~130MB model download, cached) | Real ONNX embedder + search, chunker→embedder compatibility, semantic search quality smoke tests |

Network tests download `nomic-embed-text-v1.5` on first run to `~/.local/share/debrief/models/` (Linux) or `~/Library/Application Support/debrief/models/` (macOS). Cached after first download. Skip with `CARGO_DEBRIEF_SKIP_NETWORK=1`.

### Smoke test

See `ai-docs/smoke-test.md` for the manual CLI verification protocol.
Run after changes to service wiring, chunker, embedder, search, or store.

## Mental Model

See `ai-docs/mental-model/` for operational knowledge:
- `overview.md` — crate structure, module map, coupling notes
- `config.md` — 3-layer resolution, merge semantics, known limitations
- `service.md` — DebriefService trait, RPITIT non-object-safety, dispatch options
- `chunker.md` — two-pass design, impl aggregation, orphan impl handling
- `store.md` — bincode serialization, version mismatch semantics
- `git.md` — Command shellout, changed_files contract
- `embedder.md` — ModelRegistry, Embedder, ONNX inference, model download
- `search.md` — SearchIndex, hnsw_rs ANN, metadata boosting
- `deps.md` — cargo metadata discovery, BFS root-dep computation, DepPackageInfo contract
- `daemon.md` — daemon lifecycle, PID lock, idle timeout, async/sync bridge
- `ipc.md` — platform IPC abstraction, protocol, flock contract

## Post-MVP Roadmap

```
A✓ Usability test (ripgrep)        — validate search quality on real codebase
C✓ Dependency chunking             — index transitive deps, public API only
D✓ Daemon mode                     — per-workspace, FIFO/file RPC, ~3 min idle, auto-spawn
E  LLM chunk summarization         — external LLM for embedding text enrichment
B  Rust chunking population        — additional node kinds, informed by A results
D  C++/Python chunkers             — language expansion
```

Tickets: `260404-feat-llm-chunk-summarization` (E),
`260404-feat-rust-chunking-population` (B)

## Session Notes

- Initial project setup. Research ticket captures architecture discussion.
- Phase 1A scaffold implemented: CLI, config, service trait.
- Phase 1B core indexing pipeline implemented: chunk model, tree-sitter Rust chunking, git tracking, index serialization.
- Service trait refactored: `project_root: &Path` added to all `DebriefService` methods; `InProcessService` is now zero-sized; config loading removed from `main.rs`.
- Phase 1C search pipeline implemented: embedder.rs (ONNX inference via ort, model registry with nomic-embed-text-v1.5 + bge-large-en-v1.5, streaming download, mean pooling + L2 norm), search.rs (hnsw_rs ANN, metadata symbol-name boosting), config save_config, set_embedding_model wired.
- Phase 1D integration & polish: end-to-end wiring of `index`, `search`, `overview` in InProcessService. Implicit auto-indexing via `ensure_index_fresh`. CLI renames: `index` → `rebuild-index`, `get-skeleton` → `overview`. Smoke test protocol added.
- Post-MVP roadmap defined: A→C→D*→B→D ordering. Daemon revised to per-workspace with temp-file RPC. Dependency chunking: all transitive, pub API only, root-dep annotation in embedding text.
- P0 batch split implemented (64-chunk batches). GPU EP registered (CoreML/CUDA behind feature flags). CoreML unstable (41GB RSS, context leak). CPU path verified on full ripgrep: 3070 chunks, 100 files, 9m37s.
- Full-repo search quality: 15/24 (62.5%). S1/T3 regressions from micro-chunk dilution — P1 merging designed (minimum body threshold + module overview chunk).
- Spec updated: `set-embedding-model` → unified `config <key> [value] [--global]`. LLM chunk summarization feature added (external OpenAI-compatible endpoint for overview chunk summaries).
- Roadmap: A→C→D*→E→B→D. E = LLM chunk summarization (`260404-feat-llm-chunk-summarization`).
- cargo-brief output format reviewed for reference. Adopting: module context line in search output (Phase 3).
- P1 micro-chunk merging implemented (≤5-line method inlining, module overview chunks). INDEX_VERSION 3.
- Phase 3 UX: overview ordering by visibility (pub→pub(crate)→pub(super)→private), search results prefixed with `// in crate::module` context line.
- Dependency Chunking Phase 1: ChunkOrigin enum on Chunk (Project | Dependency), new deps.rs module (cargo metadata + BFS root-dep discovery), INDEX_VERSION 4. GPU bug split to separate ticket.
- Dependency Chunking Phase 2: DepsIndexData in store.rs (DEPS_INDEX_VERSION 1, Cargo.lock hash staleness). Embedder refactored to param-passing. run_deps_index pipeline: walk dep src/, pub-filter, [dependency] tag annotation, 64-chunk batch embedding, deps-index.bin. Wired into index + search (data unused until Phase 3).
- Dependency Chunking Phase 3: Unified search (project + dep chunks in single SearchIndex), DEP_ORIGIN_PENALTY 0.1, --no-deps CLI flag, dep_overview + --dep on overview, config exclude list, [dep: crate_name] output label. Spec updated, 🚧 removed from Dependency Indexing.
- MCP server mode removed from spec and docs (user decision: will not be implemented).
- Daemon Mode Phase 2A+2B: daemon.rs (lifecycle, PID flock, 3-min idle, debug binary guard), ipc/ module (Unix FIFO + Windows atomic-rename, length-prefixed JSON, flock client serialization), DaemonClient + Service enum in service.rs, daemon status/stop + hidden __daemon in main.rs. Auto-spawn deferred to Phase 2C.
- Daemon Mode Phase 2C: auto_spawn_and_connect (flock-serialized spawn, readiness polling, stale PID cleanup), Service struct (Option<DaemonClient> + InProcessService, silent fallback), resolve_service in main.rs. Race conditions R1-R10 mitigated. Daemon mode fully implemented, 🚧 removed from spec.
- Daemon keepalive fix: DaemonResponse::Progress variant, 10s interval keepalive during long operations. Prevents 120s timeout → InProcess fallback → duplicate ONNX sessions. Readiness timeout 30s→60s.
- GPU CoreML experiments: ANE disable (CPUAndGPU) did not resolve — RSS 110GB+. Instant allocation explosion, not gradual leak. ort 2.0.0-rc.12 upstream issue. Session recreation untested. Results recorded in GPU ticket.
- ort→candle migration: Replaced ort (ONNX Runtime) with candle 0.10.x in embedder.rs. NomicBertModel + BertModel dual dispatch via EmbedderModel enum. Safetensors loading, Device selection (Metal/CUDA/CPU). Feature flags: gpu/cuda → metal/cuda. INDEX_VERSION 5, DEPS_INDEX_VERSION 2. CPU path stable (~4GB RSS). candle Metal FAILED: "no metal implementation for layer-norm" — candle Metal backend unusable for transformers. All ort stable versions yanked; candle is now the stable publishable dependency.
- Deps indexing diagnostic: Pipeline functional, no crashes. 354 packages → 206,921 chunks → 3,234 batches. CPU estimate ~24h. GPU acceleration required for practical use. CARGO_DEBRIEF_NO_DAEMON env var added for in-process debugging.
- GPU acceleration epic created (`260405-epic-gpu-embedding-acceleration`): burn + WGPU identified as viable cross-platform GPU path. burn has all transformer ops including LayerNorm. NomicBERT model implementation needed (~300-500 LOC).
- burn NomicBERT implementation (epic child ticket 1): `src/nomic_bert_burn.rs` (517 LOC), BurnNomicBert EmbedderModel variant, `wgpu` feature flag, burn-store safetensors loading with PyTorchToBurnAdapter. Candle path preserved as parallel backend. INDEX_VERSION 6, DEPS_INDEX_VERSION 3. Review finding: partial RoPE assert guard added.
- C++ chunker ticket created (`260405-feat-cpp-chunker`): tree-sitter-based best-effort C/C++ chunking. Accepted preprocessor limitations. Validation target: ASIO headers.
- C++ deps discovery ticket created (`260405-feat-cpp-deps-discovery`): vcxproj/CMake parsing + recursive `#include` resolution. `.sln` auto-discovery with project name in embedding text. No depth limits — scale via GPU.
- ort CPU revival Phase 1 (`260411-refactor-ort-cpu-revival`): NdArray CPU path proven unviable (~9× slower than ort, OOM-prone). Decided to bring ort back as the CPU-only build path. Phase 1 landed: `ort = "=2.0.0-rc.12"` optional dep, new `ort-cpu` cargo feature, burn/burn-store made optional (wgpu feature only), `compile_error!` mutual-exclusion guard in lib.rs. Two valid build configs: default `wgpu` (GPU) and `--no-default-features --features ort-cpu` (CPU).
- ort CPU revival Phase 2 (`260411-refactor-ort-cpu-revival`): ort-cpu Embedder fully implemented. CPU EP only, Level3 optimization, tokenizers-crate reuse. Cache layout migrated to `<cache>/<model>/burn/` (wgpu) and `<cache>/<model>/ort/` (ort-cpu). Three required ONNX inputs: input_ids, attention_mask, token_type_ids (zero-filled). Benchmarked at 5.49 chunks/s / 9m03s on ripgrep — matches historical ort baseline.
- ort CPU revival Phase 3 (`260411-refactor-ort-cpu-revival`): BackendTag enum (Wgpu/OrtCpu) added to IndexData and DepsIndexData headers. INDEX_VERSION 6→7, DEPS_INDEX_VERSION 3→4. Backend mismatch on load returns Ok(None) → forced re-index. Bincode decode errors (old-layout files) also return Ok(None) instead of Err.
- ort CPU revival Phases 4+5 (`260411-refactor-ort-cpu-revival`): NdArray fully excised — burn ndarray feature removed from Cargo.toml, TestBackend in nomic_bert_burn.rs switched to Wgpu. Two and only two valid build configs remain: `cargo build` (wgpu/GPU) and `cargo build --no-default-features --features ort-cpu` (CPU). All tests green (86 lib + 12 integration). Ticket closed and moved to done/. Manual ripgrep smoke test not yet run — deferred to user.
- Multi-language sources Phase 1+2 (`260409-epic-multi-language-sources`): config-driven pipeline rewrite (SourceEntry, Language enum, `add`/`sources`/`remove` CLI subcommands, chunker dispatch). CppChunker implemented via tree-sitter-cpp (classes, functions, namespaces, free functions). DaemonClient new methods bail! to InProcessService — IPC not extended (Phase 3 forward note).
- CppChunker template specialization fix: `class_specifier` name extraction appended `template_argument_list` sibling — `Foo<int>` instead of `Foo`. Regression tests added.
- External source file discovery fix (`260412-fix-external-source-file-discovery`): `run_index_for_sources` now uses per-source walkdir for external git repos (separate `.git`). Single `git ls-files` was producing 0 files for cloned C++ repos. walkdir dep added. First GPU throughput datapoint: 14 chunks/s / 7m37s (6412 chunks, wgpu). ~2.5× over CPU ort baseline.
- Live progress output added to rebuild-index: `[discovery]` file count, `[chunking]` per-file with \r on TTY, `[embedding]` batch N/total with chunks/s + ETA. No external crate.
- Test repos added: `test-repos/nlohmann-json`, `test-repos/asio`, `test-repos/fmtlib` (shallow clones for C++ smoke testing). Registered as cpp sources. Search quality smoke test pending.
- Incremental indexing implemented (`260412-fix-incremental-indexing`): per-git-root GitRepoState replaces single last_indexed_commit. git status --porcelain tracks dirty/untracked files with sha256 content hashes; revert detection via dirty_snapshot O(n) scan. Non-git sources scan once, manual-only thereafter. rebuild-index --source <path> for targeted refresh. INDEX_VERSION 7→8. Smoke test pending: verify single-file edit triggers seconds-scale re-index instead of 7-min full rebuild.
