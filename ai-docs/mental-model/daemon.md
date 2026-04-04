# Daemon — Mental Model

## Entry Points

- `src/daemon.rs` — daemon entry point (`run_daemon`), PID management, async/sync IPC bridge, auto-spawn (`auto_spawn_and_connect`)

## Module Contracts

- Each daemon instance binds to exactly one `project_root` at startup. `DaemonRequest` carries no workspace path — all operations use the root passed to `run_daemon`. Connecting a `DaemonClient` to a daemon started for workspace B and calling it with workspace A's root silently runs operations on workspace B.
- The `_pid_lock` (flock on `daemon.pid`) must remain alive for the daemon's entire lifetime. It is returned from `write_pid_file` and held in `run_daemon` as `_pid_lock`. Dropping it early allows a second daemon to start for the same workspace without error.
- `run_daemon` exits only after the `ipc_loop` blocking thread returns. Shutdown is cooperative: a `Stop` request causes the async loop to break, closing the `mpsc` sender, which causes `ipc_loop` to detect a closed channel and exit.
- `is_daemon_running` checks liveness via `ipc::process_alive` (kill(pid, 0) on Unix / OpenProcess on Windows). It returns `false` for any PID that fails, including stale PIDs from crashed daemons, making it safe to use as a dead-daemon detector.
- `CARGO_DEBRIEF_DAEMON_TIMEOUT` env var overrides the 3-minute idle timeout. The idle timer resets on each received request; timeouts fire only when `poll_request` returns `None` for the full duration.
- `auto_spawn_and_connect` acquires an exclusive flock on `daemon.pid` before spawning to serialize concurrent CLI invocations. The flock is dropped before calling `wait_for_readiness` — a concurrent spawner unblocked during that gap may also spawn a second daemon, but the second daemon self-bails in `write_pid_file` when it fails its own PID flock (R1 accepted limitation).
- Spawned daemon has all three stdio handles set to `Stdio::null()`. If the daemon crashes during startup, there is no visible error output — only the CLI's `spawning daemon.......failed.` stderr line is shown. Daemon startup errors are only diagnosable via a debugger or by running `__daemon` manually.

## Coupling

- `daemon.rs` owns the IPC lifecycle: it calls `ipc::DaemonIpc::setup` to create endpoints and `ipc::cleanup_ipc_files` on shutdown. The IPC files exist only while a daemon is running.
- `handle_request` dispatches to `InProcessService` methods directly. Every new `DebriefService` method requires a matching arm here and a new `DaemonRequest`/`DaemonResponse` variant in `ipc/protocol.rs`.
- `kill_stale_daemon` sends `SIGTERM` on Unix but sends `DaemonRequest::Stop` via IPC on Windows — the kill path is platform-asymmetric.
- `cleanup_stale` must be called inside the PID flock (as `auto_spawn_and_connect` does). Calling it outside the flock creates a TOCTOU window where a concurrent spawner can see a clean directory and also spawn.
- `check_binary_identity` and `spawn_daemon` both honor `CARGO_DEBRIEF_BIN` env var. Integration tests override this to inject a test binary without path fragility.

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

- `daemon_dir` duplicates the git-root-walk from `config.rs`, `service.rs::index_path`, and `service.rs::deps_index_path`. There are now four independent copies of this logic.
- R1 flock gap: the PID flock is released before `wait_for_readiness` to avoid holding it across a 30-second polling window. A concurrent spawner that was blocked on the flock can slip through and spawn a second daemon. The second daemon self-bails via its own PID flock in `write_pid_file`, so correctness is maintained, but a spurious daemon process is briefly created and killed.
