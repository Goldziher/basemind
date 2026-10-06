---
name: bm-doctor
description: Diagnose and recover basemind when it isn't working (MCP tools missing/erroring, "no index", dead server) — runs CLI checks and gives the client-specific way to reconnect the server.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:1b2c3b66cf0f6037df18a3c73889ea5734f7ab5926f68c3734bd8fa486f4e11c
Source-Hash: blake3:c1a08b89b18aa5d4c2259405307482d21d84a33d481461a2a4e78b954c6234cb
Schema-Version: v1
-->

# bm-doctor — diagnose and recover basemind

Diagnose and recover basemind using the CLI (works even with no MCP server running).

## When to use

basemind isn't behaving: MCP tools are missing or erroring, the statusline or a tool reports
"no index" / "no indexed files", results are empty when they shouldn't be, or the `basemind
serve` MCP server seems dead.

## How to use

Invoke `/bm-doctor` (optional free-text detail, e.g. `/bm-doctor tools return no indexed files`).
It runs the checks below in order:

1. Check the index: `basemind admin status`.
2. Check for a lock-holding server: a `basemind scan` lock error names the holder (command + pid) from the workspace cache's `.lock.meta`.
3. Rebuild the index if needed: `basemind scan`.
4. Reconnect the MCP server (client-specific — this is the only way to restart it).

## Notes

- A stdio MCP server can't be restarted by an agent or by basemind itself; reconnecting it is
  the MCP client's job. The CLI stays usable throughout.

## See also

The `basemind-doctor` skill for the full step-by-step diagnostic workflow, lock-holder detection,
and log-reading guidance.
