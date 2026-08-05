# ADR-0011: MCP tool-surface redesign — consolidation and per-concern servers

- **Status:** Proposed
- **Date:** 2026-08-05
- **Deciders:** basemind maintainers
- **Related:** GH #50 (all tools silently failed to register), ADR-0005 (CLI parity), ADR-0007
  (agent-launchable display tool)

## Context

GH #50 exposed two problems at once. The acute one — schemas the client rejected, and a config the
daemon could not parse — is fixed (`a59b114`, `ea6f5ba`) and guarded by `tests/mcp_schema_wire.rs`.
This ADR covers the structural half that those fixes revealed but did not address.

**The surface is too large and undifferentiated.** `src/mcp/tools*.rs` declares **86** `#[tool]`
methods across 15 files (76 served in a `comms` build):

| Area | Tools | Area | Tools |
|---|---|---|---|
| comms | 16 | admin | 5 |
| core code-map (`tools.rs`) | 13 | web | 3 |
| git | 11 | traverse | 3 |
| memory | 7 | graphview | 3 |
| shells | 6 | code | 2 |
| registry | 5 | community | 1 |
| governance | 5 | archmap | 1 |
| compress | 5 | | |

Three consequences, each observed rather than hypothesised:

1. **Deferred retrieval dilutes.** Hosts defer MCP tools and surface them only through keyword
   search, so an agent never sees a list — it searches. Every additional tool competes for the same
   query, and a tool whose description omits the words an agent would actually type is invisible in
   practice even when registered.
2. **One blast radius.** A schema defect in the comms layer took down code navigation entirely,
   because the whole `tools/list` array is accepted or rejected as a unit. The layers share a failure
   domain despite sharing nothing else.
3. **Unrelated concerns travel together.** Roughly 40 of the 86 (`agent_*`, `thread_*`, `shell_*`,
   `web_*`, `memory_*`, `worktree_*`, `proposal_*`) have nothing to do with the code map, yet every
   consumer of the code map pays their schema cost, their retrieval noise, and their risk.

## Decision

### 1. Consolidate mode-variants behind a `mode` parameter

Collapse families that differ only in *which index they consult* into one tool with an enumerated
`mode`, keeping distinct tools where the **question** is genuinely different:

| New tool | Replaces | `mode` values |
|---|---|---|
| `search` | `search_symbols`, `search_code`, `search_documents`, `search_git_history` | `symbols` \| `code` \| `documents` \| `history` |
| `references` | `find_references`, `find_callers`, `find_implementations` | `any` \| `callers` \| `implementations` |
| `graph` | `call_graph`, `neighbors`, `subgraph`, `path`, `communities`, `architecture_map` | `calls` \| `neighbors` \| `subgraph` \| `path` \| `communities` \| `map` |
| `history` | `recent_changes`, `commits_touching`, `find_commits_by_path`, `hot_files`, `symbol_history` | `recent` \| `touching` \| `by_path` \| `churn` \| `symbol` |
| `blame` | `blame_file`, `blame_symbol` | `file` \| `symbol` |
| `diff` | `diff_file`, `diff_outline` | `file` \| `outline` |
| `thread` | the 10 `thread_*` tools | `start` \| `list` \| `join` \| `leave` \| `post` \| `history` \| `members` \| `add_member` \| `remove_member` \| `archive` |
| `inbox` | `inbox_read`, `inbox_ack`, `message_get` | `read` \| `ack` \| `get` |
| `memory` | the 6 `memory_*` tools | `put` \| `get` \| `list` \| `search` \| `delete` \| `audit` |
| `web` | `web_scrape`, `web_crawl`, `web_map` | `scrape` \| `crawl` \| `map` |
| `shell` | the 6 `shell_*` tools | `spawn` \| `send` \| `capture` \| `kill` \| `list` \| `broadcast` |

Kept standalone because each answers a distinct question an agent searches for by name: `outline`,
`list_files`, `find_files`, `workspace_grep`, `goto_definition`, `dependents`, `status`, `rescan`,
`ui`/`display`, `working_tree_status`.

Target: **~15–18 tools** in the code-map server, down from 86 total.

`mode` is a required, non-defaulted enum. A default would let an agent omit it and silently get the
wrong index — the failure would look like an empty result, not an error.

### 2. Split into per-concern MCP servers

Four servers, each independently registrable so one layer's defect cannot take down another:

| Server | Contents | Availability |
|---|---|---|
| `basemind` (code map) | search / references / graph / outline / navigation / git / admin | always |
| `basemind-agents` | thread / inbox / agent / registry / governance / worktree | `--features comms` |
| `basemind-knowledge` | memory / documents / web | `--features documents,memory,crawl` |
| `basemind-shells` | shell | `--features shells` |

`basemind serve` keeps serving the code map, so existing configs continue to work; the others are
opt-in endpoints. The daemon already multiplexes by `?root=`, so this is a routing change at the
front-end, not a new process per server.

### 3. Write descriptions for retrieval, not for documentation

Because discovery is keyword search, each description must contain the words an agent actually types
— "grep", "find definition", "who calls this", "read this instead of opening the file" — in addition
to the precise contract (substring vs prefix, scope-aware vs name-only, what is capped). This is a
retrieval requirement, not a style preference: vocabulary absent from the description makes the tool
unreachable.

## Consequences

- **Breaking.** Tool names change. This is a minor-release cut with a `RELEASE_MINOR` bump, a
  CHANGELOG entry, and a rename table for anyone with pinned tool names.
- **CLI parity must move in lock-step.** `tests/cli_parity.rs` asserts a strict bijection, so every
  consolidation needs the matching CLI verb reshaped in the same commit — this is the completeness
  gate, and it is what keeps the two surfaces honest.
- **Per-tool schemas get larger** (a union of each mode's parameters). The wire-shape guard still
  applies: no `$ref`/`$defs`/`oneOf`/`anyOf`/`allOf`, which means mode-specific parameters are
  optional siblings validated in the helper, not a schema-level union. Validation moves from the
  schema into the helper, and must return a precise error naming the offending `mode`/field pair.
- **Fewer, denser tools improve retrieval** — the reason for the change — but each remaining
  description now carries more vocabulary and must be maintained deliberately.
- Migration is mechanical per family and can land incrementally: one family per commit, old name
  retained as a thin deprecated shim for one minor cycle where cheap.

## Alternatives considered

- **Leave the surface as-is and only fix schemas.** Rejected: it restores registration but leaves
  the retrieval dilution and the shared blast radius, both of which #50 demonstrated are real costs
  rather than theoretical ones.
- **Split servers without consolidating.** Rejected: it contains the blast radius but leaves ~40
  tools competing for the same searches inside the code-map server.
- **Consolidate without splitting.** Rejected: fewer tools still share one failure domain, so a
  comms-layer schema defect would still be able to take down code navigation.
- **A single `basemind` tool with an `action` parameter.** Rejected: it collapses retrieval entirely
  — one description cannot carry the vocabulary of 86 operations, so nothing would be findable.
