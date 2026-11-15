---
status: accepted
date: 2026-11-15
amends: ["0017", "0018", "0020"]
---

# ADR-0022: Surface ADR amendments — as-built facts and a measurable value gate

- **Deciders:** basemind maintainers
- **Related:** amends ADR-0017, ADR-0018, ADR-0020; ADR-0014, ADR-0021

## Context

ADR-0017 (surface reduction), ADR-0018 (verb tools) and ADR-0020 (their contract) were accepted on
14 and 15 November 2026. An audit compared them with the code on `main` and found statements that
are no longer true, rules that live only in a Consequences section, and one rule that cannot be
tested:

- ADR-0017 says plain memory (`put`, `get`, `list`, `search`, `delete`) is untouched. The MCP
  surface has two memory tools, `memory_save` and `memory_search`, and `delete` exists only on the
  CLI (`tests/cli_parity/capabilities.rs` records it as CLI-only).
- ADR-0017 names the `rmux-*` crates as "one of two blockers" to publishing on crates.io and never
  names the other.
- ADR-0018 lists 18 tools. Fifteen exist (`VERB_TOOLS` in `src/mcp/types_tools.rs`).
- ADR-0018 says every list takes `limit` and `cursor` and every response has a `max_tokens` budget.
  Four tools do not follow that.
- ADR-0018 says the whole `tools/list` stays under about 5k tokens. The test bounds something
  narrower.
- ADR-0018 justifies `_meta["anthropic/alwaysLoad"]` as if every host honoured it.
- ADR-0020 decides to keep the `workspace` domain tool. Commit `f976d669` removed it.
- ADR-0018's value gate reads "a tool ships only if the eval harness shows it beats the `rg`/`git`
  baseline on correctness or tokens". It names no threshold, no cadence and no consequence, and no
  eval report is committed that shows it was applied to any of the fifteen tools.

## Decision

### Corrections of fact

1. **Memory.** The agent-facing memory surface is `memory_save` and `memory_search`
   (`memory_search` also serves the lookup-by-key and list-by-prefix shapes). Deleting shared memory
   is a CLI command only, `basemind memory delete`.
2. **Publishing blockers.** The second blocker is the `xberg` dependency, which `Cargo.toml` pins by
   git revision. It is the only git dependency left in the manifest.
3. **Scope.** Git history and shared memory stay in the product. This is the owner's decision about
   scope. It is not a measured one: ADR-0018's transcripts show zero `git` calls, and no measurement
   of memory use was taken.
4. **Tool table.** The surface is the fifteen verb tools below plus the three agent tools, which
   ADR-0021 owns. `agent_launch` and `agent_stop` remain reserved names with no design.

   | Area | Tools |
   |---|---|
   | Code | `code_outline`, `code_symbols`, `code_read`, `code_grep`, `code_search`, `code_refs`, `find_files` |
   | Documents | `docs_search` |
   | Git | `git_log`, `git_blame`, `git_diff` |
   | Memory | `memory_save`, `memory_search` |
   | Index | `index_status`, `index_rescan` |

5. **`workspace`.** ADR-0020's decisions 3 and 4, as far as they concern the `workspace` tool, are
   superseded by its removal. The read-only listings remain as CLI commands.

### Contract

1. **Convention exceptions.** Every other tool that returns a list takes `limit`, `cursor` and
   `max_tokens`:
   - `git_diff` takes none of the three. A diff is returned whole.
   - `code_read` takes no `max_tokens`. It returns one span or one symbol body.
   - `memory_search` takes `limit` and `cursor` but no `max_tokens`.
   - `index_rescan.paths` is a list of strings, not of validated repository-relative paths.
2. **Byte ceilings.** The ceiling bounds the always-loaded verb tools, not the whole `tools/list`:
   20,000 bytes for the fifteen (`VERB_TOOLS_BYTE_CEILING` in `tests/mcp_schema_wire.rs`). The agent
   tools have a separate ceiling of 5,000 bytes (ADR-0021).
3. **Always-load is one vendor's key.** `_meta["anthropic/alwaysLoad"]` is read by Anthropic's
   hosts. Other hosts ignore it and load every advertised tool, so on them the agent tools are not
   deferred and a session pays for all eighteen.
4. **`code_refs` scan cap.** Promoted from ADR-0020's Consequences to a contract rule: an exact scan
   examines at most `SCAN_KEYS_EXAMINED_MAX` index keys per page (200,000, in
   `src/mcp/helpers_calls_scan.rs`). A page can therefore be empty and still carry a `next_cursor`,
   and a client must follow the cursor until it is absent before concluding there are no results.

### Value gate

This replaces the value gate of ADR-0018.

- **Measure.** `basemind admin eval` (`src/eval/`, generators and method in
  `benchmarks/eval/README.md`) runs each task through the tool and through a shell baseline (a
  targeted `git grep` plus up to three whole-file reads) and reports the median ratio of baseline
  tokens to tool tokens. A task counts only when the tool's recall reaches the harness's
  `--min-recall` (0.8).
- **Threshold.** A tool passes when the median ratio on its task kind is at least 1.5 and the
  harness's regression check against the previous release's committed report (`--baseline`) is
  clean. The harness does not score the shell baseline's correctness; the recall floor and the
  regression check stand in for "no less correct than the shell".
- **Cadence.** The gate is run once per release, and the report is committed under
  `benchmarks/eval/`.
- **Consequence.** A tool that fails in two consecutive releases is removed or reworked before the
  third.
- **Exempt,** because no shell command does the same job: `memory_save`, `memory_search`,
  `index_status`, `index_rescan`, `agent_post`, `agent_inbox`, `agent_threads`.

| Tool | Eval task kind | Ratio recorded today |
|---|---|---|
| `code_symbols` | `symbols` | 25x |
| `docs_search` | `docs` | 2.5x |
| `code_outline` | `outline` | 1.2x |
| `code_refs` | `references`, `callers`, `dependents` | 1x |
| `code_grep` | `grep` | 1x |
| `find_files` | `find` | 1x |
| `git_log` | `git_search` | 1x |
| `code_read`, `code_search`, `git_blame`, `git_diff` | none exists | not measured |

The ratios are the per-tool constants in `src/mcp/savings.rs`, which the README says were calibrated
from the eval. No eval report is committed in `benchmarks/eval/` today, so the gate has never run
and its first run is outstanding work.

**Adoption** is the measure of whether the surface succeeds: per session, the number of basemind
`code_*`, `find_files` and `git_*` calls divided by the number of Bash `rg`/`grep` calls. ADR-0018's
baseline is zero against about 60,000.

## Consequences

- On today's numbers five of the seven gated tools that have a task kind would fail: `code_outline`,
  `code_refs`, `code_grep`, `find_files` and `git_log`. If the first two gated releases confirm
  that, the rule requires removing or reworking them, which is most of the code-navigation surface.
  That outcome is intended to be possible.
- Four tools cannot be gated until task kinds and generators are written for them. Until then they
  are neither passing nor exempt, and the release report must list them as unmeasured.
- The ratio depends on how heavy the baseline is. The baseline definition is part of the gate and
  changes to it must be recorded with the report.
- basemind's telemetry records its own tool calls and cannot see a host's Bash calls. The adoption
  ratio therefore needs the host's transcripts, as ADR-0018's measurement did, or a counter added to
  the plugin's `pre-tool-guard` hook, which already matches Bash `rg`/`grep` commands.
- The four convention exceptions stay as they are. Removing one is a breaking schema change and
  needs no ADR; adding another does.
- Hosts that ignore the always-load key get no benefit from deferral, and nothing in basemind can
  change that.

## Alternatives considered

- **Edit ADR-0017, ADR-0018 and ADR-0020 in place** — rejected: accepted records are immutable, and
  three of the corrections change a rule, not a typo.
- **Supersede ADR-0018 with a consolidated contract** — rejected: its core decision (one tool per
  job, exact schemas) stands; replacing the record would bury that under corrections.
- **Gate on correctness or tokens, as ADR-0018 wrote** — rejected: with "or" every tool passes on
  the dimension it happens to win, and a rule without a threshold or a consequence was not applied.
- **A threshold of 1.0 (no worse than the shell)** — rejected: a tool that only matches `rg` costs
  schema bytes in every session and gives the agent no reason to prefer it.
- **Gate on adoption instead of token ratio** — rejected as the gate, kept as the success measure:
  adoption depends on hooks and instructions as much as on the tool, and cannot be measured per
  tool before release.
