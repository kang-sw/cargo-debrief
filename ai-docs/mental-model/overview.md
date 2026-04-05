# cargo-debrief — Mental-Model Overview

## Entry Points

- `src/main.rs` — CLI parse, config load, service construction, command dispatch
- `src/lib.rs` — public module re-exports; exists so integration tests and future consumers can `use cargo_debrief::*` without depending on the binary

## Modules

| Module | File(s) | Role |
|---|---|---|
| `config` | `src/config.rs` | Path resolution, file loading, layer merge, layer write |
| `service` | `src/service.rs` | `DebriefService` trait + `InProcessService` + `DaemonClient` + `Service` dispatch enum |
| `daemon` | `src/daemon.rs` | Daemon process entry point, PID management, idle timeout, async/sync IPC bridge |
| `ipc` | `src/ipc/mod.rs`, `src/ipc/protocol.rs`, `src/ipc/unix.rs`, `src/ipc/windows.rs` | Platform-abstracted IPC transport; `DaemonRequest`/`DaemonResponse` protocol |
| `chunk` | `src/chunk.rs` | `Chunk` data model (pure data, no logic) |
| `chunker` | `src/chunker/mod.rs`, `src/chunker/rust.rs` | `Chunker` trait + `RustChunker` (tree-sitter AST walk) |
| `git` | `src/git.rs` | Git file tracking via `Command` shellout |
| `store` | `src/store.rs` | Versioned index serialization (bincode) |
| `embedder` | `src/embedder.rs` | Model registry, safetensors download, inference via candle (`NomicBertModel`/`BertModel`) or burn (`BurnNomicBertModel`) |
| `nomic_bert_burn` | `src/nomic_bert_burn.rs` | NomicBERT architecture in burn: weights loading, mean pooling, L2 norm; GPU via `wgpu` feature, CPU via NdArray |
| `deps` | `src/deps.rs` | Cargo metadata parsing; dependency package discovery with root-dep reachability |
| `search` | `src/search.rs` | Vector ANN search (hnsw_rs) with metadata score boosting |

## Module Contracts

- `main.rs` resolves `current_dir()` as project root, calls `resolve_service()` to obtain a `Service` (either `Service::Daemon` if a live daemon is detected, or `Service::InProcess` as fallback), and dispatches subcommands. Config is loaded per-operation inside service methods, not at startup.
- `lib.rs` guarantees all modules usable as a library are declared `pub mod` here. Modules that exist only in `main.rs` are invisible to integration tests.

## Coupling

- **lib.rs is the integration-test boundary.** New modules must be declared in `lib.rs`, not inlined in `main.rs`, to be testable without spawning a subprocess.
- **Async runtime choice is load-bearing.** The service trait uses RPITIT (`impl Future` in trait) rather than the `async-trait` crate. Changing to `async-trait` later would make the trait object-safe but require a crate dep and macro overhead.
- **`chunk` is a shared data contract.** Both `chunker` and `store` depend on `chunk::Chunk`. Adding or renaming fields in `Chunk` requires updating both and bumping `store::INDEX_VERSION`.
- **`deps` is a standalone discovery module consumed by `service`.** `discover_dependency_packages` shells out to `cargo metadata` and returns `DepPackageInfo` values. `service.rs::run_deps_index` consumes these to produce `DepsIndexData` stored at `.git/debrief/deps-index.bin`. Dep chunks carry `ChunkOrigin::Dependency`; project chunks carry `ChunkOrigin::Project`.
- **`daemon` and `ipc` are only exercised when a daemon is running.** `DaemonClient::connect` checks PID liveness + readiness indicator before returning a client; if any check fails, the caller gets `Service::InProcess` silently. The daemon is bound to a single workspace at startup — it does not multiplex workspaces.

## Technical Debt

- `find_git_root` does not support git worktrees or submodules (`.git` file vs. directory). See `config.md`.
- `service.rs::index_path`, `service.rs::deps_index_path`, `daemon.rs::daemon_dir`, and `config.rs::find_git_root` all duplicate the git-root-walk logic. Four independent copies now exist; a shared utility is overdue.
