# IPC — Mental Model

## Entry Points

- `src/ipc/mod.rs` — platform re-exports (cfg-gated Unix/Windows); entry point for callers
- `src/ipc/protocol.rs` — `DaemonRequest`/`DaemonResponse` enums, `read_message`/`write_message` framing

## Module Contracts

- `send_command` acquires an exclusive advisory lock (`flock` on Unix, `LockFileEx` on Windows) before writing any request. Concurrent clients are serialized. Bypassing this lock (calling `write_message` directly) causes interleaved request/response frames and silent data corruption.
- `read_message` enforces a 16 MB maximum payload size. Responses larger than this are rejected with an error — relevant if large dep indexes are ever serialized through IPC.
- `DaemonIpc::setup` (Unix) deletes existing FIFO files before creating new ones. Daemon restarts are idempotent; stale FIFOs from a previous crash are cleaned up at startup, not at shutdown.
- `ready_indicator` returns platform-specific paths: `debrief.req` on Unix (the FIFO itself), `debrief.ready` on Windows (a dedicated marker file). Code checking daemon readiness must use this function — do not hardcode either path.

## Coupling

- `ipc/protocol.rs` imports `IndexResult` and `SearchResult` from `service.rs` and requires them to derive `Serialize`/`Deserialize`. Adding a non-serializable field to either type breaks IPC compilation.
- `DaemonClient::connect` in `service.rs` calls `ipc::ready_indicator` to confirm the daemon's IPC endpoints are live. If this file is absent (daemon crashed after writing PID but before setting up IPC), `connect` returns `None` and the caller silently falls back to `InProcessService`.

## Extension Points & Change Recipes

**Adding a new message type:**

1. Add variants to `DaemonRequest` and/or `DaemonResponse` in `src/ipc/protocol.rs`.
2. Add dispatch arm in `daemon::handle_request` (`src/daemon.rs`).
3. Add send/receive in `DaemonClient` in `src/service.rs`.

**The framing protocol** (4-byte LE length prefix + JSON) is shared between Unix and Windows backends via `protocol::read_message` / `protocol::write_message`. Transport mechanics differ; framing is unified.

## Common Mistakes

- **Reading the Unix req FIFO without toggling O_NONBLOCK**: The daemon opens the req FIFO `O_RDWR | O_NONBLOCK`. `poll_request` switches to blocking mode only during the `read_message` call, then switches back. Adding a direct read on `req_fd` without the toggle returns `EAGAIN` silently (no data) even when data is present.
- **Hardcoding `debrief.req` or `debrief.ready` as a readiness check**: The ready indicator path differs by platform. Always use `ipc::ready_indicator(&daemon_dir)`.
- **Assuming response FIFO is empty before reading (Unix)**: The client drains stale bytes from the resp FIFO before sending a new request. This handles crashed-client leftovers. Skipping the drain causes `read_message` to return a stale response from a previous client's operation.
