---
name: bm-stats
description: basemind dashboard — resource footprint (disk + RAM) and activity (tool calls, per-tool histogram, estimated tokens saved). Works with or without the MCP server.
---

# bm-stats — basemind dashboard

Show a basemind dashboard with two sections: **resource footprint** (on-disk size + process RAM) and
**activity** (tool calls, per-tool histogram, estimated tokens saved). $ARGUMENTS

Prefer the MCP tools when they're connected, but do **not** depend on them — the CLI reads the same
data with no server (the MCP server can be reconnecting, or not running at all). If a step's MCP tool
isn't available, run its CLI equivalent instead of giving up.

Default window is `today`. If the user asks for a range, map it to one of `today`, `1h`, `24h`, `all`.

1. **Resource footprint.** MCP tool `cache_stats`, or CLI `basemind cache stats` (add `--json` to
   parse). Report: `total_bytes` (matches `du`), the per-component breakdown (blobs / views /
   git-history / lance / git-cache / telemetry / other), and process RAM (`rss_bytes` +
   `peak_rss_bytes`). If `blob_accounting_ok` is `false`, note that orphan accounting was skipped
   (stale/unreadable index — re-scan to restore it); the sizes are still accurate.

2. **Activity.** MCP tool `telemetry_summary`, or CLI `basemind telemetry --window <today|1h|24h|all>`
   (add `--json`). Report call count, the per-tool histogram, and estimated tokens saved for the
   window.

3. **Render** both sections as a compact markdown dashboard.

If neither MCP nor CLI is reachable (no `basemind` binary and no server), say so plainly and point at
`/bm-doctor`.

Always end with a one-sentence disclosure that the savings number is heuristic: tools without a
realistic baseline (memory, document search, git wrappers) report 0 saved.
