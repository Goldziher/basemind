---
status: accepted
date: 2026-11-15
amends: ["0017", "0018", "0020"]
---

# ADR-0022: Surface ADR amendments — as-built facts and a measurable value gate

- **Deciders:** basemind maintainers
- **Related:** amends ADR-0017, ADR-0018, ADR-0020; ADR-0014, ADR-0021, ADR-0026

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
2. **Publishing blockers.** The second blocker is the `xberg` dependency, the only git dependency
   left in the manifest. `Cargo.toml` gives it both a `rev` and `version = "1.4.0"`, and Cargo
   publishes against the registry version when both are present. Publishing is therefore blocked
   only if `xberg` 1.4.0 is not on crates.io, which was not checked.
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

1. **Paging exceptions.** Every other tool that returns a list takes `limit`, `cursor` and
   `max_tokens`:
   - `git_diff` takes none of the three. A diff is returned whole.
   - `code_read` takes no `max_tokens`. It returns one span or one symbol body.
   - `memory_search` takes `limit` and `cursor` but no `max_tokens`.
2. **Path exception.** Every other path field is a validated repository-relative path;
   `index_rescan.paths` is a list of plain strings.
3. **Byte ceilings.** The ceiling bounds the always-loaded verb tools, not the whole `tools/list`:
   20,000 bytes for the fifteen (`VERB_TOOLS_BYTE_CEILING` in `tests/mcp_schema_wire.rs`). The three
   agent tools share a separate ceiling of 5,000 bytes (ADR-0021).
4. **Always-load is one vendor's key.** `_meta["anthropic/alwaysLoad"]` is read by Anthropic's
   hosts. Other hosts ignore it and load every advertised tool, so on them the agent tools are not
   deferred and a session pays for all eighteen.
5. **`code_refs` scan cap.** Promoted from ADR-0020's Consequences to a contract rule: an exact scan
   examines at most `SCAN_KEYS_EXAMINED_MAX` index keys per page (200,000, in
   `src/mcp/helpers_calls_scan.rs`). A page can therefore be empty and still carry a `next_cursor`,
   and a client must follow the cursor until it is absent before concluding there are no results.

### Value gate

This replaces the value gate of ADR-0018.

- **Measure.** `basemind admin eval --tasks <file> --report <file> --min-recall 0.8` (`src/eval/`,
  method in `benchmarks/eval/README.md`) runs each task through the tool and through a shell
  baseline (a targeted `git grep` plus up to three whole-file reads). A task is **credited** when
  the tool's recall reaches 0.8; the report gives, per task kind, the credited and withheld counts
  and the median ratio of baseline tokens to tool tokens over the credited tasks.
- **Corpus.** Two repositories: basemind itself at the release tag, and the `tokio` checkout of the
  harden harness. `scripts/harden.sh` clones `tokio` at whatever its default branch holds, so a
  pinned commit must be added there first.
- **Tasks.** Generated by the committed generators with `--seed 1` and committed under
  `benchmarks/eval/`, one file per corpus, regenerated only when a corpus pin moves.
- **Tokenizer.** `o200k`, which the harness uses when built with the `tokenizer` feature, as release
  builds are. A report counted in `bytes/4` does not count.
- **Pass.** For every task kind of a tool, on each corpus: median ratio at least 1.5 over the
  credited tasks, and at least 0.7 of the kind's tasks credited. `code_refs` must pass on all three
  of its kinds. Latency is reported, not gating, so `--baseline`, which also fails on a p95
  latency rise, is not part of the gate.
- **Cadence.** Once per minor release; the report is committed under `benchmarks/eval/`.
- **Consequence.** A tool that fails at two consecutive minor releases is removed or reworked in
  the next surface-breaking release (ADR-0026), not in a patch. A rework resets the counter.
- **No task kind.** A tool that is neither exempt nor has a task kind by release 0.35 counts as
  failing from 0.35 on.
- **Exempt,** because no shell command does the same job: `memory_save`, `memory_search`,
  `index_status`, `index_rescan`, `agent_post`, `agent_inbox`, `agent_threads`.

| Tool | Eval task kind | Measured median, as recorded in `src/mcp/savings.rs` |
|---|---|---|
| `code_symbols` | `symbols` | 27.9x |
| `docs_search` | `docs` | 2.54x |
| `code_outline` | `outline` | 1.24x |
| `code_refs` | `references`, `callers`, `dependents` | 1.22x, 0.76x, 0.64x |
| `find_files` | `find` | 0.30x |
| `code_grep` | `grep` | no measurement recorded |
| `git_log` | `git_search` | no measurement recorded |
| `code_read`, `code_search`, `git_blame`, `git_diff` | none exists | not measured |

Those medians were taken on "a ~63k-file Python/TS monorepo" (the comment in `savings.rs`), not on
the corpus above, and no credited fraction was recorded with them. No task file and no report are
committed in `benchmarks/eval/`, so the gate has never run.

**Adoption** is a reported number without a target: per session, basemind `code_*`, `find_files`
and `git_*` calls divided by Bash `rg`/`grep` calls. ADR-0018's baseline is zero against about
60,000. The shell-call count comes from the host's transcripts, as it did for ADR-0018, or from a
counter added to the plugin's `pre-tool-guard` hook, which already matches Bash `rg`/`grep`
commands; basemind's own telemetry cannot see a host's Bash calls.

## Consequences

- On the recorded medians only `code_symbols` and `docs_search` clear 1.5; `code_outline`,
  `code_refs` and `find_files` would fail, and `code_grep` and `git_log` are unknown. Two minor
  releases confirming that would remove or rework most of the code-navigation surface, as intended.
- Outstanding before the first run: the `tokio` pin, the committed task files, and gold for a Rust
  corpus. The `symbols`, `outline`, `references`, `callers` and `dependents` generators extract
  gold with Python's `ast` module (`benchmarks/eval/_common.py`), so they yield almost nothing on
  these two repositories until they gain a Rust extractor.
- The baseline definition is part of the gate; a change to it is recorded with the report.
- The paging and path exceptions stay as they are. Removing one is a breaking schema change and
  needs no ADR; adding another does.

## Alternatives considered

- **Edit ADR-0017, ADR-0018 and ADR-0020 in place** — rejected: accepted records are immutable, and
  three of the corrections change a rule, not a typo.
- **Supersede ADR-0018 with a consolidated contract** — rejected: its core decision (one tool per
  job, exact schemas) stands; replacing the record would bury that under corrections.
- **Gate on correctness or tokens, as ADR-0018 wrote** — rejected: with "or" every tool passes on
  the dimension it happens to win, and a rule without a threshold or a consequence was not applied.
- **A threshold of 1.0 (no worse than the shell)** — rejected: a tool that only matches `rg` costs
  schema bytes in every session and gives the agent no reason to prefer it.
- **Gate on adoption instead of token ratio** — rejected as the gate, kept as a reported number:
  adoption depends on hooks and instructions as much as on the tool, and cannot be measured per
  tool before release.
- **Gate on the harness's `--baseline` regression check** — rejected: it is relative to the previous
  report, so quality may fall by its tolerance at every release, and its latency clause depends on
  the machine.
