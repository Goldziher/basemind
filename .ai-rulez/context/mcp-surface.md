---
type: Concept
title: Mcp Surface
x-ai-rulez:
  kind: context
  id: mcp-surface
  metadata:
    priority: high
---

# MCP Surface

`basemind serve` exposes a stdio MCP server (`rmcp`). The live contract is `tests/mcp_smoke.rs` — read it before changing any tool's response shape.

The surface is **six domain tools**, each one `#[tool]` dispatching on a required,
non-defaulted `mode` — never a distinct tool per verb. The same six names are the CLI groups
(`basemind <domain> <mode>`), enforced as a strict bijection by `tests/cli_parity/`. Read
`src/mcp/mode.rs` for the authoritative wire spellings before writing about any mode; never infer
one from prose.

## Domains

| Tool / CLI group | `mode` values | Gate |
|---|---|---|
| `code` | outline, symbols, grep, files, find, definition, references, callers, implementations, dependents, expand, semantic, chunk | always |
| `git` | status, recent, touching, by_path, churn, diff, diff_outline, blame, blame_symbol, symbol_history, search, show | always |
| `memory` | put, get, list, search, delete, documents | always¹ |
| `admin` | status, repo, rescan, cache_stats, gc, cache_clear, telemetry | always |
| `agents` | register, list, thread_start, thread_list, join, leave, members, add_member, remove_member, archive, post, history, message, inbox, ack, wait | `comms` |
| `workspace` | workspaces, worktrees, branches, claim, release | `comms` |

¹ Advertised always, body-gated on the `memory` / `documents` features.

### `code` — code-map lookups

`outline` (a file's structure — read instead of the file), `symbols` (find a definition by name
substring), `grep` (pattern search, filtered by language/path), `files` (enumerate indexed
paths), `find` (fuzzy filename/path search), `definition` (resolve a reference position to its
scope-resolved definition), `references` (every call site of a name — no scope resolution;
`Foo::bar()` and `bar()` both match `name="bar"`), `callers` (callers of one specific definition,
disambiguated by path — resolves the definition first, then runs the same name-based scan as
`references`), `implementations` (types implementing/inheriting a trait or base class),
`dependents` (heuristic reverse-lookup via imports), `expand` (a symbol's raw source body),
`semantic` (search by meaning, needs `--features code-search`), `chunk` (fetch one code chunk's
body — the `semantic` fetch half).

### `git` — git history (requires `basemind serve` inside a git repo)

`status` (staged/unstaged summary), `recent` (recent commits with paths + summaries), `touching`
(commits that modified a path), `by_path` (path-filtered commit log), `churn` (churn-ranked
files), `diff` / `diff_outline` (file and outline diffs across revisions), `blame` /
`blame_symbol` (per-line and per-symbol blame), `symbol_history` (cross-commit history of a
symbol's structural hash), `search` (full-text search over commit messages and authors).

### `memory` — shared memory and documents

`put` / `get` / `list` / `search` / `delete` (a per-repo memory agents write to and search by
meaning), `documents` (semantic search over indexed PDFs / Office / HTML instead of opening
them).

### `admin` — server + cache administration

`status` (index health) / `repo` (repository identity and layout), `rescan` (re-index changed
files, or the whole workspace), `cache_stats` / `gc` / `cache_clear` (the machine-global cache),
`telemetry` (usage + token-savings summary).

### `agents` (`--features comms`)

`register` / `list` (identity card, roster), `thread_start` / `thread_list` (open / discover
threads addressed by subject, path-glob, and/or members), `join` / `leave` / `members` (explicit
membership — no auto-join), `add_member` / `remove_member` / `archive` (creator/admin-only
thread management), `post` (send a message), `history` / `inbox` (front-matter only — subject /
from / id), `message` (read one body by id — the only path to a body), `ack` (clear read
messages), `wait` (block until a peer posts or the timeout elapses).

### `workspace` (`--features comms`)

`workspaces` / `worktrees` / `branches` (the daemon's machine-wide registry), `claim` / `release`
(advisory worktree claims so sessions don't collide).

## Contract rules

- All paths are `RelPath` (byte-precise, repo-relative). Do not accept arbitrary `String` paths.
- Responses are `JsonSchema`-derived and stable; new fields are additive with `#[serde(default)]`.
- Lists are capped (`limit`, default 100, max 1000). Index scans use `scan_cap = limit * 8` to bound work on common names.
- Mode descriptions are the routing surface for agents; state semantics (substring vs prefix, scope-aware vs name-only) explicitly.
- Each domain owns a `types_<domain>.rs` / `tools_<domain>.rs` / `helpers_<domain>.rs` trio (`code`'s live in the base `types.rs`/`tools.rs`/`helpers_code.rs`; `agents` in the `*_comms.rs` files; `workspace` in the `*_registry.rs` files — filenames predate the CLI/tool rename). `tools.rs` and the `tools_<area>.rs` siblings contain `#[tool]` shims only, one per domain, dispatching on `mode`.
- No `output_schema` on any of the domain tools — every domain's modes return different response shapes, and SEP-2106 allows exactly one schema per tool. Per-mode shapes are documented in the description instead.
- Annotations coarsen to the union of a domain's modes, resolving toward the side effect (e.g. `admin` advertises `destructive_hint` because `cache_clear` is one of its modes). See ADR-0011.
