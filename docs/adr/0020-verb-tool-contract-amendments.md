---
status: accepted
date: 2026-11-15
amends: ["0018"]
---

# ADR-0020: Verb-tool contract — no output schemas, always-load scope, kept domain tools

- **Status:** Accepted; amended by ADR-0021 and ADR-0022 (2026-11-15)
- **Date:** 2026-11-15
- **Deciders:** basemind maintainers
- **Related:** amends ADR-0018; ADR-0014

## Context

ADR-0018 replaced the mode-dispatched domain tools with verb tools and listed what every tool
carries. Four of its statements do not describe what shipped, and the difference is deliberate in
each case:

- **"Every tool has an output schema."** The response structs nest (a hit list inside a page, a
  definition inside a callers response). schemars emits a nested struct as a `$ref` into `$defs`,
  the construct that silently dropped the whole tool registry in GH #50 and that
  `tests/mcp_schema_wire.rs` forbids. Inlining every nested struct by hand would also put
  `tools/list` over its byte ceiling: the fifteen input schemas and descriptions already use about
  18 KB of the 20 KB budget.
- **`_meta["anthropic/alwaysLoad"]: true` on every tool.** The marker keeps a schema loaded in hosts
  that defer MCP tools. Applied to every advertised tool it also pinned the two remaining domain
  tools, which cost 11 KB between them and are used only when agents coordinate.
- **The agent tools `agent_post`, `agent_inbox`, `agent_threads`.** They were not built. The
  coordination surface has its own open design questions (thread addressing, delivery, launching
  agents), and splitting sixteen modes into three verb tools without settling them would freeze the
  wrong shape.
- **"`src/mcp/mode.rs` and the per-mode allow-lists are deleted."** They are still what the two kept
  domain tools run on.

## Decision

1. **No verb tool declares an output schema.** Each tool's description states what it returns, and
   the response fields are documented on the response structs. A test asserts the absence on every
   advertised tool, so a later "add the output schema back" change fails instead of taking the
   registry down. Structured content is still returned with every result.
2. **Only the verb tools are marked always-load.** `finalize_router` sets the marker for the names
   in `VERB_TOOLS` and for nothing else.
3. **`agents` and `workspace` stay mode-dispatched domain tools** until the comms redesign replaces
   them. They keep their lenient argument parsing, their own byte ceiling in
   `tests/mcp_schema_wire.rs`, and stay deferred in hosts that defer tools.
4. **`src/mcp/mode.rs` is trimmed, not deleted.** It carries the `define_mode!` macro, the `agents`
   and `workspace` vocabularies and the per-mode field validator, and nothing for the replaced
   domains.

## Consequences

- A host cannot validate a verb tool's result against a schema. The cost is accepted: no host in
  use validates results, and every host rejects the registry on a `$ref`.
- A session pays about 18 KB for the verb tools at start and nothing for `agents` / `workspace`
  until a host loads them through tool search.
- The CLI parity rules of ADR-0014 as amended by ADR-0018 are unchanged: the two domain tools keep
  their mode-to-subcommand bijection, the verb tools their injective mapping.
- The comms redesign owns the removal of `mode.rs`, of `lenient.rs` and of the domain-tool ceiling.
- `code_refs` pages as ADR-0018 says every list does, with one bound of its own: an exact scan
  examines at most `SCAN_KEYS_EXAMINED_MAX` index keys per page, so a page can be empty and still
  carry a `next_cursor`. An older client talking to a newer daemon does not follow that cursor: it
  stops at the cap with `total_is_partial` set, which is accepted because client and daemon ship
  together.

## Alternatives considered

- **Flatten the response structs so output schemas need no `$ref`** — rejected: it would reshape
  every response to satisfy a schema no host reads, and still exceed the byte ceiling.
- **Raise the byte ceiling to fit output schemas** — rejected: the ceiling is what keeps the surface
  cheap enough to load on every session, which is the point of ADR-0018.
- **Mark `agents` and `workspace` always-load until they are redesigned** — rejected: it charges
  every session for a surface most sessions never call.
