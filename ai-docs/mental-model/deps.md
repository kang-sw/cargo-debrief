# Deps — Mental Model

## Entry Points

- `src/deps.rs` — `DepPackageInfo` struct + `discover_dependency_packages()`

## Module Contracts

- `discover_dependency_packages` shells out to `cargo metadata --no-deps=false --format-version 1` using `cargo_metadata::MetadataCommand`. The caller must ensure `cargo` is on `PATH`; there is no fallback.
- Workspace members (packages whose `id` appears in `metadata.workspace_members`) are **excluded** from the result — the function returns only external dependencies.
- `DepPackageInfo::root_deps` contains the names of **direct** workspace dependencies from which the package is transitively reachable. A package with no path to any direct workspace dep has an empty `root_deps` (should not happen in practice; the BFS guarantees coverage for all reachable packages).
- `root_deps` entries use the original hyphenated crate name from `package.name` (e.g. `"tree-sitter"`, not `"tree_sitter"`).
- Results are sorted by `crate_name` ascending. `root_deps` entries inside each record are also sorted and deduplicated (backed by `BTreeSet` internally).
- `src_root` is `manifest_path.parent()` — the directory containing `Cargo.toml`, not the `src/` subdirectory.

## Coupling

- **Phase 2 wiring lives in `service.rs`, not `deps.rs`.** `discover_dependency_packages` is called by `run_deps_index` in `service.rs`. The `deps` module itself remains a pure discovery module — no indexing logic is added here.
- **`discover_dependency_packages` is called once per `run_deps_index` invocation** (i.e., whenever `Cargo.lock` hash changes). There is no cross-call caching inside `deps.rs`; `service.rs` avoids repeat calls by gating on `ensure_deps_index_fresh`.
- **`ChunkOrigin::Dependency` is populated by `service.rs::run_deps_index`**, not by `deps.rs` or `chunker`. The chunker assigns `ChunkOrigin::Project` by default; `run_deps_index` overwrites `chunk.origin` after chunking.

## Common Mistakes

- **Passing a file path instead of a directory** — `MetadataCommand::current_dir` expects the project root directory, not a `Cargo.toml` path. Passing a file path makes `cargo metadata` fail with a non-obvious error.
- **Expecting `src_root` to point at source files directly** — `src_root` is the crate's manifest directory. Source files live under `src_root/src/`. `service.rs::collect_dep_rs_files` appends `src/` before walking.
- **Assuming non-empty `root_deps` for every package** — packages reachable only via optional features may have empty `root_deps`. The embedding annotation omits the `(dependency of: ...)` clause in that case; chunks are still indexed.
