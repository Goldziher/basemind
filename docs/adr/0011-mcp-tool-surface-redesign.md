# ADR-0011: MCP tool-surface redesign — consolidation into nine domains

- **Status:** Accepted (§1 and §3, sharpened to nine domains) — §2 withdrawn
- **Date:** 2026-08-05
- **Deciders:** basemind maintainers
- **Related:** GH #50 (all tools silently failed to register), ADR-0005 (CLI parity), ADR-0007
  (agent-launchable display tool)

> **Amendment (2026-08-05).** This ADR was written as a proposal and is now decided. §1
> (consolidate) and §3 (write descriptions for retrieval) are **accepted**, and §1 is taken further
> than drafted: the target is **nine domain tools**, not fifteen-to-eighteen, and the CLI collapses
> onto the same nine-name vocabulary. §2 (four per-concern servers) is **withdrawn**. The
> consequences of the original draft that no longer hold, and three mechanisms it missed, are
> recorded in *Amendment detail* at the end.

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

### 1. Consolidate into nine domains, each one tool with a required `mode`

Every operation moves behind a domain tool selected by an enumerated `mode`. The **same nine names
are the CLI groups**, so an agent that knows one surface knows the other. All 85 tools map; none are
dropped.

| Tool / CLI group | `mode` values | Count | Gate |
|---|---|---|---|
| `code` | outline, symbols, grep, files, find, definition, references, callers, implementations, dependents, expand, semantic, chunk | 13 | always |
| `graph` | calls, neighbors, path, subgraph, communities, map, export, display, open | 9 | always |
| `git` | status, recent, touching, by_path, churn, diff, diff_outline, blame, blame_symbol, symbol_history, search | 11 | always |
| `memory` | put, get, list, search, delete, audit, documents, mine, proposals, accept, reject | 11 | always¹ |
| `web` | scrape, crawl, map | 3 | `crawl` |
| `agents` | register, list, thread_start, thread_list, join, leave, members, add_member, remove_member, archive, post, history, message, inbox, ack, wait | 16 | `comms` |
| `workspace` | workspaces, worktrees, branches, claim, release | 5 | `comms` |
| `shell` | spawn, send, capture, kill, list, broadcast | 6 | `shells` |
| `admin` | status, repo, rescan, cache_stats, gc, cache_clear, telemetry, compress, delta, checkpoint, waste | 11 | always |

¹ Advertised always, body-gated on the `memory` / `documents` features — the existing pattern.

**5 tools** in a default build, **7** with `comms`, **9** with `full`.

The draft kept ten tools standalone on the reasoning that each "answers a distinct question an agent
searches for by name". That is rejected: retrieval is keyword search over descriptions, so a
standalone name earns nothing a mode clause in a denser description does not, while costing another
competitor for the same query. `outline`, `workspace_grep`, and `goto_definition` become `code` modes
like the rest.

`mode` is a required, non-defaulted enum. A default would let an agent omit it and silently get the
wrong index — the failure would look like an empty result, not an error.

The CLI expresses `mode` as real clap **subcommands** (`basemind code outline src/x.rs`), not a
`--mode` flag, so each operation keeps its own `--help` and its own argument validation.

### 2. Split into per-concern MCP servers — **WITHDRAWN**

The original proposal was four independently registrable servers (`basemind`, `basemind-agents`,
`basemind-knowledge`, `basemind-shells`) so one layer's schema defect could not take down another.
It is withdrawn for two reasons discovered after drafting:

- **`basemind serve` is no longer a server.** It is a byte relay that dials the machine daemon and
  pumps stdio, and its `RelayHello { relay_proto_ver, root, view, agent }` carries no server
  selector. A split therefore needs a `RELAY_PROTO_VER` bump *and* four MCP entries in every user's
  config — a migration paid by every user, not a front-end routing change as the draft assumed.
- **The blast radius it was buying is already guarded.** `tests/mcp_schema_wire.rs` asserts that no
  `$ref` / `$defs` / `oneOf` / `anyOf` / `allOf` reaches any tool's `inputSchema`, which is the
  construct that caused the all-or-nothing drop in the first place. A test that fails in CI is a
  cheaper containment than a config migration.

Consolidation to nine tools also shrinks the surface a defect can hide in, which was the other half
of the motivation.

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
- Migration is mechanical per domain and lands incrementally, one domain per commit. **No deprecated
  shims** — a clean break. A shim that answers to the old name keeps it competing in deferred-tool
  search, which is the cost the consolidation exists to remove.

## Alternatives considered

- **Leave the surface as-is and only fix schemas.** Rejected: it restores registration but leaves
  the retrieval dilution and the shared blast radius, both of which #50 demonstrated are real costs
  rather than theoretical ones.
- **Split servers without consolidating.** Rejected: it contains the blast radius but leaves ~40
  tools competing for the same searches inside the code-map server.
- **Consolidate without splitting.** Rejected: fewer tools still share one failure domain, so a
  comms-layer schema defect would still be able to take down code navigation.
- **A single `basemind` tool with an `action` parameter.** Rejected: it collapses retrieval entirely
  — one description cannot carry the vocabulary of 86 operations, so nothing would be findable. This
  already exists as opt-in `BASEMIND_MCP_LEAN` (`src/mcp/lean.rs`, three wrapper tools), which is
  what nine domains is calibrated against: dense enough to retrieve, not so dense that one
  description carries everything.

## Amendment detail

### Three name-keyed tables the draft missed

Consolidation moves the operation from the tool *name* into the `mode` *argument*. Four places keyed
on the name would therefore have kept compiling while silently losing per-operation behavior — none
were named in the original draft, and each fails quietly rather than loudly:

- `src/mcp/tasks.rs` — the slow-call table that decides SEP-2663 task offload. Keyed on the bare tool
  name, `web` would have offloaded *every* mode or none, since one name now covers both a multi-page
  crawl and a body-less sitemap lookup. Now keyed `"domain:mode"`.
- `src/mcp/savings.rs` — `estimate_from_text`'s per-tool baselines, same failure, same fix.
- `src/cli/render.rs` — `render_special`'s per-tool human renderers, dispatched on `(domain, mode)`.
- The ~85 `record_call(state, "<name>")` literals, which are the telemetry granularity. Now
  `mode.telemetry_key()`, so per-operation counts survive.

`Mode::telemetry_key()` in `src/mcp/mode.rs` is the single generator for all four.

### CLI parity had to be re-keyed, not just updated

`tests/cli_parity.rs` enumerates the live surface by runtime router introspection and checks it
against a name table. Left alone it would have verified **nine names** and stopped covering the 85
operations beneath them — losing the completeness gate precisely when the migration needed it most.
It is now keyed on `(tool, mode)` and walks `mode::domain_modes()`, generated from the same enums the
schemas are, so a mode without a CLI subcommand fails the build.

### Two per-tool metadata surfaces do not survive consolidation

Both are real costs, accepted rather than solved, because MCP allows exactly one of each per tool:

- **`output_schema` is dropped on all nine tools**, not only the domains whose modes visibly return
  unrelated shapes. SEP-2106 allows exactly one schema per tool, and expressing a union would mean
  nested structs — which schemars emits as `$ref` into `$defs`, the construct that dropped the whole
  registry in GH #50. Every `tools_<area>.rs` shim carries a `// No output_schema:` comment stating
  its own mode count as the reason (`code`: 13 shapes, `graph`: 9, `git`: 11, `memory`: 11, `admin`:
  11, `web`: 3, `agents`: 16, `shell`: 6, `workspace`: 3 shapes across its five modes — `workspaces`,
  `worktrees`, and `branches` list, `claim` and `release` return the same acknowledgement shape). The
  per-mode shapes are documented in the description instead.
- **Annotations coarsen to the union of the domain's modes**, and the union resolves toward the side
  effect: if any mode writes, a client that auto-approves read-only tools must not be able to trigger
  it, so the whole tool is labelled by its least-safe mode.

  `admin` bundles the read-only `status` and `repo` with `rescan` and `cache_clear`, so the tool
  advertises `read_only_hint: false` and `destructive_hint: true` for all eleven. A host that gates
  by tool-level annotations will now refuse `admin` mode `status` in a read-only context, where it
  would have allowed the old `status` tool. `memory` (write + delete modes) and `agents` (`post` /
  `join` / `register` write) land the same way: `read_only_hint: false`. `workspace` is the mild
  case — `claim`/`release` write, so it loses `read_only_hint`, but every mode is idempotent and none
  destroys data, so `destructive_hint` stays `false`. Splitting a domain by mutability would restore
  the hint at the cost of reintroducing the name proliferation this ADR removes, so the hint loses.

  `graph` pays the same cost for a different reason: seven of its nine modes are pure reads, but
  `display` and `open` launch a viewer on the human's session by default, so the tool takes the
  side-effecting side of the union (`read_only_hint: false`, `open_world_hint: true`). `open: false`
  is the per-call escape hatch that keeps those two modes pure.

  `shell` is the sharpest case: `spawn` / `send` / `broadcast` write and `kill` terminates a process,
  so the tool can claim neither `read_only_hint` nor `idempotent_hint`, and it advertises
  `destructive_hint: true` even though `capture` and `list` are pure reads.

  `code` and `git` are the counterexample that shows the cost is not universal: every mode in both
  domains is a pure read (no mode writes, deletes, or launches anything), so both keep
  `read_only_hint: true` — consolidation only coarsens a domain that actually mixes reads with
  writes or side effects.

### Enum schemas must be hand-written

`#[derive(JsonSchema)]` on an enum whose variants carry doc comments emits
`oneOf: [{const, description}, …]` — the exact construct that silently dropped the whole registry in
GH #50. Every `mode` enum therefore hand-writes a flat `{"type": "string", "enum": [...]}` with the
variant meanings folded into `description`, and opts into `inline_schema()`. `define_mode!` in
`src/mcp/mode.rs` generates this; `Visibility` in `src/mcp/types_memory.rs` was the pattern.
