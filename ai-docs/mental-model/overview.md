# cargo-debrief — Mental-Model Overview

## Entry Points

- `src/main.rs` — CLI parse, config load, service construction, command dispatch
- `src/lib.rs` — public module re-exports (`config`, `service`); exists so integration tests and future consumers can `use cargo_debrief::*` without depending on the binary

## Module Contracts

- `main.rs` guarantees it constructs config from `current_dir()` at startup, then constructs exactly one `InProcessService`. No lazy initialization.
- `lib.rs` guarantees all modules usable as a library are declared `pub mod` here. Modules that exist only in `main.rs` are invisible to integration tests.

## Coupling

- **lib.rs is the integration-test boundary.** New modules must be declared in `lib.rs`, not inlined in `main.rs`, to be testable without spawning a subprocess.
- **Async runtime choice is load-bearing.** The service trait uses RPITIT (`impl Future` in trait) rather than the `async-trait` crate. Changing to `async-trait` later would make the trait object-safe but require a crate dep and macro overhead. Changing to `async fn` in trait (stabilized in Rust 2024) is equivalent to RPITIT — both work; the current code uses the explicit form.

## Technical Debt

- All `DebriefService` methods are stubs that return `anyhow::bail!`. The CLI currently errors on every command.
- `find_git_root` does not support git worktrees or submodules (`.git` file vs. directory). See `config.md`.
