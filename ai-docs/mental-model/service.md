# Service — Mental Model

## Entry Points

- `src/service.rs` — `DebriefService` trait, `InProcessService`, result types
- `src/main.rs` — only place that constructs and dispatches through the service

## Module Contracts

- `DebriefService` is **not object-safe**. It uses RPITIT (`impl Future<Output = ...> + Send` in trait methods). `Box<dyn DebriefService>` is a compile error.
- Every trait method receives `project_root: &Path` as its **first parameter**. This enables a single service instance (or daemon) to serve multiple workspaces without construction-time binding.
- `InProcessService` is a **zero-sized type** — no fields. Config is resolved from `project_root` per call.
- `main.rs` resolves `project_root` from `std::env::current_dir()` and passes it to every service call. Config loading has been removed from `main.rs`.
- The trait requires all returned futures to be `Send`, enforcing that implementations must be usable in a multi-threaded tokio runtime.
- **`search` and `overview` auto-index silently.** Both construct an `Embedder` upfront, then call `ensure_index_fresh(project_root, &embedder)`. If the on-disk index is missing, stale (commit changed), or was built with a different model, a full or incremental reindex runs transparently before returning results. The `index` method (exposed as `rebuild-index` CLI) always forces a full reindex regardless of staleness.
- **`search` merges dep chunks when `include_deps: true`.** After ensuring the project index is current, `search` calls `ensure_deps_index_fresh(project_root, &embedder)` only when `include_deps` is `true`. Dep chunks are appended to the flat chunk list before building the `SearchIndex`. When `include_deps` is `false`, dep reindexing is skipped entirely on the search path.
- **`index` triggers both project and dep indexing.** `InProcessService::index` runs `run_index` followed by `run_deps_index`. Both receive the same `Embedder` instance constructed once at the top of `index`.
- **`run_deps_index` applies the `exclude` list at indexing time, not at search time.** Crates in `config.dependencies.exclude` are dropped before chunking. Removing a crate from the exclude list requires a full `rebuild-index` to take effect — exclude changes are not picked up on the next implicit reindex unless `Cargo.lock` changed.
- **`Embedder` is constructed by callers, not internally in `run_index`.** `run_index` and `ensure_index_fresh` both accept `embedder: &Embedder` as a parameter. The model is resolved from config before calling these helpers.
- **Embedding is batched in groups of 64** (`EMBED_BATCH_SIZE` in `service.rs`). A progress line is written to stderr during indexing: `indexing` followed by one `.` per completed batch, then `\ndone. N chunks, M files.` Dep indexing prints `indexing deps` + dots + `\ndone. N dep chunks.` Both are unconditional stderr — not gated by `RUST_LOG`.
- **`index` ignores its `path` parameter.** `InProcessService::index` always indexes the full `project_root` tree via `git ls-files`. The `path` argument is accepted to satisfy the trait but is ignored (`_path`).

## Coupling

- `main.rs` uses `resolve_service(project_root)` which calls `daemon::auto_spawn_and_connect(project_root)` and wraps the result in `Service::new(client)`. `Service` is a **struct** (not an enum) holding `Option<DaemonClient>` and `InProcessService`. Each method tries the daemon first; on any `Err`, it logs `[daemon] error, falling back to in-process: ...` to stderr and retries the same operation on `InProcessService`. Daemon errors never propagate to callers.
- `IndexResult` and `SearchResult` derive `Serialize`/`Deserialize` — required for IPC transport. Any new field on these types must be serializable or the build fails.

## Extension Points & Change Recipes

**Implementing a new service method:**

1. Add the method to the `DebriefService` trait in `src/service.rs` with `project_root: &Path` as first parameter.
2. Implement it on `InProcessService`.
3. Implement it on `DaemonClient` — add variant to `DaemonRequest`/`DaemonResponse` in `src/ipc/protocol.rs`, add arm to `daemon::handle_request`, then call via `self.send(...)` in `DaemonClient`.
4. Add a `if let Some(d) = &self.daemon { ... }` arm in the new method on `Service` in `src/service.rs`, following the try-daemon-then-fallback pattern used by the existing methods.
5. Add dispatch arm in `main.rs`, passing `&project_root`.

**Implementing a method body in InProcessService:**

- Call `crate::config::config_paths(project_root)` then `crate::config::load_config(&paths)` to derive config.
- To write config, call `crate::config::load_layer_single(&target_path)?.unwrap_or_default()`, mutate, then `crate::config::save_config(&target_path, &config)`. See `set_embedding_model` as the reference implementation.
- No caching at the struct level — config is small and resolution is cheap.
- To read the project index, construct an `Embedder` from config first, then call `ensure_index_fresh(project_root, &embedder)` (returns `IndexData`). To write it, call `store::save_index(&index_path(project_root)?, &data)`.
- To read the dep index, call `ensure_deps_index_fresh(project_root, &embedder)` (returns `DepsIndexData`). Staleness is Cargo.lock-hash based, not commit-based.
- `dep_overview` reads `deps-index.bin` directly — it does **not** call `ensure_deps_index_fresh`. If the dep index is absent, it fails with an explicit error. This means `dep_overview` can return stale data without error if `Cargo.lock` changed since the last `rebuild-index`.

## Common Mistakes

- **Attempting `Box<dyn DebriefService>`** — fails to compile because RPITIT makes the trait non-object-safe. The `Service` struct is the dispatch mechanism.
- **Passing a different `project_root` to `DaemonClient` methods** — `DaemonClient` ignores the `project_root` parameter (`_project_root`). The daemon is bound to the workspace it was started with; requests run against that workspace regardless of the parameter value.
- **Omitting `project_root` from a new trait method** — violates the multi-workspace contract; every operation must be workspace-addressed.
- **Passing `path` to `InProcessService::index` and expecting scoped indexing** — the `path` parameter is ignored; the implementation always indexes `project_root` fully. A scoped path parameter on the trait is a forward-compat placeholder.
- **Expecting `search` or `overview` to fail fast on a missing index** — they will silently trigger a full reindex (including model download) before returning. This can cause unexpected latency on first call.
- **Calling `run_index` or `ensure_index_fresh` without constructing an `Embedder` first** — both functions require `embedder: &Embedder` as a parameter. The old pattern of constructing the embedder inside `run_index` was removed in Phase 2; callers are responsible for construction.
- **Calling `search` without `include_deps: true` and expecting dep results** — dep chunks are only merged into the search pool when `include_deps` is `true`. The CLI default is `include_deps = true` (controlled by the `--no-deps` flag).
- **Capturing only stdout when testing service output** — indexing progress (`indexing....done.`) goes to stderr unconditionally. Integration tests or tools that pipe only stdout will miss it; tests that scan stderr for clean output will see it.
- **Expecting daemon errors to propagate** — `Service` silently falls back to `InProcessService` on any daemon error, emitting only an `eprintln!`. A broken daemon causes silent performance degradation (InProcess is heavier than IPC), not a visible error. Daemon-bug investigation must inspect stderr for `[daemon] error` lines.
