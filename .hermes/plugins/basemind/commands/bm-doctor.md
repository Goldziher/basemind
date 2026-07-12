---
name: bm-doctor
description: Diagnose and recover basemind when it isn't working (MCP tools missing/erroring, "no index", dead server) — runs CLI checks and gives the client-specific way to reconnect the server.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:bddb5b7c433220cce32a3cad1c9011d9f1fc6b05e2420bb09f7dfd280b1da2e7
Source-Hash: blake3:ca37d0bd1accfd4a3156158a1f47fd4568c00994d83f95330f41124a857f34dc
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

1. Check the index: `basemind query status`.
2. Check for a lock-holding server: `cat .basemind/.lock.meta`.
3. Rebuild the index if needed: `basemind scan`.
4. Reconnect the MCP server (client-specific — this is the only way to restart it).

## Notes

- A stdio MCP server can't be restarted by an agent or by basemind itself; reconnecting it is
  the MCP client's job. The CLI stays usable throughout.

## See also

The `basemind-doctor` skill for the full step-by-step diagnostic workflow, lock-holder detection,
and log-reading guidance.
