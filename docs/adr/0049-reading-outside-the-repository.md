---
status: accepted
date: 2026-11-20
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
3. **Containment.** Paths are canonicalized and must stay under their root; a walk follows no
   symlink out of it. The transcript reader also refuses a filesystem root as a root.
4. **Read only.** Nothing is written into a root outside the repository.
5. **Tested before the code lands.** Each feature ships with an escape test (a symlink out of a
   root, a repository configuration naming a root) in the same commit as its reader.

### #128: session transcripts

- `basemind sessions [--since D] [--json]` is a CLI command that runs in the CLI process: it reads
  the user's home, so it is not forwarded to the daemon (ADR-0039 rule 4) and has no `CLI_ONLY`
  entry. Its parity row is `cli_only(...)`. No MCP tool or argument exposes it.
- Hosts: Claude Code and Codex, behind a `TranscriptParser` trait in a new `src/sessions/`
  (`mod.rs`, `claude.rs`, `codex.rs`). Others are follow-up work.
- A transcript is read only when its recorded `cwd` is this repository's root, one of its live
  worktrees or a removed worktree the next rule admits, or a directory below one of them (resolved
  first when it holds `..`). Claude Code files such a session under a project directory whose slug
  extends the checkout's, so those are walked too; a sibling the lossy slug also matches is
  dropped by its `cwd`. Claude Code keeps a slug of up to 200 characters whole and cuts a longer
  one to 200, followed by `-` and a hash that differs between its runtimes (`sanitizePath`), so
  such a project directory is matched by those 200 characters and `-`.
- A session of a removed worktree is this repository's. A worktree is removed when its gitdir is
  gone and `git worktree list` no longer lists it. Git keeps no record of where a removed worktree
  was, so it is recognised by where worktrees are made: a directory, no checkout itself, that
  holds one of the repository's live linked worktrees (one whose checkout exists) and no other
  repository (every child that is a git checkout is one of its checkouts or has a `.git` file
  naming a gitdir under its `worktrees/`). A child of that directory which is no live checkout is
  a removed worktree when it is gone, or when its `.git` file names such a gitdir that is gone;
  one that exists without that file is not. The probe stats the child and reads at most 4 KiB of
  its `.git` file, never through a symlink. The Claude Code project directories whose slug extends
  that directory's are walked for them. A directory that also holds another repository (`~/code`
  beside other clones) names no removed worktree, and the sessions of a worktree removed there
  count under `other_directories`.
- A relative path a host tool, shell command or patch names is taken from the session's working
  directory, then made repository-relative; a basemind `path` argument is already
  repository-relative. Paths are compared as text in either platform's shape (`/`, drive and UNC
  roots, either separator), with `..` resolved lexically.
- Subagent transcripts are read under the same rules: Claude Code's `agent-*.jsonl` up to two
  directories below `<slug>/<session>/subagents/` (`workflows/<run>/`), walked without following a
  symlink at any level, or beside the sessions with its records marked as a sidechain of the
  session they name; and Codex's subagent rollouts, rollouts of their own whose `session_meta`
  marks them as a subagent's and names their parent. A subagent is attributed through its direct
  parent, up through other subagents, to the session it descends from, and reported on its own.
- Copied history is counted once. A Codex subagent skips the records before its
  `subagent_history_start_ordinal`, by line position where a Legacy-mode rollout writes no
  ordinals. A fork, a revert, a resumed Claude Code session and a subagent with forked context
  copy earlier records with their ids, so tool calls, results and model responses are counted once
  per host by id, reading transcripts oldest first; the rollouts of one reverted Codex thread are
  one session. Codex output tokens are the rise of the cumulative total, so a repeated
  `token_count` adds nothing. A Codex `token_count` has no id: one copied before a fork point
  repeats its `total_token_usage`, which counts it once, so a reply after the fork point that calls
  no tool still counts its own tokens.
- Codex rollouts it has compressed (`*.jsonl.zst`) are decompressed and read. A line over 32 MiB,
  or one that is not JSON of a known shape, is skipped and counted.
- `src/sessions/` also holds `code_mode.rs`: Codex's code mode records one `exec` call whose input
  is a JavaScript script, and its nested tool calls are read from that script by a lexer that
  evaluates nothing.
- **Counts only.** The output is per-session and aggregate counts plus repository-relative paths.
  It never echoes message text, tool input or output, or a command line, and writes nothing but
  stdout: no cache, no telemetry, no file. A string taken from a transcript (a version, a thread
  id) is printed only as a short token of `[0-9A-Za-z._+-]`, else as `unparseable`.
- **No hook output.** No session-start or other hook prints these numbers.
- A transcript whose format version the parser does not know is reported as `unsupported`, never
  skipped silently.

### #132: dependency documentation

- **Opt-in, off by default:** `[documents.dependencies]` in `src/config/documents.rs` with
  `enabled = false`, `direct_only = true`, `max_packages = 200` and `max_bytes_per_package = 2 MiB`;
  the schema snapshot is regenerated.
- **Package roots.** The direct dependencies come from the manifest and lockfile pairs:
  `Cargo.toml` with `Cargo.lock`, `package.json` with `package-lock.json`, `pyproject.toml` with
  `uv.lock` or `poetry.lock`, and `go.mod` alone (`pnpm-lock.yaml` and `go.sum` only trigger a
  re-resolve when they change). Each resolves to one installed source: `node_modules/<pkg>`; the
  `<pkg>-<ver>.dist-info` in `site-packages` of `.venv` or `$VIRTUAL_ENV`, its README read from
  `METADATA`; `vendor/`, else `$CARGO_HOME/registry/src/*/<pkg>-<ver>` (`~/.cargo` by default);
  `vendor/`, else `$GOMODCACHE/<mod>@<ver>` (else `$GOPATH/pkg/mod`, else `~/go/pkg/mod`). Only
  README, CHANGELOG and `docs/**` are indexed from it.
- **Trust.** The repository's `basemind.toml` may enable the feature for package roots inside the
  repository tree. A root outside it (the cargo registry, the Go module cache, a virtual
  environment outside the tree) is read only under the operator grant
  `BASEMIND_ALLOW_DEPENDENCY_ROOTS`, defined in `src/config/trust.rs` and read by `cli::env_config`
  like the existing grants (ADR-0050);
  without it those packages are listed as unavailable.
- **Virtual paths.** Each document is stored under `deps/<ecosystem>/<name>@<version>/<file>`, so
  `docs_search {path:"deps/npm/react@*/**"}` filters by package with no new MCP field.
- **Columns.** `docs` gains nullable `package` and `package_version` columns so a hit cites the
  version; `VECTOR_STORE_VER` is bumped (ADR-0037 rule 2).
- A dependency that is not installed, or not granted, is listed in `index_status` under
  `deps_unavailable`. A changed manifest or lockfile re-resolves the dependencies.

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
