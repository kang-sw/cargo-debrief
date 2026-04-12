---
title: "CLI scaffold, config system, and service trait"
started: 2026-04-04
completed: 2026-04-04
parent: 260403-epic-mvp-implementation
related:
  260403-research-rag-architecture: architecture decisions
---

# CLI Scaffold, Config System, and Service Trait

Establish the project skeleton: Cargo.toml dependencies, `lib.rs` /
`main.rs` split, clap CLI with all MVP subcommands, 3-layer config
resolution, and the `DebriefService` async trait with an
`InProcessService` stub. After this ticket, every subsequent phase
has a runnable binary to plug into.

Spec: `ai-docs/spec/cargo-debrief.md` (Project Configuration,
Embedding Model Management sections).

## Design Decisions

Captured from discussion — these are **firm**:

1. **Single crate, no workspace.** One publishable crate for
   `cargo install` simplicity. No `lib`-prefix subcrates.
2. **`lib.rs` + `main.rs` split.** `main.rs` is a thin clap wrapper;
   all logic lives behind `lib.rs` for integration-test access.
3. **Single binary, multi-mode.** Daemon runs as `cargo debrief daemon`
   (Phase 2), not a separate executable.
4. **Async service trait from day one.** Avoids signature-breaking
   changes when Phase 2 adds IPC. `#[tokio::main]` in `main.rs`.
5. **Minimal config skeleton.** 3-layer path resolution and TOML
   I/O are implemented; config fields are kept to the minimum
   needed now (embedding model name). More fields added by later
   tickets.
6. **`anyhow` for errors.** No typed error enums at MVP stage.
7. **Only Phase 1A modules.** `config.rs` and `service.rs` created;
   other modules (`chunker/`, `embedder.rs`, etc.) deferred to
   their own tickets.

## Phase 1: Project Structure + CLI + Config

Cargo.toml, module layout, clap subcommands, and the 3-layer
config system.

### Goal

A binary that parses all MVP subcommands and loads/merges
configuration from the three layers.

### Scope

**Cargo.toml dependencies:**

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
toml = "0.8"
anyhow = "1"
dirs = "6"
tokio = { version = "1", features = ["full"] }
```

Versions are starting points — use latest compatible at implementation.

**File layout:**

```
src/
  main.rs    — #[tokio::main], clap App, subcommand dispatch
  lib.rs     — pub mod config; (pub mod service in Phase 2)
  config.rs  — Config struct, path resolution, load/merge
```

**CLI subcommands (clap derive):**

```
cargo debrief index [<path>]
cargo debrief search <query> [--top-k N]
cargo debrief get-skeleton <file>
cargo debrief set-embedding-model [--global] <model-name>
```

All subcommands are parsed but stub-implemented (print
"not yet implemented" and exit). The goal is argument parsing
correctness, not execution.

**Config system:**

Three config layers, each a TOML file:

| Layer | Path | Tracked | Purpose |
|-------|------|---------|---------|
| Global | `~/.config/debrief/config.toml` | N/A | User defaults |
| Project | `.debrief/config.toml` | git | Team-shared settings |
| Local | `.git/debrief/local-config.toml` | no | Machine overrides |

Resolution order: local → project → global → built-in default.
Later layers override earlier ones field-by-field (not file-level
replacement).

Minimal `Config` struct for Phase 1A:

```rust
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub embedding_model: Option<String>,
    // Future phases add fields here.
}
```

Provide functions:
- `config_paths(project_root: &Path) -> ConfigPaths` — resolve
  the three paths (global via `dirs::config_dir`, project/local
  via walking to `.git/`).
- `load_config(paths: &ConfigPaths) -> Result<Config>` — load
  and merge all layers.

Path to `.git/` is discovered by walking parent directories (same
logic git uses). If no `.git/` is found, local and project layers
are absent (global + default only).

### Success Criteria

- `cargo run -- search "foo"` parses successfully, prints stub message.
- `cargo run -- set-embedding-model --global nomic-embed` parses.
- `cargo run -- index` with no args defaults path to `.`.
- Config loads from global dir when no project config exists.
- Config merges correctly when multiple layers are present.
- Unit tests for config path resolution and layer merging.

### Result (90d43cb) — 26-04-04

Implemented as specified. 7 config tests + CLI parsing verified.

- `toml = "1"` used instead of `"0.8"` (ticket says "use latest compatible")
- `lib.rs` exports `config` module; `main.rs` is thin clap wrapper
- `find_git_root` only handles `.git` as directory (worktree/submodule
  limitation documented in code and spec)

## Phase 2: DebriefService Trait + InProcessService

Define the async service boundary and wire CLI subcommands through it.

### Goal

CLI subcommands dispatch through `DebriefService` trait methods.
`InProcessService` implements the trait with stubs that return
"not implemented" errors. This establishes the contract that all
subsequent phases implement against.

### Scope

**`src/service.rs`:**

```rust
use anyhow::Result;

/// Service boundary between CLI and core logic.
/// Phase 1: InProcessService (direct calls).
/// Phase 2: DaemonClient (IPC to background daemon).
#[trait_variant::make(Send)]
pub trait DebriefService {
    async fn index(&self, path: &Path) -> Result<IndexResult>;
    async fn search(&self, query: &str, top_k: usize) -> Result<Vec<SearchResult>>;
    async fn get_skeleton(&self, file: &Path) -> Result<String>;
    async fn set_embedding_model(&self, model: &str, global: bool) -> Result<()>;
}
```

Result types are minimal structs — enough to compile and display,
expanded by later tickets:

```rust
pub struct IndexResult {
    pub files_indexed: usize,
    pub chunks_created: usize,
}

pub struct SearchResult {
    pub file_path: String,
    pub line_range: (usize, usize),
    pub score: f64,
    pub display_text: String,
}
```

**`InProcessService`:**

```rust
pub struct InProcessService {
    config: Config,
}

impl InProcessService {
    pub fn new(config: Config) -> Self { ... }
}
```

All trait methods return `anyhow::bail!("not yet implemented")` for
now. Later tickets fill in real logic one method at a time.

**CLI wiring (`main.rs`):**

Each subcommand constructs `InProcessService` (loading config),
calls the corresponding trait method, formats the result, and
prints. Error display via `anyhow`'s default formatting.

**Dependency note:** `trait_variant` crate may be needed for
async trait Send bounds. Evaluate whether `async-trait` or
manual desugaring is preferable — choose the simplest option
that compiles cleanly.

### Success Criteria

- `cargo run -- index .` calls `InProcessService::index`, prints
  "not yet implemented" error.
- `cargo run -- search "foo"` calls `search` method, same result.
- `InProcessService` is constructable with a loaded `Config`.
- Trait is object-safe or explicitly not (document the choice).
- Integration test: construct `InProcessService`, call each method,
  assert error contains "not yet implemented".

### Result (b943612) — 26-04-04

Implemented as specified with one deviation:

- Used native RPITIT (`fn ... -> impl Future + Send`) instead of
  `trait_variant` or `async-trait` — Rust 2024 edition supports this
  natively. Trait is non-object-safe (documented).
- CLI subcommands now dispatch through `InProcessService`, format
  results, and print. Errors surface via anyhow's default display.
- 1 integration test verifying all 4 stub methods return expected errors.
- Review fixes (540c5b6): clippy let-chains, object-safety doc,
  malformed TOML test. Total: 8 tests.

## Future Phases (out of scope)

Later epic phases build on this scaffold:
- Phase 1B tickets add `git.rs`, `chunker/`, `store.rs` and fill
  in `InProcessService::index`.
- Phase 1C tickets add `embedder.rs`, `search.rs` and fill in
  `InProcessService::search`.
- Phase 1D wires everything end-to-end.
