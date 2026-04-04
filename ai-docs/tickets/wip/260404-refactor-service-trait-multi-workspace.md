---
title: "DebriefService trait — multi-workspace support via project root parameter"
parent: 260403-epic-mvp-implementation
related:
  - 260404-feat-cli-scaffold-config-service  # original trait definition
  - 260404-feat-core-indexing-pipeline  # depends on trait shape
plans:
  phase-1: null
started: 2026-04-04
---

# DebriefService Trait — Multi-Workspace Support

Refactor `DebriefService` trait so every operation receives its project
root explicitly. This enables a single daemon instance to serve
multiple workspaces without an external routing/multiplexing layer.

## Motivation

The current trait methods (`index`, `search`, `get_skeleton`,
`set_embedding_model`) take no workspace parameter — they are
implicitly single-workspace, bound at construction time. This works
for in-process mode (one CLI invocation = one workspace) but forces
the daemon to maintain a hidden routing layer mapping IPC requests
to per-workspace `InProcessService` instances.

By making project root a trait-level parameter, the daemon
implementation becomes straightforward: it receives the root in each
request and manages its internal state accordingly. No separate
multiplexing abstraction needed.

## Design Decisions

1. **Daemon-default, in-process-fallback.** The intended runtime model
   is: CLI transparently spawns/connects to daemon; falls back to
   in-process only if daemon is unavailable. The trait should reflect
   this — operations are workspace-addressed, not implicitly bound.

2. **`project_root: &Path` on each method, not on construction.**
   Construction-time binding forces one instance per workspace. Method
   parameter allows a single daemon instance to serve N workspaces.
   `InProcessService` can either validate the root matches its config
   or re-derive config per call (lightweight — config is small).

3. **No session/connection abstraction.** Considered a "connect to
   workspace" → "operate" → "disconnect" model. Rejected: adds
   statefulness and failure modes (stale sessions, reconnection) with
   no benefit over stateless per-call root parameter.

4. **Config resolution moves inside the service.** Currently `main.rs`
   resolves config and passes it to `InProcessService::new()`. After
   this change, the service resolves config from the project root on
   each call (or caches internally). This keeps `main.rs` thin —
   it only needs the project root, not the full config.

## Phase 1: Trait Signature + InProcessService Update

### Goal

Update `DebriefService` trait methods to accept `project_root: &Path`.
Update `InProcessService` and `main.rs` dispatch accordingly. All
existing tests continue to pass.

### Scope

**`src/service.rs`:**

- Add `project_root: &Path` parameter to `index`, `search`,
  `get_skeleton`, `set_embedding_model`.
- `InProcessService` no longer needs `Config` at construction —
  it can derive config from project root per call, or keep a cache.
- Result types unchanged.
- Consider whether `InProcessService` should hold any state at all
  or become a zero-sized type.

**`src/main.rs`:**

- Resolve project root from CLI args or cwd.
- Pass project root to each service method call.
- Remove config resolution from main — let the service handle it.

**Tests:**

- Update existing service tests to pass a project root (tempdir or
  test fixture).
- Verify config resolution works when driven from project root.

### Success Criteria

- All trait methods accept `project_root: &Path`.
- `InProcessService` works without pre-resolved config.
- `main.rs` dispatches with project root only.
- All existing tests pass.
- New test: calling with different project roots produces independent
  results (no cross-contamination).

## Future

- Phase 2 (daemon mode) implements `DaemonClient` that sends project
  root over IPC with each request.
- Daemon-side `DaemonService` manages a `HashMap<PathBuf, WorkspaceState>`
  internally, keyed by project root.
- MCP server can use the same trait — each MCP request includes the
  workspace context.
