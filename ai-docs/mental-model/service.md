# Service — Mental Model

## Entry Points

- `src/service.rs` — `DebriefService` trait, `InProcessService`, result types
- `src/main.rs` — only place that constructs and dispatches through the service

## Module Contracts

- `DebriefService` is **not object-safe**. It uses RPITIT (`impl Future<Output = ...> + Send` in trait methods). `Box<dyn DebriefService>` is a compile error.
- Every trait method receives `project_root: &Path` as its **first parameter**. This enables a single service instance (or daemon) to serve multiple workspaces without construction-time binding.
- `InProcessService` is a **zero-sized type** — no fields. Config is resolved from `project_root` per call when methods are fully implemented (currently stubs).
- `main.rs` resolves `project_root` from `std::env::current_dir()` and passes it to every service call. Config loading has been removed from `main.rs`.
- The trait requires all returned futures to be `Send`, enforcing that implementations must be usable in a multi-threaded tokio runtime.

## Trait Signature

```rust
pub trait DebriefService {
    fn index(&self, project_root: &Path, path: &Path) -> impl Future<Output = Result<IndexResult>> + Send;
    fn search(&self, project_root: &Path, query: &str, top_k: usize) -> impl Future<Output = Result<Vec<SearchResult>>> + Send;
    fn get_skeleton(&self, project_root: &Path, file: &Path) -> impl Future<Output = Result<String>> + Send;
    fn set_embedding_model(&self, project_root: &Path, model: &str, global: bool) -> impl Future<Output = Result<()>> + Send;
}
```

## Coupling

- `main.rs` is hard-coded to `InProcessService`. There is no runtime dispatch or feature flag yet. Phase 2 (`DaemonClient`) will require a conditional construction point in `main.rs`.

## Extension Points & Change Recipes

**Implementing a new service method:**

1. Add the method to the `DebriefService` trait in `src/service.rs` with `project_root: &Path` as first parameter.
2. Implement it on `InProcessService` (stub with `anyhow::bail!` is acceptable during scaffolding).
3. When Phase 2 `DaemonClient` exists, implement it there too — the compiler will enforce this.
4. Add the dispatch arm in `main.rs`, passing `&project_root`.

**Implementing a method body in InProcessService:**

- Call `crate::config::config_paths(project_root)` then `crate::config::load_config(&paths)` to derive config.
- No caching at the struct level — config is small and resolution is cheap.

**Adding Phase 2 DaemonClient:**

- Implement `DebriefService` for `DaemonClient`.
- `DaemonClient` will include `project_root` in each IPC request, mapping to a per-workspace `WorkspaceState` in the daemon (`HashMap<PathBuf, WorkspaceState>`).
- Because the trait is not object-safe, `main.rs` cannot use `Box<dyn DebriefService>` to switch at runtime. Options: (a) an `enum Service { InProcess(InProcessService), Daemon(DaemonClient) }` that implements the trait by delegating, or (b) conditional construction with two monomorphized code paths.

## Common Mistakes

- **Attempting `Box<dyn DebriefService>`** — fails to compile because RPITIT makes the trait non-object-safe. Use enum dispatch or monomorphization instead.
- **Adding a method to the trait without implementing it on `DaemonClient`** — compile error only surfaces once `DaemonClient` exists; until then it silently compiles but blocks Phase 2 integration.
- **Omitting `project_root` from a new trait method** — violates the multi-workspace contract; every operation must be workspace-addressed.
