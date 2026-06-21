# Media assets

Generated demo assets embedded in the top-level `README.md`. Do not hand-edit
the binary outputs — regenerate them.

## `demo.cast` / `demo.gif` — CLI demo

The terminal demo of `basemind scan` + the query / git / telemetry surface.

- Source of truth: `demo.cast` (an [asciinema](https://asciinema.org) recording).
- Embedded asset: `demo.gif`, rendered from the cast with
  [`agg`](https://github.com/asciinema/agg).

Re-record (needs `asciinema` + `agg` on PATH — macOS: `brew install asciinema agg`):

```bash
task demo:record        # → scripts/demo-record.sh
```

The recorded command sequence lives in `scripts/demo.sh`; edit that to change
what the demo shows, then re-record. Preview without recording:

```bash
./scripts/demo.sh
```

## `mcp-demo.gif` — agent (MCP) demo

A live Claude Code session using the basemind MCP tools (`outline` +
`find_references`) to answer a question from structure, not file reads. Captured
manually (a live session can't be scripted) — see [`MCP_DEMO.md`](./MCP_DEMO.md)
for the capture + conversion steps.
