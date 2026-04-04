---
title: "Daemon Mode — Per-Workspace Background Process"
category: feat
priority: medium
parent: null
plans: null
related:
  - 260403-research-rag-architecture  # architecture decisions in section 6
  - 260404-feat-dependency-chunking  # dep indexing increases index size, amplifies daemon value
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

### Phase 2C — Integration & Fallback

Transparent daemon spawn on first CLI use. Fallback to in-process when
daemon is unavailable. Index freshness notification (daemon detects
git changes and re-indexes).

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

- Temp-file RPC protocol details: request format, response format,
  polling vs notification, cleanup of stale temp files
- Index freshness in daemon: poll git HEAD on each request? Or rely on
  client to signal staleness?
- Concurrency: single-threaded tokio sufficient for per-workspace use?
