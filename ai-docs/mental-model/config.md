# Config — Mental Model

## Entry Points

- `src/config.rs` — all config logic: path resolution, file loading, merge
- `src/main.rs` lines 49-52 — shows how config is wired at startup

## Module Contracts

- `config_paths` returns `None` for `global` only if `dirs::config_dir()` returns `None` (rare; happens in stripped environments with no home directory). It returns `None` for both `project` and `local` together when no `.git` directory ancestor is found — they are always either both `Some` or both `None`, never one without the other.
- `load_config` silently skips missing files; only malformed TOML causes an error. The error message always includes the offending file path.
- `Config::merge` treats `None` in the overlay as "absent" — it does **not** overwrite an existing value. A `Some` in the overlay always wins.

## Coupling

- Config is resolved from `std::env::current_dir()` at CLI startup, not from the `path` argument passed to `index`. Running `cargo debrief index /other/project` applies config from wherever the shell is, not from `/other/project`. This is intentional but easy to miss.

## Extension Points & Change Recipes

**Adding a new config field:**

1. Add the field as `Option<T>` to `Config` in `src/config.rs`.
2. Add a branch to `Config::merge` — if you forget this step, the new field silently ignores all config layers and always returns its default value regardless of what is written in any config file.
3. Add a `set_<field>` subcommand or update `SetEmbeddingModel` if the field needs CLI write support.

## Common Mistakes

- **Missing merge branch for a new field.** If `Config::merge` is not updated when a new field is added, higher-priority layers (project, local) silently have no effect on that field. No error is raised.
- **Worktree/submodule silently uses global-only config.** `find_git_root` checks for `.git` as a *directory*. Git worktrees and submodules write `.git` as a *file*. In those cases `project` and `local` paths are `None`, and only global config applies — no error, no warning.
