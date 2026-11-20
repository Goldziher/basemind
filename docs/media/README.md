# Media assets

Generated demo assets embedded in the top-level `README.md`. Do not hand-edit
the binary outputs — regenerate them.

## `demo.cast` / `demo.gif` — CLI demo

The terminal demo of `basemind scan` plus the `code`, `git`, and `admin`
command groups.

- Source of truth: `demo.cast` (an [asciinema](https://asciinema.org) recording).
- Embedded asset: `demo.gif`, rendered from the cast with
  [`agg`](https://github.com/asciinema/agg).

Re-record (needs `asciinema` + `agg` on PATH — macOS: `brew install asciinema agg`):

```bash
mise run demo:record    # → scripts/demo-record.sh
```

The script records whichever binary `scripts/demo.sh` resolves: `BASEMIND_BIN`,
then `target/release/basemind`, then `basemind` on `PATH`. It builds
`target/release/basemind` with the release features (`--features full`) only
when neither of the first two exists, so a stale build is recorded as is. To
record a published release, point `BASEMIND_BIN` at it:

```bash
BASEMIND_BIN="$(command -v basemind)" mise run demo:record
```

The recorded command sequence lives in `scripts/demo.sh`; edit that to change
what the demo shows, then re-record. Preview without recording:

```bash
./scripts/demo.sh
```

## `mcp-demo.gif` — agent (MCP) demo

A live Claude Code session using the basemind MCP tools `code_outline` and
`code_refs` to answer a question from structure, not file reads.
Captured manually (a live session can't be scripted) — see
[`MCP_DEMO.md`](./MCP_DEMO.md) for the capture + conversion steps.
