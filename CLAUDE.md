# CLAUDE.md — cargo-debrief

## Project Memory

Read in this order at every session start, before any other action:

1. **Preamble** — read `ai-docs/_index.md`. Project-level truth that no
   session should re-derive. Prune aggressively: if derivable from code
   or commit history, delete.
2. **Local** — read `ai-docs/_index.local.md` if it exists. .gitignored.
   Machine-bound context (paths, env vars, build config) and personal
   session notes.
3. **Project arc** — run `git log --oneline --graph -50`. Trajectory and
   topic clusters at a glance.
4. **Recent history** — run `git log -10`. Decision rationale via AI Context
   sections. Fades as history grows.

## Response Discipline

- **Evidence before claims.** Run verification commands and read output before
  stating success. Never use "should pass", "probably works", or "looks correct."
- **No performative agreement.** Never respond with "Great point!", "You're
  absolutely right!", or similar. Restate the technical requirement, verify
  against the codebase, then act (or push back with reasoning).
- **Actions over words.** "Fixed. [what changed]" or just show the diff.
  Skip gratitude expressions and filler.

## Code Standards

1. **Simplicity.** Write the simplest code that works. Implement fully when the spec is
   clear — judge scope by AI effort, not human-hours.
2. **Surgical changes.** Change only what the task requires. Follow existing style. Every
   changed line must trace to the request.
3. **Responsibility check.** As you implement, ask whether each change
   keeps the module's role clean. Split when responsibility drifts.
4. **Descriptive naming.** Prefer self-documenting identifiers over
   abbreviated ones — names serve as implicit search metadata for RAG retrieval.

## Workflow

### Approval Protocol

- **Auto-proceed**: Bug fixes, pattern-following additions, test code, boilerplate,
  refactoring within a single module.
- **Ask first**: New component/protocol additions, architectural changes,
  cross-module interface changes, anything that changes observable behavior.
- **Always ask**: Deleting existing functionality, changing protocol/API semantics,
  modifying persistence schema.

### Commit Rules

Auto-create git commits, each covering one logical unit of change.
Include an **AI context** section in every commit message recording design decisions,
alternatives considered, and trade-offs — focus on _why_ this approach was chosen.

```
<type>(<scope>): <summary>

<what changed — brief>

## AI Context
- <decision rationale, rejected alternatives, user directives, etc.>

## Ticket Updates                          # optional — only when ticket-driven
- <ticket-stem>[: <optional-label>]
  > Forward: <what future phases must know>
```

### Context Window Discipline

- Source code is ground truth; load only docs relevant to the current task. Update drifted docs on contact.

## Architecture Rules

1. **Single binary.** The tool ships as one binary — no external DB, no
   sidecar processes. Vector index and BM25 index are serialized to disk.
2. **CLI-first.** Primary interface is the CLI. A background daemon is
   lazy-spawned on first use and auto-expires on idle. MCP layered later.
3. **Git-aware indexing.** Incremental re-indexing tracks `git diff` between
   last-indexed commit and HEAD. Never re-index unchanged files.

## Project Knowledge

- Project state and cross-session context live in `ai-docs/`.
- Before creating or editing tickets, load `/write-ticket` for conventions.
- Reference tickets by **stem only** (e.g., `260403-research-rag-architecture`),
  never by full path — stems stay stable across status moves.
- When starting work on a ticket, move it to `wip/` immediately.
- To check ticket completion or prior phase results, use
  `git log --grep=<ticket-stem>` and look for `## Ticket Updates`
  sections in matching commits.
- **Language:** All AI-authored artifacts — documents, plans, commit messages, ticket entries,
  and inline code comments — must be in English regardless of conversation language.
  Human-facing UI strings are exempt.

<!-- Inclusion test: if breaking this rule makes a skill produce
     wrong results, it belongs here. Everything else goes in
     _index.md (context) or skills (process). -->

<!-- Template Version: v0017 -->
