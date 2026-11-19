---
status: proposed
date: 2026-11-18
---

# ADR-0049: Reading outside the repository — session transcripts and dependency documentation

- **Deciders:** the repository owner
- **Related:** ADR-0029 and ADR-0037 (retrieval store), ADR-0039 (CLI commands forwarded to the
  daemon), ADR-0044 decision 4 (a path a project can steer is not trusted), ADR-0047 (analysis
  commands)

## Context

Two issues for 0.34.0 need files outside the repository being indexed:

- **#128** measures how agents work in this repository from their session transcripts: basemind
  calls against shell searches, whole-file reads, edits without a prior read. The transcripts live
  in the user's home: Claude Code's under `~/.claude/projects/<slug>/*.jsonl`, Codex's under
  `~/.codex/sessions/**/*.jsonl`. They hold prompts, file contents and command output.
- **#132** indexes the documentation of the repository's dependencies (README, CHANGELOG,
  `docs/**`) so `docs_search` can answer questions about a library at the version in use. The
  sources are in `node_modules/`, a virtual environment's `site-packages/`,
  `~/.cargo/registry/src/` and `$GOMODCACHE`.

basemind already contains reads that reach outside the tree:

- `basemind.toml` is written by whoever wrote the repository. `src/config/trust.rs` strips from its
  file layer every setting that reaches outside the repository unless the operator grants it in the
  environment (`BASEMIND_ALLOW_FOLLOW_SYMLINKS`), and `scan.extra_roots` needs
  `BASEMIND_ALLOW_EXTRA_ROOTS` (`src/scanner_candidates.rs`). `tests/extra_roots_containment_smoke.rs`
  proves a cloned repository cannot point the scan at `~/.ssh`.
- The retrieval store (`src/lance/docs_schema.rs`) keys documents by `doc_key` and filters
  `docs_search` by a `path` glob (ADR-0029 rule 5). Its table layout is versioned by
  `VECTOR_STORE_VER` (ADR-0037 rule 2).

## Decision

### Rules both features follow

1. **Fixed roots.** Each feature reads only from a fixed list of roots that basemind derives:
   the two transcript directories, and the package locations of the ecosystems below. No setting
   in the repository's `basemind.toml` adds, moves or widens a root.
2. **Operator grant for reads outside the tree.** A read outside the repository tree happens only
   on the operator's request: an explicit command the user runs (#128), or a grant in the process
   environment (#132). A repository's own configuration never grants it; `src/config/trust.rs`
   enforces this on the file layer, as it does for `follow_symlinks`.
3. **Containment.** Paths are canonicalized; a walk follows no symlink out of its root, reuses the
   `extra_roots` containment and keeps the per-root refusal of a filesystem root.
4. **Read only.** Nothing is written into a root outside the repository.
5. **Tested before the code lands.** Each feature ships with an escape test (a symlink out of a
   root, a repository configuration naming a root) in the same commit as its reader.

### #128: session transcripts

- `basemind sessions [--since D] [--json]` is a CLI command that runs in the CLI process: it reads
  the user's home, so it is not forwarded to the daemon (ADR-0039 rule 4) and has no `CLI_ONLY`
  entry. Its parity row is `cli_only(...)`. No MCP tool or argument exposes it.
- Hosts: Claude Code and Codex, behind a `TranscriptParser` trait in a new `src/sessions/`
  (`mod.rs`, `claude.rs`, `codex.rs`). Others are follow-up work.
- A transcript is read only when its recorded `cwd` is this repository's root or one of its
  worktrees, or a directory below one of them (resolved first when it holds `..`). Claude Code files
  such a session under a project directory whose slug extends the checkout's, so those are walked
  too; a sibling the lossy slug also matches is dropped by its `cwd`.
- Subagent transcripts are read under the same rules: Claude Code's
  `<slug>/<session>/subagents/[workflows/<run>/]agent-*.jsonl`, walked without following a
  symlink at any level. Each is reported on its own and attributed to its parent session.
- **Counts only.** The output is per-session and aggregate counts plus repository-relative paths.
  It never echoes message text, tool input or output, or a command line, and writes nothing but
  stdout: no cache, no telemetry, no file.
- **No hook output.** No session-start or other hook prints these numbers.
- A transcript whose format version the parser does not know is reported as `unsupported`, never
  skipped silently.

### #132: dependency documentation

- **Opt-in, off by default:** `[documents.dependencies]` in `src/config/documents.rs` with
  `enabled = false`, `direct_only = true`, `max_packages = 200` and `max_bytes_per_package = 2 MiB`;
  the schema snapshot is regenerated.
- **Package roots.** The direct dependencies come from the manifest and lockfile pairs:
  `Cargo.toml` with `Cargo.lock`, `package.json` with `package-lock.json` or `pnpm-lock.yaml`,
  `pyproject.toml` with `uv.lock` or `poetry.lock`, and `go.mod` with `go.sum`. Each resolves to
  one installed source: `node_modules/<pkg>`, `site-packages/<pkg>*` of `.venv` or
  `$VIRTUAL_ENV`, `~/.cargo/registry/src/*/<pkg>-<ver>`, `$GOMODCACHE/<mod>@<ver>`. Only README,
  CHANGELOG and `docs/**` are indexed from it.
- **Trust.** The repository's `basemind.toml` may enable the feature for package roots inside the
  repository tree. A root outside it (the cargo registry, the Go module cache, a virtual
  environment outside the tree) is read only under the operator grant
  `BASEMIND_ALLOW_DEPENDENCY_ROOTS`, read in `src/config/trust.rs` like the existing grants;
  without it those packages are listed as unavailable.
- **Virtual paths.** Each document is stored under `deps/<ecosystem>/<name>@<version>/<file>`, so
  `docs_search {path:"deps/npm/react@*/**"}` filters by package with no new MCP field.
- **Columns.** `docs` gains nullable `package` and `package_version` columns so a hit cites the
  version; `VECTOR_STORE_VER` is bumped (ADR-0037 rule 2).
- A dependency that is not installed, or not granted, is listed in `index_status` under
  `deps_unavailable`. A changed lockfile hash re-indexes the dependency source.

## Consequences

- Transcripts are processed where they already are, by the user's own command, and only counts
  leave the parser. The command cannot run inside an agent's MCP session or through the daemon.
- Each transcript format change in Claude Code or Codex can break a parser; `unsupported` makes
  that visible, and synthetic fixtures under `tests/fixtures/sessions/` (never real transcripts)
  pin the formats.
- A cloned repository that enables dependency documents reaches only its own tree; reading the
  shared caches in the home needs the operator, as `extra_roots` does.
- The grant is an environment variable like the existing two, so it shares their exposure: an MCP
  host that sets its servers' environment from a project's settings lets that project set it.
  ADR-0044 decision 4 avoided the environment for the agent policy file for this reason; the
  dependency grant only widens which documentation is read, never what is executed.
- `$VIRTUAL_ENV` and `$GOMODCACHE` are read from the scanning process's environment, so a daemon
  sees the environment it was started with, not the shell's.
- Dependency documents share the group store and its liveness, collection and model identity
  (ADR-0029, ADR-0037). An indexed dependency costs embedding time at its first scan, bounded by
  the two caps.
- Tests: fake `node_modules` and cargo registry fixtures with home roots overridden, a
  symlink-escape refusal, ingestion as in `tests/doc_tiers.rs`, the caps, and an assertion that
  `basemind sessions` output contains no message text.

## Alternatives considered

- **Run `sessions` through the daemon like the other analyses** — rejected: the daemon serves many
  workspaces and agents; reading a user's transcripts there would put them in reach of a relayed
  call.
- **Expose session metrics over MCP** — rejected: an agent would read metrics of other agents'
  sessions, and the numbers serve the user, not the agent.
- **Store per-session metrics for trends** — rejected: anything derived from transcripts stays out
  of every store; a user who wants history keeps the `--json` output.
- **A new `docs_search` field for the package** — rejected: the virtual path does the same with no
  schema bytes (ADR-0047 decision 2).
- **Let the repository's `basemind.toml` enable the home roots** — rejected: a clone would then
  choose what is read from the operator's home, which `tests/extra_roots_containment_smoke.rs`
  exists to prevent.
- **Index dependency sources, not only their documentation** — rejected: it multiplies the index by
  the dependency tree, and code navigation into dependencies is not what #132 asks for.
