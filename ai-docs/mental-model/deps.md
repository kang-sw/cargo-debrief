---
domain: deps
description: "Cargo dependency discovery: MetadataCommand shell-out, BFS root-dep attribution (currently unused in Phase 1 pipeline)"
sources:
  - src/
related:
  service: "Phase 3 of 260409-epic-multi-language-sources will re-wire discover_dependency_packages into the indexing pipeline"
---

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

- `deps.rs` is a **pure discovery module** — no indexing logic lives here.
- **`discover_dependency_packages` is not called in Phase 1** of the indexing pipeline. Sources with `dep == true` are currently dropped with a `tracing::warn!` inside `load_or_rebuild_index`. Phase 3 of ticket `260409-epic-multi-language-sources` will re-wire it under `.debrief/deps/<key>.bin` storage.
- When dep indexing resumes, `ChunkOrigin::Dependency` must be set explicitly by the pipeline (the chunker always emits `ChunkOrigin::Project` by default).

## Common Mistakes

- **Passing a file path instead of a directory** — `MetadataCommand::current_dir` expects the project root directory, not a `Cargo.toml` path. Passing a file path makes `cargo metadata` fail with a non-obvious error.
- **Expecting `src_root` to point at source files directly** — `src_root` is the crate's manifest directory (`Cargo.toml` parent). Source files live under `src_root/src/` — callers must append `src/` before walking.
- **Assuming non-empty `root_deps` for every package** — packages reachable only via optional features may have empty `root_deps`. The embedding annotation omits the `(dependency of: ...)` clause in that case; chunks are still indexed.
