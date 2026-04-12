---
domain: git
description: "Git file tracking via shell-out: head commit, file diffs, dirty-file detection with sha256 hashes"
sources:
  - src/
related:
  store: "GitRepoState and DirtySnapshot are the persistence types for per-root state"
  service: "apply_incremental_updates is the sole production caller of changed_files and dirty_files"
---

# Git — Mental Model

## Entry Points

- `src/git.rs` — `head_commit`, `changed_files`, `FileChanges`, `find_git_root`, `current_head`, `dirty_files`

## Module Contracts

- `changed_files(repo_root, None)` returns all tracked files via `git ls-files`, all in `FileChanges::added`. It does **not** diff; it triggers a full reindex.
- `changed_files(repo_root, Some(hash))` diffs `hash..HEAD` via `git diff --name-status`. Renames are split into a `deleted` entry for the old path and an `added` entry for the new path. Copies produce only an `added` entry.
- All paths returned by `changed_files` and `dirty_files` are **relative to the repo root** (as git reports them). Callers must join with the repo root to get usable file paths.
- All functions shell out via `std::process::Command`. `git` must be on `PATH`; no libgit2 dependency.
- `find_git_root(path)` is a **pure filesystem walk** — no subprocess. It walks parent directories looking for `.git` (file or directory), handling both regular `.git/` dirs and `.git` files (worktrees, submodules). Returns `None` when the path is outside any git repo.
- `current_head(root)` and `head_commit(repo_root)` are functionally identical (`rev-parse HEAD`). `current_head` is the preferred call in the incremental pipeline; `head_commit` predates it and is retained for backward compat.
- `dirty_files(root)` parses `git status --porcelain` and computes a sha256 content hash per dirty file. Deleted files that no longer exist on disk get an all-zeros hash `[0u8; 32]`. The map covers tracked-modified, staged, and untracked files; `.gitignore` is respected.

## Coupling

- All git functions are stateless free functions — no shared state, no caching. The indexing pipeline stores the per-git-root state in `store::IndexData::git_states` (`GitRepoState { last_indexed_commit, dirty_snapshot }`).

## Common Mistakes

- **Using returned paths as absolute paths** — paths from `changed_files` and `dirty_files` are repo-relative. Joining with the repo root is required before any file I/O.
- **Passing `None` to `changed_files` when incremental diff is intended** — `None` means "no prior commit known, reindex everything." Pass the stored `last_indexed_commit` from `GitRepoState` for incremental updates.
- **git not on PATH** — shells out via `Command::new("git")`; fails with an I/O error if git is absent, not a descriptive domain error.
- **Confusing `find_git_root` input with `changed_files` root** — `find_git_root` accepts any path inside the repo (walks up); `changed_files` and `current_head` must receive the repo root (the directory containing `.git`).
