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

## MCP-session video

The live agent demo is captured manually — see [`MCP_DEMO.md`](./MCP_DEMO.md).
The MP4 is **not** committed here; it is hosted via GitHub user-attachments and
embedded by URL in the top-level `README.md`.
