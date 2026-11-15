---
status: accepted
date: 2026-11-14
supersedes: "0011"
amends: ["0014"]
---

# ADR-0018: MCP surface — verb tools with exact schemas

- **Status:** Accepted; amended by ADR-0020 and ADR-0022 (2026-11-15)
- **Date:** 2026-11-14
- **Deciders:** basemind maintainers
- **Related:** supersedes ADR-0011; amends ADR-0014; ADR-0017

## Context

ADR-0011 consolidated the tools into mode-dispatched domains. Measurement shows the result is not
used and not precise:

- **Adoption.** Across about 1,450 Claude Code transcripts there were about 60k Bash `rg`/`grep`
  calls and zero basemind `code` or `git` calls. All 650 basemind MCP calls were agent messaging.
- **Schema weight.** `tools/list` costs about 13.6k tokens. Between 72% and 90% of each domain
  schema is irrelevant to any single call, and only `mode` is schema-required.
- **Silent failures.** Unknown parameters are dropped: a `grep` call with an unsupported `glob` key
  ran unscoped and succeeded.
- **Inconsistent conventions.** `start_row` is 0-based in `outline`, `symbols`, `callers` and the
  graph modes, but 1-based in `expand`.
- **Wrong execution model.** Fast calls (6 to 27 ms) were returned as background Tasks.
- **Value.** In a head-to-head on this repository basemind beat the shell only on file outlines.

## Decision

Replace the mode-dispatched domain tools with one tool per job:

| Area | Tools |
|---|---|
| Code | `code_outline`, `code_symbols`, `code_read`, `code_grep`, `code_search`, `code_refs`, `find_files` |
| Documents | `docs_search` |
| Git | `git_log`, `git_blame`, `git_diff` |
| Memory | `memory_save`, `memory_search` |
| Agents | `agent_post`, `agent_inbox`, `agent_threads` |
| Index | `index_status`, `index_rescan` |

`agent_launch` and `agent_stop` are reserved for a later ADR.

Every tool has:

- its own params struct, with required fields required in the schema;
- unknown fields rejected with a did-you-mean error;
- one example call in the description;
- an output schema;
- accurate read-only and destructive annotations;
- `_meta["anthropic/alwaysLoad"]: true`, so a host that defers MCP tools by default loads the
  schema at session start instead of needing a tool-search round trip.

Conventions for all tools:

- `path` is repo-relative; `line` and `end_line` are 1-based everywhere.
- Every list takes `limit` and `cursor`, and every response has a default `max_tokens` budget.
- Failures are returned as `isError` tool results that name the fix.
- A `notice` reports an empty index, a scan in progress or a degraded lane.
- Results are returned inline; a server-side Task is used only past a time threshold.
- The whole `tools/list` stays under about 5k tokens, enforced by a test.

Value gate: a tool ships only if the eval harness (`src/eval/`, run with `basemind admin eval`)
shows it beats the `rg`/`git` baseline on correctness or tokens.

## Consequences

- Breaking change, taken in one release together with the plugin's skills, commands, hooks and
  rules. The plugin downloads the version-matched binary, so hosts never see a mismatched pair.
- Supersedes ADR-0011. `src/mcp/mode.rs` and the per-mode allow-lists are deleted.
- Amends ADR-0014: CLI parity becomes "every MCP tool has a CLI command" (injective), no longer a
  bijection, and the CLI may keep administrative commands that have no MCP tool. The coverage,
  parameter and behaviour layers keep their structure and are re-keyed on tool names.
- Telemetry keys are renamed.
- `tests/mcp_smoke.rs`, `tests/mcp_schema_wire.rs`, `tests/cli_parity/` and the `tests/harden.rs`
  sweeps are rewritten against the new tools.
- Annotations are exact per tool, so a host that gates on read-only can allow `index_status` and
  `git_log` without allowing `index_rescan`.

## Alternatives considered

- **Keep the nine domain tools and fix the defects only** — rejected: it addresses unknown-key
  handling and line numbering but not schema weight or the per-call irrelevance of `mode`.
- **A two-tier surface: verb tools plus the domain tools for the long tail** — rejected once
  breaking changes were approved: two surfaces to document, test and keep consistent.
