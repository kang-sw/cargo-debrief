# Service — Mental Model

## Entry Points

- `src/service.rs` — `DebriefService` trait, `InProcessService`, result types
- `src/main.rs` — only place that constructs and dispatches through the service

## Module Contracts

- `DebriefService` is **not object-safe**. It uses RPITIT (`impl Future<Output = ...> + Send` in trait methods). `Box<dyn DebriefService>` is a compile error.
- `InProcessService` holds a `Config` but currently ignores it in all methods (stubs). No implicit state beyond the config snapshot taken at construction.
- The trait requires all returned futures to be `Send`, enforcing that implementations must be usable in a multi-threaded tokio runtime.

## Coupling

- `main.rs` is hard-coded to `InProcessService`. There is no runtime dispatch or feature flag yet. Phase 2 (`DaemonClient`) will require a conditional construction point in `main.rs`.

## Extension Points & Change Recipes

**Implementing a new service method:**

1. Add the method to the `DebriefService` trait in `src/service.rs`.
2. Implement it on `InProcessService` (stub with `anyhow::bail!` is acceptable during scaffolding).
3. When Phase 2 `DaemonClient` exists, implement it there too — the compiler will enforce this.
4. Add the dispatch arm in `main.rs`.

**Adding Phase 2 DaemonClient:**

- Implement `DebriefService` for `DaemonClient`.
- Because the trait is not object-safe, `main.rs` cannot use `Box<dyn DebriefService>` to switch at runtime. Options: (a) an `enum Service { InProcess(InProcessService), Daemon(DaemonClient) }` that implements the trait by delegating, or (b) conditional construction with two monomorphized code paths.

## Common Mistakes

- **Attempting `Box<dyn DebriefService>`** — fails to compile because RPITIT makes the trait non-object-safe. Use enum dispatch or monomorphization instead.
- **Adding a method to the trait without implementing it on `DaemonClient`** — compile error only surfaces once `DaemonClient` exists; until then it silently compiles but blocks Phase 2 integration.
