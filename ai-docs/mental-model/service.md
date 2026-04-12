# Service — Mental Model

## Entry Points

- `src/service.rs` — `DebriefService` trait, `InProcessService`, `DaemonClient`, `Service`, result types, all pipeline helpers
- `src/main.rs` — only place that constructs and dispatches through the service

## Module Contracts

- `DebriefService` is **not object-safe**. It uses RPITIT (`impl Future<Output = ...> + Send` in trait methods). `Box<dyn DebriefService>` is a compile error.
- Every trait method receives `project_root: &Path` as its **first parameter**. This enables a single service instance (or daemon) to serve multiple workspaces without construction-time binding.
- `InProcessService` is a **zero-sized type** — no fields. Config is resolved from `project_root` per call.
- `main.rs` resolves `project_root` from `std::env::current_dir()` and passes it to every service call. Config loading has been removed from `main.rs`.
- The trait requires all returned futures to be `Send`, enforcing that implementations must be usable in a multi-threaded tokio runtime.

### Pipeline (Phase 1 of ticket 260409)

The legacy `run_index` / `run_deps_index` / `ensure_index_fresh` /
`ensure_deps_index_fresh` helpers are gone. The Phase 1 pipeline is two
private free functions plus a small embedder helper:

- **`build_embedder(project_root) -> (Embedder, String)`** — single
  reference path for embedder construction. Loads merged config,
  resolves the model name (falling back to `ModelRegistry::DEFAULT_MODEL`),
  validates it via `ModelRegistry::lookup`, and downloads/loads model
  files into `dirs::data_dir()/debrief/models/`. Every service method
  that needs an embedder calls this helper — never inline the logic.
- **`load_or_rebuild_index(project_root, embedder, model_name, force_full) -> IndexData`** —
  staleness gate. Priority order: (1) `force_full == true` → full rebuild unconditionally;
  (2) missing file or version/backend mismatch → full rebuild (handled by `load_index`);
  (3) `embedding_model` differs → full rebuild; (4) otherwise, calls
  `apply_incremental_updates` and saves the index only if it changed.
  On full rebuild, calls `run_index_for_sources` then `stamp_all_git_states`.
- **`apply_incremental_updates(project_root, sources, data, embedder) -> bool`** —
  per-git-root incremental staleness. For each non-dep source: if the git root has
  no prior `git_states` entry, performs a full scan of all sources under that root;
  if HEAD changed, diffs committed changes; compares `dirty_files` hashes against
  the stored `DirtySnapshot` to detect working-tree changes and reverts; re-chunks
  and re-embeds only affected files. Deduplicates: each git root is processed at
  most once even if multiple sources share it. Returns `true` if any chunk was
  inserted or removed (caller should save). Non-git sources: full scan on first
  appearance (no chunks under that root), skip on subsequent calls (manual-only policy).
- **`stamp_all_git_states(project_root, sources, data)`** — records current HEAD +
  `dirty_files` snapshot for every non-dep git root reachable from sources; prunes
  `git_states` entries whose root is no longer referenced by any active source.
  Called after a full rebuild. Not called after incremental updates (`apply_incremental_updates`
  stamps each root it processes inline).
- **`scan_source_full(project_root, entry, embedder) -> HashMap<PathBuf, Vec<Chunk>>`** —
  full walkdir/ls-files scan of a single `SourceEntry`. Returns chunk-key → chunks
  pairs ready for insertion into `IndexData.chunks`. Used by incremental update
  (first-time source or git-diff fallback) and `rebuild_source`.
- **`chunk_file(abs_path, file_key, language) -> Vec<Chunk>`** — reads and chunks a
  single file. Returns `vec![]` on read or chunker failure (logged as warnings).
- **`embed_chunks(chunks, embedder) -> Result<()>`** — embeds a slice of chunks in-place.
- **`run_index_for_sources(project_root, sources, embedder) -> IndexData`** —
  the full rebuild pipeline. Wrapped in `tracing::info_span!("rebuild_index")`
  with four nested stage spans (`file_discovery`, `chunking`,
  `embedding`, `index_build`) for per-stage timing under `RUST_LOG=info`.
  The caller (`load_or_rebuild_index`) is responsible for resolving the
  source list — `run_index_for_sources` does not call `resolve_sources`
  itself, so tests can inject a synthetic source list.

### Pipeline stage contracts

1. **Source partitioning.** Sources with `dep == true` are dropped
   (Phase 1 only) with a `tracing::warn!`. Phase 3 will reintroduce them
   under a separate per-dep namespace at `.debrief/deps/<key>.bin`.
2. **`file_discovery` span.** Per-source dispatch — each `SourceEntry`
   is classified as **external** or **project** and collected independently.
   A source is external when `abs_root != project_root && abs_root.join(".git").exists()`
   (foreign cloned sub-repo) OR when `abs_root.strip_prefix(project_root)` fails
   (entirely outside the project tree). External sources are walked with
   `walkdir::WalkDir::new(&abs_root)` (follow_links=false); project sources
   use a lazily-populated `git::changed_files(project_root, None)` result
   that is fetched at most once and shared across all project entries.
   All output paths are relative to `project_root`; for fully-external abs
   roots the stored key is an absolute path (POSIX `join` of an absolute path
   supersedes the prefix, so `project_root.join(abs_key)` resolves correctly).
   The effective extension set is `entry.extensions` if `Some`, otherwise
   the language default (`Rust → ["rs"]`, `Cpp → ["cpp", "cc", "cxx", "c",
   "h", "hpp", "hxx", "hh"]`). Paths matched by multiple sources with the
   **same** language are silently unified; paths matched by multiple sources
   with **different** languages keep the first registration and emit a
   `tracing::warn!`. After discovery a `[discovery] N files across M sources`
   line is printed unconditionally to stderr.
3. **`chunking` span.** Iterates `(relative_path, language)` pairs,
   reads source bytes, dispatches via `chunker_for(&language)`. Per-file
   read or chunk failures log a `tracing::warn!` and skip the file —
   one broken file never aborts the rebuild. The `relative_path` is
   passed to `Chunker::chunk` so `RustChunker::derive_module_path` can
   compute `crate::foo::bar` style module paths correctly.
4. **`embedding` span.** Flattens chunks into a single mutable-ref list
   in stable iteration order, batches by `EMBED_BATCH_SIZE = 64`, calls
   `embedder.embed_batch(&texts)`, assigns vectors back into
   `chunk.embedding = Some(vec)`. Progress uses `std::io::IsTerminal` to
   detect TTY on stderr: on a TTY each batch overwrites the previous line
   with `\r[embedding]  batch N/total  (X chunks/s, ETA Ys)` (padded to
   80 chars), followed by a final `\n`; on non-TTY each batch is a
   separate `eprintln!` line. After all batches a
   `done. N chunks, M files.  total Xs` summary is printed
   unconditionally. All progress output goes to stderr and ignores
   `RUST_LOG`. The `tracing` span coexists with it.
   Per-file chunking progress (`[chunking]   file N/M  <path>`) follows
   the same TTY/non-TTY pattern immediately before the embedding phase.
5. **`index_build` span.** Constructs `IndexData::new()` (the `version`
   and `backend` fields are private and cannot be set via struct literal
   from outside `store.rs`), assigns `data.chunks = map`, returns it.
   `git_states` and `embedding_model` are stamped by `load_or_rebuild_index`
   (via `stamp_all_git_states`), not by this stage.

### Method behavior

- **`index(project_root, _path, _include_deps)`** — full rebuild via
  `load_or_rebuild_index(.., force_full = true)`. The `path` parameter
  is ignored (forward-compat placeholder). `include_deps` is a no-op in
  Phase 1, logged via `tracing::debug!`. Counts files as
  `data.chunks.len()` and chunks as `sum(v.len() for v in
  data.chunks.values())`. When `rebuild-index --source <path>` is used,
  `main.rs` dispatches to `rebuild_source` instead — `index` is not called.
- **`search(project_root, query, top_k, _include_deps)`** — uses
  `load_or_rebuild_index(.., force_full = false)` so a missing or stale
  index triggers a silent rebuild (including model download).
  Constructs a fresh `SearchIndex` from the flattened chunks every
  call; the index is not cached on `InProcessService` (which is
  zero-sized). `include_deps` is a no-op in Phase 1.
- **`overview(project_root, file)`** — silent rebuild, then
  `data.chunks.get(&relative_file)`. Filters to `ChunkType::Overview`
  chunks, sorts by visibility (`Pub → PubCrate → PubSuper → Private`),
  joins `display_text` with blank lines. Missing index entry → `bail!`
  with `"no index entries for {file}"`. Empty overview chunk list →
  `bail!` with `"no overview chunks found for {file}"`.
- **`dep_overview`** — `bail!` with `"dependency overview not yet
  available (Phase 3 of ticket 260409)"`. Non-panicking — the CLI
  exits cleanly.
- **`set_embedding_model(project_root, model, global)`** — validates
  via `ModelRegistry::lookup`, then writes to the global or project
  config layer using the `load_layer_single → mutate → save_config`
  pattern (the reference implementation for any single-layer write).
- **`rebuild_source(project_root, source_root)`** — force a full re-index
  of one registered source identified by `source_root` (canonicalized for
  comparison against `SourceEntry.root`). Returns `Err` if no matching
  source is found. Clears all existing chunks under that source root before
  re-scanning. Updates `git_states` for the source's git root afterward.
  Dispatched from `main.rs` when `rebuild-index --source <path>` is used;
  the `DaemonClient` implementation is a `bail!` stub (falls back to
  `InProcessService` via `Service`).
- **`add_source` / `list_sources` / `remove_source`** — thin
  delegations to `config::append_source` / `config::resolve_sources` /
  `config::remove_source_at`. `list_sources` honors the Cargo.toml
  backward-compat fallback in `resolve_sources`.

## Coupling

- `main.rs` uses `resolve_service(project_root)` which calls `daemon::auto_spawn_and_connect(project_root)` and wraps the result in `Service::new(client)`. `Service` is a **struct** (not an enum) holding `Option<DaemonClient>` and `InProcessService`. Each method tries the daemon first; on any `Err`, it logs `[daemon] error, falling back to in-process: ...` to stderr and retries the same operation on `InProcessService`. Daemon errors never propagate to callers.
- `IndexResult` and `SearchResult` derive `Serialize`/`Deserialize` — required for IPC transport. Any new field on these types must be serializable or the build fails.
- **DaemonClient source-registration and per-source rebuild methods are stubs.**
  `add_source`, `list_sources`, `remove_source`, and `rebuild_source` on
  `DaemonClient` return `bail!` unconditionally because no IPC variant exists for
  them yet. The `Service` wrapper sees the `Err` and falls back to `InProcessService`,
  which is the path that owns the project config file anyway. Phase 1
  intentionally avoids touching `ipc/protocol.rs` and `daemon.rs`; a
  later phase adds the IPC variants and removes the bail stubs.
- **`DepsIndexData` is gone.** Phase 1 removed the type along with
  `save_deps_index` / `load_deps_index` / `DEPS_INDEX_VERSION`. Old
  `deps-index.bin` files on disk are simply ignored. Phase 3
  reintroduces dep storage under `.debrief/deps/<key>.bin` with a
  different shape.

## Extension Points & Change Recipes

**Implementing a new service method:**

1. Add the method to the `DebriefService` trait in `src/service.rs` with `project_root: &Path` as first parameter.
2. Implement it on `InProcessService`.
3. Implement it on `DaemonClient` — add variant to `DaemonRequest`/`DaemonResponse` in `src/ipc/protocol.rs`, add arm to `daemon::handle_request`, then call via `self.send(...)` in `DaemonClient`. (Or, if you have no IPC variant yet, return `anyhow::bail!` so the `Service` wrapper falls back to `InProcessService`.)
4. Add a `if let Some(d) = &self.daemon { ... }` arm in the new method on `Service` in `src/service.rs`, following the try-daemon-then-fallback pattern used by the existing methods.
5. Add dispatch arm in `main.rs`, passing `&project_root`.

**Implementing a method body in InProcessService:**

- Call `crate::config::config_paths(project_root)` then `crate::config::load_config(&paths)` to derive config.
- To write config, call `crate::config::load_layer_single(&target_path)?.unwrap_or_default()`, mutate, then `crate::config::save_config(&target_path, &config)`. See `set_embedding_model` as the reference implementation.
- No caching at the struct level — config is small and resolution is cheap.
- To read or refresh the project index, construct an `Embedder` via `build_embedder(project_root)` first, then call `load_or_rebuild_index(project_root, &embedder, &model_name, force_full)`. Use `force_full = true` for an unconditional rebuild (e.g. the `index` method); use `force_full = false` for incremental staleness-based refresh (e.g. `search`, `overview`).

## Common Mistakes

- **Attempting `Box<dyn DebriefService>`** — fails to compile because RPITIT makes the trait non-object-safe. The `Service` struct is the dispatch mechanism.
- **Passing a different `project_root` to `DaemonClient` methods** — `DaemonClient` ignores the `project_root` parameter (`_project_root`). The daemon is bound to the workspace it was started with; requests run against that workspace regardless of the parameter value.
- **Omitting `project_root` from a new trait method** — violates the multi-workspace contract; every operation must be workspace-addressed.
- **Passing `path` to `InProcessService::index` and expecting scoped indexing** — the `path` parameter is ignored; the implementation always indexes the full set of project sources resolved from config.
- **Expecting `search` or `overview` to fail fast on a missing index** — they will silently trigger a full rebuild or incremental update (including model download on first run) before returning. This can cause unexpected latency on first call.
- **Calling `run_index_for_sources` without first resolving the source list** — the helper does not call `config::resolve_sources` itself. `load_or_rebuild_index` is the canonical caller; tests that drive the pipeline directly must pass a `&[SourceEntry]` slice in.
- **Calling `run_index_for_sources` and expecting `git_states` / `embedding_model` to be set** — both are empty/`None` on the returned `IndexData`. `load_or_rebuild_index` stamps them via `stamp_all_git_states` and direct assignment.
- **Expecting `--no-deps` to do anything in Phase 1** — `include_deps` is parsed and passed through, but the body is a `tracing::debug!` no-op. Dep indexing reactivates in Phase 3 of ticket 260409 under a per-dep namespace.
- **Expecting `dep_overview` to return data** — Phase 1 always returns `Err`. The previous `deps-index.bin` storage was removed; Phase 3 will reintroduce dep storage and re-enable this method.
- **Capturing only stdout when testing service output** — all indexing progress (`[discovery]`, `[chunking]`, `[embedding]`, `done.`) goes to stderr unconditionally. Integration tests or tools that pipe only stdout will miss it.
- **Expecting daemon errors to propagate** — `Service` silently falls back to `InProcessService` on any daemon error, emitting only an `eprintln!`. A broken daemon causes silent performance degradation (InProcess is heavier than IPC), not a visible error. Daemon-bug investigation must inspect stderr for `[daemon] error` lines.
