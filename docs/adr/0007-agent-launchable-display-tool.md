# ADR-0007: Agent-launchable UI display tool

- **Status:** Proposed
- **Date:** 2026-08-02
- **Deciders:** basemind maintainers
- **Related:** ADR-0005 (rendering engine), ADR-0006 (interactive UI)

## Context

Beyond a human opening the UI, the user wants an **agent** to open a window and show a human data —
"an agent can launch a UI window and display data to a user via a tool." That is a distinct
capability from the `basemind ui` subcommand (ADR-0006): here the initiative is the agent's, mid-task,
pushing a view onto a human's screen — the agent's human-facing output channel.

The pieces to build on already exist: the interactive UI (ADR-0006), the canonical view payload that
renders it (ADR-0005), and the agent layer's per-workspace local IPC transport that the engine
already uses to talk to its front-ends.

## Decision

Add an **MCP tool that pushes a typed view to a running basemind UI window, launching one if none is
open.** The view is a **typed descriptor** — a graph or subgraph, a search result, a file span — not
rendered bytes, so the window renders it with the shared engine (ADR-0005). Transport reuses the agent
layer's existing **per-workspace IPC** with a dedicated display channel. This is the "agent shows the
user" primitive.

When no display is available (headless, no UI instance), the tool **degrades gracefully** to writing
a rendered export (ADR-0005) and returning its path, rather than failing.

## Consequences

- Agents gain a human-facing output channel: a reviewer or pair sees what the agent found, not just a
  text description of it.
- Reuses transport and rendering already built for the agent layer and the UI — one display path, one
  rendering path.
- The view descriptor is the same payload the UI and the exports use, so there is no bespoke display
  format to maintain.
- Trade-off: the rich path needs a UI instance and a live display channel; the graceful export
  fallback keeps the tool useful headless.
- Feasibility constraint: an MCP server often runs with no attached GUI session — headless, over SSH,
  or launched in a background/daemon context — where it cannot open a window at all, and even on a
  local desktop the serve process's access to the user's display/session is not guaranteed. The export
  fallback is therefore the reliable baseline and the live window is the enhancement, not the reverse;
  "launch if absent" will legitimately land on the fallback in many environments.

## Alternatives considered

- **Return a path to a rendered HTML the human opens manually.** This is exactly the graceful
  fallback, but on its own it is not "the agent opened a window" — so it is the fallback, not the
  primary behavior.
- **Stream rendered images over MCP.** Rejected: heavy, and it throws away interactivity that the
  descriptor + shared engine preserve.
- **A separate socket protocol dedicated to display.** Rejected: reuse the workspace IPC the agent
  layer already has rather than add a parallel transport.
