---
title: "Fix incremental indexing — multi-repo git tracking, dirty HEAD, and per-source rebuild"
category: bug
priority: high
started: 2026-04-12
completed: 2026-04-12
related:
  260412-fix-external-source-file-discovery:
skeletons:
  phase-1: c4914ab
  phase-2: c4914ab
  phase-3: c4914ab
---

# Fix Incremental Indexing

## Background

`load_or_rebuild_index` uses `last_indexed_commit` only as a freshness gate
("should we rebuild at all?"), not as an input to partial re-indexing. Any
HEAD change triggers a full re-index of all files across all sources. On a
codebase with external C++ repos registered (~1700 files), a single doc
commit causes a 7+ minute re-index.

The original Key Design Decision — "Incremental re-indexing tracks git diff
between last-indexed commit and HEAD. Never re-index unchanged files." — was
never implemented. The infra stubs exist (`git::changed_files(from)`,
`last_indexed_commit` on `IndexData`) but are not wired together.

Expanded scope settled in design discussion 2026-04-12: beyond commit-based
incremental, the fix should also track dirty/untracked working-tree changes
and extend staleness tracking to every nested git repository under the
`.debrief` context — not just the project root.

## Decisions

**Dirty tracking via `git status --porcelain`**: Covers tracked-modified,
staged, deleted, and untracked files in one command. `.gitignore` is
respected, so build artifacts are filtered. Content hash (sha256) per dirty
file stored at index time; compared on next `ensure_index_fresh` to detect
re-edits of already-dirty files.

**Revert detection**: A file that was dirty when last indexed but is now
neither dirty nor in the commit diff was reverted (`git checkout -- file`).
Without explicit detection, stale dirty chunks persist indefinitely. Handled
by iterating `dirty_snapshot.file_hashes` O(n) on every freshness check —
dirty file count is small, cost is negligible.

**Multi-repo via git root discovery**: Each `SourceEntry.root` resolves to a
git root by walking parent directories for `.git` (file or directory). Each
discovered git root gets an independent `GitRepoState`. Submodules, shallow
clones, and plain nested repos are all handled uniformly — no special
submodule logic needed.

**Non-git sources — manual-only after first scan**: Sources with no `.git`
ancestor auto-scan once (when no chunks exist for that source). Subsequent
refreshes require explicit `rebuild-index --source <path>`. These are
typically stable external libraries where periodic manual refresh is
acceptable.

**Rejected — `git diff HEAD --name-only` for dirty tracking**: Does not
include untracked files. A new file should be indexed immediately on first
`search` without requiring `git add`. `git status --porcelain` is a strict
superset and equally fast.

## Constraints

- `rebuild-index` (no flags) remains force-full for all sources.
- HNSW is always rebuilt from full `data.chunks` after any patch — no
  incremental ANN structure update.
- `DepsIndexData` staleness (Cargo.lock hash) is unchanged.
- INDEX_VERSION bump is required; old indexes are silently discarded
  (`Ok(None)`) and trigger a one-time full re-index on next run.

## Prior Art

- `git::changed_files(root, from: Option<&str>)` — already supports diff
  mode; returns `FileChanges { added, modified, removed }`.
- `IndexData.chunks: HashMap<PathBuf, Vec<Chunk>>` — per-file structure
  already supports surgical remove/insert.
- `last_indexed_commit: Option<String>` on `IndexData` — currently stored
  and read; replaced by `git_states` map in Phase 1.

## Phases

### Phase 1: Data structures + git root discovery

**Goal**: Introduce `GitRepoState`, `DirtySnapshot`, and the git-root
discovery helper. Migrate `IndexData` from the single `last_indexed_commit`
field to a `git_states` map. Bump INDEX_VERSION.

**Data structures**:

```rust
pub struct DirtySnapshot {
    // sha256 of file content at index time; key relative to git root
    pub file_hashes: HashMap<PathBuf, [u8; 32]>,
}

pub struct GitRepoState {
    pub last_indexed_commit: String,
    pub dirty_snapshot: DirtySnapshot,
}

// IndexData change:
// - remove: last_indexed_commit: Option<String>
// - add:    git_states: HashMap<PathBuf, GitRepoState>  // key = git root absolute path
// INDEX_VERSION 7 → 8
```

**Git root discovery** (add to `git.rs`):

```rust
/// Walk parent directories looking for `.git` (file or directory).
/// Returns the directory that contains `.git`, or None if not found.
/// Handles both regular `.git/` dirs and `.git` files (worktrees, submodules).
pub fn find_git_root(path: &Path) -> Option<PathBuf>
```

Pure filesystem traversal — no subprocess.

**Success criteria**: `IndexData` round-trips through bincode with a
populated `git_states` map. Version mismatch returns `Ok(None)`. Unit test:
serialize/deserialize with at least one `GitRepoState` entry.

### Phase 2: Wire staleness check

**Goal**: Replace the current "any HEAD change → full rebuild" logic with a
per-git-root incremental check covering commit changes, dirty changes, and
reverts.

**Staleness algorithm** (per `SourceEntry`, replaces current
`load_or_rebuild_index` freshness logic):

```
git_root = find_git_root(source.root)

if git_root is None:
    if no chunks exist for any file under source.root → queue full scan
    else → skip (manual-only policy)
    continue

state = index_data.git_states.get(&git_root)  // None on first run

current_head  = git -C <git_root> rev-parse HEAD
current_dirty = parse(git -C <git_root> status --porcelain)
                → HashMap<repo-relative PathBuf, sha256: [u8;32]>

commit_changed = ∅
if state.is_some() && state.last_indexed_commit != current_head:
    commit_changed = git::changed_files(&git_root, Some(&state.last_indexed_commit))

dirty_changed = ∅
for (path, hash) in &current_dirty:
    if state.map_or(true, |s| s.dirty_snapshot.file_hashes.get(path) != Some(hash)):
        dirty_changed.insert(path)

reverted = ∅
if let Some(state) = &state:
    for path in state.dirty_snapshot.file_hashes.keys():
        if !current_dirty.contains_key(path) && !commit_changed.contains(path):
            reverted.insert(path)  // was dirty, now clean → re-index to HEAD version

files_to_reindex = commit_changed ∪ dirty_changed ∪ reverted
```

After patching `IndexData.chunks`, update the `git_states` entry:

```
git_states.insert(git_root, GitRepoState {
    last_indexed_commit: current_head,
    dirty_snapshot: DirtySnapshot { file_hashes: current_dirty },
})
```

Rebuild HNSW from full `data.chunks` after all sources are processed (same
as today).

**Dead-state cleanup**: On each full-refresh pass, remove `git_states`
entries whose git root is no longer referenced by any active `SourceEntry`.
Prevents unbounded growth when sources are removed from config.

**`git.rs` additions** needed: `current_head(root: &Path) -> Result<String>`
and `dirty_files(root: &Path) -> Result<HashMap<PathBuf, [u8; 32]>>` that
parse `git -C <root> status --porcelain` output and compute sha256 per file.

**Success criteria**:
- Editing one source file and calling `search` re-indexes only that file.
- Creating a new untracked file and calling `search` indexes it immediately.
- Reverting a file (`git checkout -- <file>`) and calling `search` re-indexes
  it to the HEAD version.
- A doc-only commit on the project root causes no C++ source file re-indexing.
- Each nested git repo (e.g., `test-repos/asio`) tracks its own HEAD
  independently.

### Phase 3: `rebuild-index --source <path>` CLI

**Goal**: Allow targeted force-rebuild of a single source. Primarily useful
for non-git sources that have no auto-refresh path.

**CLI change**: Add `--source <path>` flag to `rebuild-index`. Matches the
flag value against `SourceEntry.root`. When specified:
- Full walk/ls-files for that source only (git or non-git).
- Clears all existing chunks whose path falls under that source root.
- Re-chunks, re-embeds, updates `git_states` (if git) or leaves it absent
  (if non-git).
- Rebuilds HNSW.

Existing `rebuild-index [<project-root>]` (no `--source`) remains force-full
for all sources.

**Success criteria**: `rebuild-index --source ./test-repos/asio` re-processes
only files under that path. Other source chunks are unchanged. Calling it on
a non-git source re-indexes and makes `search` return fresh results.
