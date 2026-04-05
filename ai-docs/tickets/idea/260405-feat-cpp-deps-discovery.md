---
title: "C++ Dependency Discovery — build system parsing and include resolution"
category: feat
priority: medium
parent: null
plans:
  phase-1: null
  phase-2: null
related:
  - 260405-feat-cpp-chunker            # prerequisite — chunker must exist first
  - 260404-feat-dependency-chunking    # Rust equivalent (cargo metadata)
---

# C++ Dependency Discovery

## Goal

Discover which C++ source and header files to index by parsing build
system configuration and recursively resolving `#include` directives.
Analogous to `deps.rs` (cargo metadata) for Rust, but adapted to C++
build system conventions.

## Motivation

C++ has no standard package manager or build metadata format. File
discovery requires parsing build system configs (Visual Studio `.vcxproj`,
CMake `CMakeLists.txt`) to extract source files and include paths, then
following `#include` directives to find transitive header dependencies.

Without this, the tool can only index explicitly listed files — missing
the vast majority of headers pulled in via relative includes.

## Design Decisions

### Include resolution algorithm

```
1. Parse build system config → include_dirs[] + source_files[]
2. Queue source_files
3. For each file in queue:
   a. Line-scan for #include "..." → resolve relative to current file dir,
      then search include_dirs in order
   b. Line-scan for #include <...> → search include_dirs only
      (system headers excluded by default)
   c. If resolved file not yet visited → add to queue
4. All discovered files → CppChunker
```

Text-level scanning, not AST. A simple regex extracts `#include`
directives. Conditional compilation (`#ifdef`) branches are all scanned
— over-inclusion is preferable to missing files.

### Intentionally skipped

- `#include MACRO_NAME` — macro-variable includes, unresolvable without
  preprocessor
- System headers (`/usr/include`, `/usr/local/include`) — excluded by
  default, configurable
- Circular includes — prevented by visited set

### Visual Studio `.vcxproj` parsing

XML format. Key elements:
- `<ClCompile Include="...">` — source files
- `<ClInclude Include="...">` — header files
- `<AdditionalIncludeDirectories>` — semicolon-separated include paths
- Macro resolution required for:
  - `$(ProjectDir)` — directory containing the `.vcxproj` file
  - `$(SolutionDir)` — directory containing the `.sln` file (if discoverable)
  - `$(Configuration)` — Debug/Release (user-configurable, default: Debug)
  - `$(Platform)` — x64/Win32 (user-configurable, default: x64)
- Other `$(...)` macros: best-effort resolve from PropertyGroup elements
  in the vcxproj, warn and skip if unresolvable

### CMake parsing

Lightweight extraction, not full CMake evaluation:
- `add_library` / `add_executable` — source file arguments
- `target_include_directories` / `include_directories` — include paths
- `${CMAKE_CURRENT_SOURCE_DIR}`, `${PROJECT_SOURCE_DIR}` — resolve from
  CMakeLists.txt location
- Nested `add_subdirectory` — follow recursively
- Generator expressions (`$<...>`) — skip, too complex without CMake eval

### Configuration

User-facing config for C++ project discovery:
- Build system type: auto-detect (look for `.vcxproj` / `CMakeLists.txt`)
  or explicit
- Additional include paths (manual override)
- System header exclusion list / threshold
- Solution/project file path (if not at workspace root)

## Phases

### Phase 1: Build system parsing

- `.vcxproj` XML parser: extract source files, include files,
  include directories with macro resolution
- `CMakeLists.txt` lightweight parser: extract source files,
  include directories
- Auto-detection: scan workspace for `.sln`/`.vcxproj`/`CMakeLists.txt`
- Output: `(Vec<PathBuf>, Vec<PathBuf>)` — (source_files, include_dirs)
- Unit tests with sample vcxproj/CMake files

### Phase 2: Recursive include resolution

- `#include` directive scanner (line-level regex)
- Include path resolution: `"..."` (relative-first) vs `<...>` (dirs-only)
- BFS/DFS traversal with visited set
- System header detection and exclusion
- Integration with CppChunker: discovered files → chunk pipeline
- Integration tests with a small multi-file C++ project

## Resolved Questions

- **`.sln` parsing** → Yes, auto-discover all projects in the solution.
  Parse `.sln` to find all `.vcxproj` references. Project name included
  in embedding text (e.g., `[project: MyEngine]`) so queries can surface
  project-level context.
- **Depth / file count limit** → No artificial limits. Scale via GPU
  acceleration (same philosophy as Rust dep indexing — 206K chunks solved
  by GPU, not caps). If a solution has thousands of transitive headers,
  index them all.

## Open Questions

- CMake: should we attempt to parse `find_package` results or just use
  explicitly listed include paths?
