# Daemon — Mental Model

## Entry Points

- `src/daemon.rs` — daemon entry point (`run_daemon`), PID management, async/sync IPC bridge

## Module Contracts

- Each daemon instance binds to exactly one `project_root` at startup. `DaemonRequest` carries no workspace path — all operations use the root passed to `run_daemon`. Connecting a `DaemonClient` to a daemon started for workspace B and calling it with workspace A's root silently runs operations on workspace B.
- The `_pid_lock` (flock on `daemon.pid`) must remain alive for the daemon's entire lifetime. It is returned from `write_pid_file` and held in `run_daemon` as `_pid_lock`. Dropping it early allows a second daemon to start for the same workspace without error.
- `run_daemon` exits only after the `ipc_loop` blocking thread returns. Shutdown is cooperative: a `Stop` request causes the async loop to break, closing the `mpsc` sender, which causes `ipc_loop` to detect a closed channel and exit.
- `is_daemon_running` checks liveness via `ipc::process_alive` (kill(pid, 0) on Unix / OpenProcess on Windows). It returns `false` for any PID that fails, including stale PIDs from crashed daemons, making it safe to use as a dead-daemon detector.
- `CARGO_DEBRIEF_DAEMON_TIMEOUT` env var overrides the 3-minute idle timeout. The idle timer resets on each received request; timeouts fire only when `poll_request` returns `None` for the full duration.

## Coupling

- `daemon.rs` owns the IPC lifecycle: it calls `ipc::DaemonIpc::setup` to create endpoints and `ipc::cleanup_ipc_files` on shutdown. The IPC files exist only while a daemon is running.
- `handle_request` dispatches to `InProcessService` methods directly. Every new `DebriefService` method requires a matching arm here and a new `DaemonRequest`/`DaemonResponse` variant in `ipc/protocol.rs`.
- `kill_stale_daemon` sends `SIGTERM` on Unix but sends `DaemonRequest::Stop` via IPC on Windows — the kill path is platform-asymmetric.

## Extension Points & Change Recipes

**Adding a new service operation to the daemon:**

1. Add variant to `DaemonRequest` and `DaemonResponse` in `src/ipc/protocol.rs`.
2. Add arm to `handle_request` in `src/daemon.rs`, dispatching to the `InProcessService` method.
3. Implement the operation in `DaemonClient` in `src/service.rs`.
4. The compiler enforces step 3 via the `DebriefService` trait.

## Common Mistakes

- **Dropping `_pid_lock` early**: The flock advisory lock on `daemon.pid` is held only as long as the returned `File` handle lives. If the lock file handle is dropped before `cleanup()`, a concurrent daemon startup succeeds without error, resulting in two daemons for the same workspace.
- **Sending requests expecting the daemon to use a different workspace**: The daemon ignores the `project_root` parameter inside `DaemonClient` methods. The binding is the workspace passed to `run_daemon` at process start, which is embedded in the daemon directory path (`.git/debrief/daemon/` under the workspace's git root).

## Technical Debt

- Auto-spawn not implemented (Phase 2C): `resolve_service()` in `main.rs` falls back to `InProcessService` when no daemon is running. Users must start the daemon manually via an undocumented `__daemon` entry point.
- `daemon_dir` duplicates the git-root-walk from `config.rs`, `service.rs::index_path`, and `service.rs::deps_index_path`. There are now four independent copies of this logic.
