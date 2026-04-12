---
title: "Fix file discovery for external source roots (non-project git repos)"
category: bug
priority: high
related:
  - 260409-epic-multi-language-sources
---

# Fix File Discovery for External Source Roots

## Problem

`run_index_for_sources` in `service.rs` uses a single `git ls-files` call on
the project root for all file discovery (full rebuild and incremental). This
works only for files tracked by the project's own git repo. Any source root
that is a separate git repository (has its own `.git` directory) — such as
cloned C++ libraries in `test-repos/` — or a non-git directory produces zero
files because the parent `git ls-files` is unaware of them.

**Reproducer:**
```
cargo debrief add cpp test-repos/nlohmann-json
cargo debrief rebuild-index
# → Indexed 0 files, 0 chunks created.
```

## Root Cause

`service.rs:299`:
```rust
let changes = git::changed_files(project_root, None)?;
let all_paths: Vec<PathBuf> = changes.added.iter().map(PathBuf::from).collect();
```

`git::changed_files(root, None)` calls `git -C <root> ls-files`. Subdirectories
that are separate git repos are opaque to the parent git. The subsequent
per-source filter loop (lines 302–344) can only match files already in
`all_paths`, so external repos match nothing.

## Fix

Replace the single up-front `git ls-files` with per-source file discovery:

1. **Project-root sources** (source root resolves to `"."` or is under the
   project git root and tracked by it): use the existing `git ls-files` result.
   For incremental runs (`from = Some(hash)`), use `git diff` as today.

2. **External sources** (source root has its own `.git`, or is not under the
   project git root): use `walkdir` on the source root, filtered by the
   source's extension set. Always treat as fully changed (no incremental
   diffing — we have no git reference to diff against). On future runs where
   the source has its own git HEAD, store `last_indexed_commit` per-source
   to enable incremental diffing later (Phase 2, see below).

**Detection heuristic**: `entry.root.join(".git").exists()` (covers both
`.git` directories and `.git` files used by submodules/worktrees).

## Implementation Sketch

In `run_index_for_sources`:

```rust
// --- replace the current single `git ls-files` block ---

let mut files: Vec<(PathBuf, Language)> = Vec::new();
let mut seen: HashMap<PathBuf, Language> = HashMap::new();

for entry in &project_sources {
    let abs_root = project_root.join(&entry.root);
    let ext_set = build_ext_set(entry);

    let is_external = abs_root.join(".git").exists()
        || !abs_root.starts_with(project_root);

    let candidates: Vec<PathBuf> = if is_external {
        // Filesystem walk — external git repo or non-git directory.
        walkdir::WalkDir::new(&abs_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let ext = e.path().extension()?.to_str()?.to_ascii_lowercase();
                ext_set.contains(&ext).then(|| {
                    // Store as path relative to project_root for consistency.
                    e.path().strip_prefix(project_root).ok()?.to_path_buf()
                })
            })
            .collect()
    } else {
        // Project git source — use git ls-files (or diff for incremental).
        git::changed_files(project_root, from_hash)?
            .added
            .iter()
            .map(PathBuf::from)
            .filter(|p| path_under_root(p, &entry_relative_root(&entry.root)))
            .filter(|p| lowercase_extension(p).map_or(false, |e| ext_set.contains(&e)))
            .collect()
    };

    for rel in candidates {
        match seen.get(&rel) {
            None => { seen.insert(rel.clone(), entry.language); files.push((rel, entry.language)); }
            Some(existing) if *existing == entry.language => {}
            Some(existing) => { warn!(...conflict...); }
        }
    }
}
```

`walkdir` is already in the dependency tree (check `Cargo.toml`; add if absent).

## Phases

### Phase 1 — Fix full-rebuild for external sources

Implement the per-source discovery logic above. Incremental support for
external git sources is out of scope — always do a full walk for external roots.
Project-root sources retain their git-based incremental behavior.

**Success criteria:**
- `add cpp test-repos/nlohmann-json && rebuild-index` produces non-zero file/chunk counts
- `add cpp test-repos/asio && rebuild-index` processes all `.hpp` files under `asio/include/`
- Existing Rust source (`.`) indexing behavior unchanged
- `cargo test` passes

### Phase 2 — Incremental indexing for external git sources (future)

Per-source `last_indexed_commit` storage so external git repos can diff their
own HEAD instead of full-walking on every rebuild. Defer until the basic walk
is proven.

## Constraints

- `walkdir` must not follow symlinks (`.follow_links(false)`) — C++ headers
  often use symlink trees.
- Path representation must remain relative to `project_root` throughout (the
  rest of the pipeline assumes this).
- External sources do NOT participate in the project-git incremental check
  (`last_indexed_commit` stamp). They are always re-walked on rebuild until
  Phase 2.
