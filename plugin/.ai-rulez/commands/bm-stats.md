---
type: Reference
title: Bm Stats
description: Show the basemind dashboard — resource footprint (disk + RAM) and activity (tool calls, per-tool histogram, estimated tokens saved). Works with or without the MCP server.
x-ai-rulez:
  kind: command
  id: bm-stats
  metadata:
    argument-hint:
      - today|1h|24h|all
    name: bm-stats
---

# bm-stats — basemind dashboard

Show a basemind dashboard with two sections: resource footprint (on-disk size + process RAM) and
activity (tool calls, per-tool histogram, estimated tokens saved).

## When to use

The user asks "how much is basemind helping?", "show me basemind stats", or wants to check disk /
RAM usage.

## How to use

Invoke `/bm-stats` (default window `today`) or `/bm-stats <today|1h|24h|all>`. Window: $ARGUMENTS

1. **Resource footprint.** CLI `basemind cache stats` (add `--json` to parse). Report: `total_bytes` (matches `du`), the per-component breakdown (blobs / views /
   git-history / lance / git-cache / telemetry / other), and process RAM (`rss_bytes` +
   `peak_rss_bytes`). If `blob_accounting_ok` is `false`, note that orphan accounting was skipped
   (stale/unreadable index — re-scan to restore it); the sizes are still accurate.

2. **Activity.** CLI `basemind admin telemetry --window <today|1h|24h|all>` (add `--json`). Report call count, the per-tool histogram, and estimated tokens saved for the
   window.

3. **Render** both sections as a compact markdown dashboard.

## Notes

- Both are CLI commands with no MCP tool; they read the same data with or without a running
  server. Run them through the plugin launcher, `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh <args>`
  (bare `basemind` from `PATH` when no plugin launcher exists).
- If the CLI is not reachable, say so plainly and point at `/bm-doctor`.
- Always end with a one-sentence disclosure that the savings number is heuristic: tools without a
  realistic baseline (memory, document search, git wrappers) report 0 saved.

## See also

The `basemind-stats` skill for the render shape and the `--explain` breakdown.
