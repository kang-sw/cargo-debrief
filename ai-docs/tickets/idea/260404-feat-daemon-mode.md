---
title: "Phase 2 — Daemon Mode"
category: feat
priority: medium
parent: null
plans: null
related:
  - 260403-research-rag-architecture  # architecture decisions in section 6
  - 260404-refactor-service-trait-multi-workspace  # trait already accepts project_root per call
---

# Phase 2 — Daemon Mode

## Goal

Add a background daemon that holds indexes in memory and serves CLI
requests over IPC. The `DebriefService` trait already accepts
`project_root: &Path` per method, so the daemon naturally supports
multiple workspaces without construction-time binding.

## Architecture (from research)

- **Single binary.** Daemon runs as `cargo debrief daemon`, not a
  separate executable.
- **Lazy spawn.** First CLI invocation spawns the daemon if not running.
- **Per-machine singleton.** One daemon serves all CLI invocations and
  sessions.
- **Idle expiry.** Daemon auto-exits after configurable idle timeout.
- **Fallback.** CLI detects daemon availability and falls back to
  in-process mode (`InProcessService`) if the daemon is unreachable.
- **IPC mechanism TBD.** Candidates: Unix domain socket, named pipe,
  HTTP on localhost.

## Phases

_To be detailed when this ticket moves to `todo/`._

### Phase 2A — Daemon Process Lifecycle

Daemon spawn, PID file, idle timeout, `daemon status`/`daemon stop`
subcommands.

### Phase 2B — IPC Transport

Choose and implement IPC mechanism. `DaemonClient` implements
`DebriefService` — CLI dispatches through it when daemon is available.

### Phase 2C — Integration & Fallback

Transparent daemon spawn on first CLI use. Fallback to in-process when
daemon is unavailable. Index sharing and cache coherence across
concurrent requests.

## Development Notes

### Debug-build binary mismatch guard

In debug builds only (`#[cfg(debug_assertions)]`): when the CLI connects
to an existing daemon, compare the daemon's binary identity (e.g., build
timestamp, binary hash, or inode) against the current CLI binary. If they
differ, kill the stale daemon and respawn. This prevents zombie processes
from previous `cargo build` runs from interfering during development.

This guard should be compiled out in release builds — users should not
have their daemon killed unexpectedly.

## Open Questions

- IPC mechanism selection (Unix socket vs. named pipe vs. localhost HTTP)
- Daemon discovery: PID file location (`.git/debrief/daemon.pid`?
  XDG runtime dir?)
- Index memory sharing: does the daemon own all indexes, or can the CLI
  read the on-disk index directly when the daemon is down?
- Concurrency model: single-threaded tokio, or multi-threaded for
  parallel workspace indexing?
