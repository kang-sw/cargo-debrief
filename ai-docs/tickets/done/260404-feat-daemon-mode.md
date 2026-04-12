---
title: "Daemon Mode — Per-Workspace Background Process"
category: feat
priority: medium
related:
  260403-research-rag-architecture: architecture decisions in section 6
  260404-feat-dependency-chunking: dep indexing increases index size, amplifies daemon value
started: 2026-04-04
completed: 2026-04-05
---

# Daemon Mode — Per-Workspace Background Process

## Goal

Add a per-workspace background daemon that holds ONNX model session and
HNSW index in memory, eliminating 2-4 seconds of startup overhead on
repeated CLI calls. Primary value: fast sequential queries from AI agents.

## Architecture

**Revised from original system-wide design.** Per-workspace is sufficient
because:
- ONNX model is ~130MB — duplicate across 2 workspaces is negligible
- HNSW index is per-workspace anyway (no sharing possible)
- Multi-workspace routing (`HashMap<PathBuf, WorkspaceState>`) is
  eliminated, significantly reducing complexity

### Core decisions

- **Single binary.** Daemon runs as `cargo debrief daemon`, not a
  separate executable.
- **Per-workspace.** One daemon per project root. Binds to a single
  workspace at spawn time.
- **Lazy spawn.** First CLI invocation spawns the daemon if not running.
- **Short idle expiry (~3 min).** Purpose is eliminating delay during
  burst usage (AI agent sessions), not long-term persistence.
- **Fallback.** CLI falls back to in-process mode (`InProcessService`)
  if daemon is unreachable. 2-4s startup is acceptable for fallback.
- **IPC: temp-file-based RPC.** Avoids sandbox restrictions that affect
  Unix sockets and named pipes. Reference: `cargo-brief` project uses
  this pattern (inspired by rust-analyzer LSP). Request/response via
  temp files in a known directory.
- **Discovery.** `.git/debrief/daemon.pid` (or similar workspace-local
  path). Per-workspace, so no global PID file needed.

### What the daemon holds in memory

| Resource | Cold load cost | Daemon benefit |
|----------|---------------|----------------|
| ONNX model session | 200-500ms | Query embedding drops to ~5ms |
| HNSW graph | 1-3s (rebuild from vectors) | Search drops to ~1ms |
| Deserialized index | 100-300ms | Already in memory |
| **Total** | **~2-4s** | **~10ms per query** |

## Phases

_To be detailed when this ticket moves to `todo/`._

### Phase 2A — Daemon Process Lifecycle

Daemon spawn, PID file, idle timeout, `daemon status`/`daemon stop`
subcommands. Temp-file RPC directory setup.

### Phase 2B — IPC Transport

Implement temp-file-based RPC protocol. `DaemonClient` implements
`DebriefService` — CLI dispatches through it when daemon is available.

### Result (6304de3) - 26-04-04

Phases 2A and 2B landed together. `src/daemon.rs` owns process lifecycle:
PID file at `.git/debrief/daemon/daemon.pid` with advisory flock, 3-minute
idle timeout (overridable via `CARGO_DEBRIEF_DAEMON_TIMEOUT`), and a debug
binary-identity guard (`#[cfg(debug_assertions)]`) that kills and respawns a
daemon built from a different binary.

IPC landed as `src/ipc/` — platform-abstracted transport (Unix FIFOs with
`poll(2)` / Windows atomic-rename with file polling), length-prefixed JSON
protocol in `ipc/protocol.rs`. `DaemonClient` in `service.rs` implements
`DebriefService`, with a `Service` enum dispatching `InProcess` vs `Daemon`
paths. CLI gains `daemon status` and `daemon stop` subcommands plus a hidden
`__daemon` entry point.

Key adaptation from original design: temp-file RPC replaced by Unix FIFOs for
lower latency; sandbox-compat concern was the original driver, but FIFOs proved
cleaner than temp-file polling. Auto-spawn deferred to Phase 2C as planned.

### Phase 2C — Integration & Fallback

Transparent daemon spawn on first CLI use. Fallback to in-process when
daemon is unavailable. Index freshness notification (daemon detects
git changes and re-indexes).

### Result (e18a4ac) - 26-04-04

`resolve_service()` in `main.rs` transparently auto-spawns the daemon on
first CLI invocation. Spawn path acquires an exclusive flock on `daemon.pid`
to serialize concurrent spawners (R1), runs `cleanup_stale` inside the flock
to close a TOCTOU window (R4), re-execs `cargo debrief __daemon`, and polls
readiness with a "spawning daemon.......done." progress line on stderr.

`Service` struct wraps `Option<DaemonClient>` + `InProcessService` — each
method tries the daemon first and falls back silently to in-process on any
error (R3/R6). Daemon stdio set to `Stdio::null()` to avoid EPIPE panics on
early exit. Integration tests added covering auto-spawn, fallback, and
concurrent invocation.

Key decisions baked in: R1 flock gap accepted (second daemon self-bails via
its own PID flock); fallback is silent with debug logging only; git-aware
re-indexing handled by the existing `InProcessService` on each request rather
than a separate daemon-side watch loop.

### Result (e589e0d) - 26-04-04 — Post-landing: Progress keepalive

Long operations (rebuild-index, stale-index search) exceeded the fixed 120s
`DaemonClient` timeout, causing silent fallback to a duplicate ONNX session.
Fixed by sending `DaemonResponse::Progress` heartbeats every 10s while a
handler runs. The client loops on response reads, resetting the timeout on
each `Progress`. `oneshot` channel in daemon changed to `mpsc` to allow
streaming. Readiness poll timeout bumped 30s → 60s.

### Result (08908e4) - 26-04-05 — Post-landing: NO_DAEMON diagnostic env var

Added `CARGO_DEBRIEF_NO_DAEMON` env var: when set, `resolve_service()` skips
auto-spawn and falls back to `InProcessService`. Required for GPU memory
profiling where `Stdio::null()` on the daemon hides RSS instrumentation
output. Doubles as a general debug escape hatch.

## Development Notes

### Debug-build binary mismatch guard

In debug builds only (`#[cfg(debug_assertions)]`): when the CLI connects
to an existing daemon, compare the daemon's binary identity (e.g., build
timestamp, binary hash, or inode) against the current CLI binary. If they
differ, kill the stale daemon and respawn. This prevents zombie processes
from previous `cargo build` runs from interfering during development.

Compiled out in release builds.

### IPC reference implementation

`cargo-brief` (sibling project at `../cargo-brief/`) implements
temp-file-based RPC for sandbox-compatible IPC. Investigate its approach
when starting Phase 2B.

## Open Questions

_All resolved at implementation time._

- **IPC protocol**: Unix FIFOs (not temp files) with length-prefixed JSON.
  Request/response documented in `src/ipc/protocol.rs`. Stale files cleaned
  by `ipc::cleanup_ipc_files` on daemon shutdown.
- **Index freshness**: Daemon holds no watch loop. `InProcessService` on each
  request detects git HEAD changes via the existing git-tracking layer and
  re-indexes as needed.
- **Concurrency**: Single-threaded tokio confirmed sufficient. One request
  processed at a time via the `ipc_loop` blocking thread + async bridge.
