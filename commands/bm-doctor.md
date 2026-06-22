---
name: bm-doctor
description: Diagnose and recover basemind when it isn't working (MCP tools missing/erroring, "no index", dead server) — runs CLI checks and gives the client-specific way to reconnect the server.
---

# bm-doctor — diagnose and recover basemind

basemind isn't behaving (MCP tools missing or erroring, "no index" / "no indexed files", empty
results, or the MCP server seems dead). Diagnose and recover using the **CLI** (works with no MCP
server). $ARGUMENTS

Note: a stdio MCP server can't be restarted by an agent or by basemind itself — reconnecting it is
the MCP client's job (step 4). You can still make the index healthy from here.

1. **Index present?** `basemind query status` — errors / `file_count: 0` → build it with
   `basemind scan` (or `/bm-scan`). Healthy count → it's a connection problem (step 4).

2. **Server already running?** If `basemind scan` errors on the lock, check the holder:
   `cat .basemind/.lock.meta` (shows `command` + `pid`). Live pid → server is up, use the MCP
   tools / `rescan`. Dead pid → stale lock; the OS already released it, so retry (you may delete
   the stale `.basemind/.lock.meta`).

3. **Rebuild if needed:** `basemind scan` (non-extractable files are skipped, not failed).

4. **Reconnect the MCP server (only way to restart it):** in Claude Code, reconnect the basemind
   MCP server from the MCP UI or restart the session (the launcher re-execs the binary on the next
   connection); in Cursor/others, toggle the server in MCP settings. Meanwhile you're not blocked —
   the `basemind query …` / `basemind git …` CLI reads the same index with no server.

`basemind serve` logs its startup (pid/version/view) and exit reason to stderr — captured in the
client's MCP server logs; if serve keeps dying, that line names why.
