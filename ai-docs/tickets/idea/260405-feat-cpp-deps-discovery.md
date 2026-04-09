---
title: "C++ Dependency Indexing — on-demand path-based with header-only default"
category: feat
priority: medium
related:
  - 260405-feat-cpp-chunker              # prerequisite — chunker must exist first
  - 260405-feat-on-demand-dep-indexing   # shared --dep infrastructure
  - 260404-feat-dependency-chunking      # Rust equivalent (completed, pre-index approach)
---

# C++ Dependency Indexing

## Goal

Enable on-demand indexing of C++ dependencies via `--dep <path>`. User
points to a dependency's include directory, headers are indexed and cached.
No build system parsing required.

## Motivation (revised)

Original approach (build system parsing + `#include` resolution) was
over-engineered. The `--dep` on-demand model (see
`260405-feat-on-demand-dep-indexing`) eliminates the need for vcxproj/CMake
parsing entirely. User explicitly provides the path — no discovery needed.

## Design Decisions

### Usage

```bash
# Index ASIO headers
debrief --dep /path/to/asio/include search "async_read"

# Index Boost headers
debrief --dep /usr/local/include/boost search "shared_ptr"

# Multiple deps
debrief --dep /path/to/asio --dep /path/to/fmt search "format"
```

### Header-only default

C++ deps are typically consumed via headers. Default indexing scope:
- `.h`, `.hpp`, `.hxx`, `.h++` files — indexed
- `.c`, `.cpp`, `.cc`, `.cxx` source files — excluded by default
- Override: `--dep-include-source` or config flag to include sources

This keeps dep indexes small and focused on the API surface.

### Relies on shared infrastructure

All caching, namespace separation, staleness checking, and CLI handling
comes from `260405-feat-on-demand-dep-indexing`. This ticket only adds:
- C++ header file extension filtering
- CppChunker integration for dep paths
- C++ specific embedding text annotation

## Phases

### Phase 1: C++ dep indexing via --dep

Depends on: `260405-feat-on-demand-dep-indexing` Phase 1-2,
`260405-feat-cpp-chunker` Phase 1.

- Register C++ file extensions in dep indexing pipeline
- Header-only filter (default) with source opt-in
- `[dep: <dirname>]` annotation in embedding text
- Integration test: index ASIO headers, verify search results

## Dropped (from original ticket)

The following were planned in the original ticket but are no longer
needed with the on-demand `--dep` approach:

- [dropped] `.vcxproj` XML parsing
- [dropped] CMake `CMakeLists.txt` parsing
- [dropped] `.sln` auto-discovery
- [dropped] `$(ProjectDir)` / `$(SolutionDir)` macro resolution
- [dropped] Recursive `#include` resolution
- [dropped] Build system auto-detection

These were the primary complexity sources. The user provides the path
directly, making all discovery logic unnecessary.

## Resolved Questions

- **Build system parsing** → Dropped. User provides path via `--dep`.
- **`#include` resolution** → Dropped. Index the directory tree as-is.
- **`.sln` project name in embedding text** → Replaced by `[dep: <dirname>]`
  using the dep directory name.
- **Depth / file count limit** → No limits. Per-dep indexes are small
  enough for CPU. Scale problem eliminated by on-demand model.
