# Git — Mental Model

## Entry Points

- `src/git.rs` — `head_commit`, `changed_files`, `FileChanges`

## Module Contracts

- `changed_files(repo_root, None)` returns all tracked files via `git ls-files`, all in `FileChanges::added`. It does **not** diff; it triggers a full reindex.
- `changed_files(repo_root, Some(hash))` diffs `hash..HEAD` via `git diff --name-status`. Renames are split into a `deleted` entry for the old path and an `added` entry for the new path. Copies produce only an `added` entry.
- All returned paths are **relative to the repo root** (as git reports them). Callers must join with `repo_root` to get usable file paths.
- Both functions shell out via `std::process::Command`. `git` must be on `PATH`; no libgit2 dependency.

## Coupling

- `head_commit` and `changed_files` are stateless free functions — no shared state, no caching. The caller (future indexing pipeline) is responsible for storing and passing the `last_indexed_commit` hash (see `store::IndexData::last_indexed_commit`).

## Common Mistakes

- **Using returned paths as absolute paths** — they are repo-relative. Joining with `repo_root` is required before any file I/O.
- **Passing `None` when incremental diff is intended** — `None` means "no prior commit known, reindex everything." Passing the stored `last_indexed_commit` is required for incremental updates.
- **git not on PATH** — shells out via `Command::new("git")`; fails with an I/O error if git is absent, not a descriptive domain error.
