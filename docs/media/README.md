# Media assets

Generated demo assets. Do not hand-edit the binary outputs; regenerate them
with [vhs](https://github.com/charmbracelet/vhs) (and `ffmpeg`, `git`, `python3`):

```bash
BASEMIND_BIN=/path/to/basemind mise run demo:record          # all three
BASEMIND_BIN=/path/to/basemind ./scripts/demo-record.sh mcp  # one of cli|mcp|comms
```

`BASEMIND_BIN` defaults to `target/release/basemind`, then `basemind` on `PATH`.
Each recording uses a throwaway `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`,
and the MCP and comms demos use a shallow clone of a public project.

## `demo.gif` - CLI demo

`basemind scan` plus the `code`, `git` commands on this repository. The command
sequence lives in `scripts/demo.sh`; edit it, then re-record. Preview without
recording: `./scripts/demo.sh`.

## `mcp-demo.gif` - MCP demo

An MCP client driving `basemind serve` over stdio (`code_outline`, `code_refs`).
See [`MCP_DEMO.md`](./MCP_DEMO.md).

## `../demos/comms-demo.gif` - agent messaging demo

Two agent identities coordinating with `basemind agent post|threads|inbox`
(`scripts/demo-comms.sh`). Needs a `comms` build.
