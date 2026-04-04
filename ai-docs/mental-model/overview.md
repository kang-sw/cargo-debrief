# cargo-debrief — Mental-Model Overview

## Entry Points

- `src/main.rs` — CLI parse, config load, service construction, command dispatch
- `src/lib.rs` — public module re-exports; exists so integration tests and future consumers can `use cargo_debrief::*` without depending on the binary

## Modules

| Module | File(s) | Role |
|---|---|---|
| `config` | `src/config.rs` | Path resolution, file loading, layer merge |
| `service` | `src/service.rs` | `DebriefService` trait + `InProcessService` stub |
| `chunk` | `src/chunk.rs` | `Chunk` data model (pure data, no logic) |
| `chunker` | `src/chunker/mod.rs`, `src/chunker/rust.rs` | `Chunker` trait + `RustChunker` (tree-sitter AST walk) |
| `git` | `src/git.rs` | Git file tracking via `Command` shellout |
| `store` | `src/store.rs` | Versioned index serialization (bincode) |

## Module Contracts

- `main.rs` guarantees it constructs config from `current_dir()` at startup, then constructs exactly one `InProcessService`. No lazy initialization.
- `lib.rs` guarantees all modules usable as a library are declared `pub mod` here. Modules that exist only in `main.rs` are invisible to integration tests.

## Coupling

- **lib.rs is the integration-test boundary.** New modules must be declared in `lib.rs`, not inlined in `main.rs`, to be testable without spawning a subprocess.
- **Async runtime choice is load-bearing.** The service trait uses RPITIT (`impl Future` in trait) rather than the `async-trait` crate. Changing to `async-trait` later would make the trait object-safe but require a crate dep and macro overhead.
- **`chunk` is a shared data contract.** Both `chunker` and `store` depend on `chunk::Chunk`. Adding or renaming fields in `Chunk` requires updating both and bumping `store::INDEX_VERSION`.

## Technical Debt

- All `DebriefService` methods are stubs that return `anyhow::bail!`. The CLI currently errors on every command.
- `find_git_root` does not support git worktrees or submodules (`.git` file vs. directory). See `config.md`.
