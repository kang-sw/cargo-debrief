<!-- AI-maintained project state — read before work, update after -->
<!-- - `ai-docs/_index.md` — architecture, conventions, build/test, session notes -->

# CLAUDE.md — cargo-debrief

## Project Summary

**cargo-debrief** — A Rust CLI tool that provides RAG
(Retrieval-Augmented Generation) over codebases. Uses tree-sitter for
AST-aware chunking and vector search with metadata boosting to feed
LLMs only the relevant code fragments, reducing context window consumption.
CLI-first with a lazy-spawned background daemon for index serving.

## Tech Stack

Rust (2024 edition). Key libs: tree-sitter, ort (ONNX Runtime), clap, serde,
tokio.

## Workspace

```
src/           — Main source code
ai-docs/       — Project knowledge and cross-session context
```

## Architecture Rules

1. **Single binary.** The tool ships as one binary — no external DB, no
   sidecar processes. Vector index and BM25 index are serialized to disk.
2. **CLI-first.** Primary interface is the CLI. A background daemon is
   lazy-spawned on first use and auto-expires on idle. MCP layered later.
3. **Git-aware indexing.** Incremental re-indexing tracks `git diff` between
   last-indexed commit and HEAD. Never re-index unchanged files.

---

## Project Knowledge

Project state and cross-session context live in `ai-docs/`.
Read `_index.md` at session start.
Before creating or editing tickets, load `/write-ticket` for conventions.
Reference tickets by **stem only** (e.g., `260403-research-rag-architecture`),
never by full path — stems stay stable across status moves.
When starting work on a ticket, move it to `wip/` immediately.

**Language:** All AI-authored artifacts — documents, plans, commit messages, ticket entries,
`### Result` entries, and inline code comments — must be in English regardless of
conversation language. Human-facing UI strings are exempt.

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
```

### Session Start

- Read `ai-docs/_index.md` for project context.
- Run `git log -10` for recent changes. (without `--oneline`!)

### Response Discipline

- **Evidence before claims.** Run verification commands and read output before
  stating success. Never use "should pass", "probably works", or "looks correct."
- **No performative agreement.** Never respond with "Great point!", "You're
  absolutely right!", or similar. Restate the technical requirement, verify
  against the codebase, then act (or push back with reasoning).
- **Actions over words.** "Fixed. [what changed]" or just show the diff.
  Skip gratitude expressions and filler.

### Context Window Discipline

- Keep context small. Load only the module docs relevant to the current task.
- Source code is the ground truth; docs supplement it.
- When a module doc drifts from source, update the doc (or flag it).
