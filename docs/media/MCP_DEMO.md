# Agent (MCP) demo

`mcp-demo.gif` shows what an MCP host sees: `basemind serve` is a stdio JSON-RPC
server, so the recording drives it with `scripts/demo-mcp.py`, a small client
that sends `initialize`, `tools/list`, then `tools/call` for `code_outline` and
`code_refs`. It shows the answers are paths, line numbers and signatures, not
file bodies. A live host session is not recorded: it needs a model, and the
recording must be reproducible.

## Record

```bash
BASEMIND_BIN=/path/to/basemind mise run demo:record mcp
```

`scripts/demo-record.sh mcp` shallow-clones a small public project
(`psf/requests`) into a temp directory, scans it with a throwaway
`BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR`, records with
[vhs](https://github.com/charmbracelet/vhs), and writes `docs/media/mcp-demo.gif`.
The temp directories are removed on exit.

Every tool on screen must be one the binary serves (`VERB_TOOLS`,
`src/mcp/types_tools.rs`): `tools/list` is printed from the live server, so a
renamed tool fails the recording instead of showing a stale name. Check the
frames for local paths, usernames and secrets before committing; the recorded
shell uses a bare `$` prompt and a relative `.` root for that reason.
