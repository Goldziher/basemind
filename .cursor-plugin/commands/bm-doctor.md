---
description: Diagnose and recover basemind when it isn't working (MCP tools missing/erroring, "no index", dead server) — runs CLI checks and gives the client-specific way to reconnect the server.
name: bm-doctor
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:ba948a4477edcb08beeb6416e450db654cc64a68779f5a0d38361b36576ccb4d
Source-Hash: blake3:e146284b6cc772d8544f539c0212b9a4ded40fbbc37dc231af438266f4887b8d
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
It runs the checks below in order. Run every `basemind …` command through the plugin-managed,
version-matched binary, `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh <args>` (in other hosts,
`scripts/mcp-launch.sh` in the plugin directory), and use bare `basemind` from `PATH` only when no
plugin launcher exists:

1. Check the index: `basemind status`.
2. Check for a lock holder: with a daemon running and no other process holding the writer lock, `basemind scan` is forwarded to it; when another process holds the lock the scan runs directly and reports that holder, and otherwise a lock error (exit 3) names the holder (command + pid) when the workspace cache's `.lock.meta` records it, and is generic when it does not.
3. Rebuild the index if needed: `basemind scan`.
4. Reconnect the MCP server (client-specific — this is the only way to restart it).

## Notes

- A stdio MCP server can't be restarted by an agent or by basemind itself; reconnecting it is
  the MCP client's job. The CLI stays usable throughout.

## See also

The `basemind-doctor` skill for the full step-by-step diagnostic workflow, lock-holder detection,
and log-reading guidance.
