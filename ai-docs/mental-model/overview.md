# cargo-debrief — Mental-Model Overview

## Entry Points

- `src/main.rs` — CLI parse, config load, service construction, command dispatch
- `src/lib.rs` — public module re-exports; exists so integration tests and future consumers can `use cargo_debrief::*` without depending on the binary

## Modules

| Module | File(s) | Role |
|---|---|---|
| `config` | `src/config.rs` | Path resolution, file loading, layer merge, layer write |
| `service` | `src/service.rs` | `DebriefService` trait + `InProcessService` |
| `chunk` | `src/chunk.rs` | `Chunk` data model (pure data, no logic) |
| `chunker` | `src/chunker/mod.rs`, `src/chunker/rust.rs` | `Chunker` trait + `RustChunker` (tree-sitter AST walk) |
| `git` | `src/git.rs` | Git file tracking via `Command` shellout |
| `store` | `src/store.rs` | Versioned index serialization (bincode) |
| `embedder` | `src/embedder.rs` | ONNX embedding pipeline: model registry, download, inference |
| `search` | `src/search.rs` | Vector ANN search (hnsw_rs) with metadata score boosting |

## Module Contracts

- `main.rs` resolves `current_dir()` as project root, constructs one `InProcessService` (zero-sized), and dispatches subcommands. Config is loaded per-operation inside service methods, not at startup.
- `lib.rs` guarantees all modules usable as a library are declared `pub mod` here. Modules that exist only in `main.rs` are invisible to integration tests.

## Coupling

- **lib.rs is the integration-test boundary.** New modules must be declared in `lib.rs`, not inlined in `main.rs`, to be testable without spawning a subprocess.
- **Async runtime choice is load-bearing.** The service trait uses RPITIT (`impl Future` in trait) rather than the `async-trait` crate. Changing to `async-trait` later would make the trait object-safe but require a crate dep and macro overhead.
- **`chunk` is a shared data contract.** Both `chunker` and `store` depend on `chunk::Chunk`. Adding or renaming fields in `Chunk` requires updating both and bumping `store::INDEX_VERSION`.

## Technical Debt

- `find_git_root` does not support git worktrees or submodules (`.git` file vs. directory). See `config.md`.
- `service.rs::index_path` duplicates the git-root-walk logic from `config.rs::find_git_root`. A third caller should prompt a shared utility.
