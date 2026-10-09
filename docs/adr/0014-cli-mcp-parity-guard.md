# ADR-0014: Executable CLI/MCP parity guard

- **Status:** Accepted
- **Date:** 2026-10-09
- **Deciders:** basemind maintainers
- **Related:** ADR-0011

## Context

ADR-0011 consolidated the MCP surface into nine domain tools that dispatch on a required `mode`.
The contract is that every operation an agent can invoke over MCP is also reachable from the CLI
with the same parameters and the same answer. A tool-name-keyed check cannot hold that: the
operations live inside `mode`, so adding a mode would silently go uncovered. In practice the two
surfaces drifted (flag names, allowed values, defaults, operations only reachable on one side).

## Decision

`tests/cli_parity/` guards parity in three layers:

1. **Coverage** (`capabilities.rs`): one checked-in table maps each MCP `(tool, mode)` to its CLI
   command, or declares it `cli_only` / `mcp_only` with a required reason. The test enumerates the
   live tool schemas and the live command tree (the built binary's `-h`) and fails on an uncovered
   mode or command, a stale row, an empty reason or a duplicate, printing the exact row to add.
2. **Parameters** (`params.rs`, `exceptions.rs`): for each pair, the fields the MCP mode accepts
   (probed from the server's own validator) are compared with the command's arguments: names, shape,
   allowed values, defaults and required-ness. Each difference needs a reasoned exception; stale
   exceptions fail.
3. **Behaviour** (`behaviour.rs`): representative read-only queries run through the in-process MCP
   server and through `basemind --json` over one fixture repo and must agree.

Closed value sets are clap value enums on the CLI (`src/cli/choices.rs`) while MCP keeps free
strings; the parameter layer compares the two sets.

## Consequences

- Adding or renaming a mode fails the build until the CLI command and the table row exist.
- Operations with no counterpart (`init`, `serve`, `doctor`, `completions`, `man`, `admin tokens`,
  `admin eval`, the `comms` lifecycle) are explicit, with reasons.
- The CLI runs the same tool code as MCP, so the behaviour layer is a check on rendering and
  argument mapping, not on a second implementation.
- Some MCP modes sit at the top level of the CLI (`status`, `rescan`, `cache`, `delta`,
  `checkpoint`, `detect-waste`); the table is the source of that mapping.

## Alternatives considered

- **Key the table on tool names** - rejected: it stops covering modes silently.
- **Generate the CLI from the MCP schemas** - rejected: loses per-command `--help`, value-enum
  validation and shell completions.
- **Document parity and review it by hand** - rejected: it had already drifted.
