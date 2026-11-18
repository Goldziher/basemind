---
status: accepted
date: 2026-11-18
amends: ["0030"]
---

# ADR-0038: Comms v2 cutover as built — agent tool replay and the closed open items

- **Deciders:** the repository owner
- **Related:** amends ADR-0030; ADR-0013, ADR-0021, ADR-0022

## Context

ADR-0030 recorded the rules as built while the mode-dispatched `agents` tool was still served. Its
rule 10 decides relay replay for `agents` by mode, and two of its open items describe that tool and
a goto-definition answer the daemon took from another snapshot. The comms v2 cutover replaces
`agents` with `agent_post`, `agent_inbox` and `agent_threads` (ADR-0021), and refuses a daemon
definition edge whose use file changed.

## Decision

ADR-0030 stands except where this record changes it.

1. **Rule 10 becomes:** replay safety is decided per tool in `src/mcp/replay.rs`, every advertised
   tool in exactly one list, enforced by a test: read-only verb tools are re-sent
   (`REPLAY_SAFE_TOOLS`), `memory_save` and `index_rescan` never (`NEVER_REPLAY_TOOLS`), and
   `agent_post`, `agent_inbox` and `agent_threads` are re-sent with the relay's replay key
   (`KEYED_REPLAY_TOOLS`). The decision is made when a tool is added.
2. **Two open items are closed.** The `agents` tool no longer stands in for ADR-0022's three agent
   tools. A `code_refs` definition the daemon answers carries the use file's hash and the settled
   flag read before the edge, and the session refuses an edge whose hash differs from its store's.

## Consequences

The replay rule names the three agent tools and no mode. The remaining open items of ADR-0030 are
unchanged.

## Alternatives considered

- **Editing ADR-0030.** It was accepted before the cutover landed, so it is immutable.
