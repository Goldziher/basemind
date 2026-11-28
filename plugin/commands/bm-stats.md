---
description: Show the basemind dashboard — resource footprint (disk + RAM) and activity (tool calls, per-tool histogram, estimated tokens saved). Works with or without the MCP server.
argument-hint:
    - today|1h|24h|all
name: bm-stats
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:e7fbacac41c3382282e4c1a8bb065251bdda8628338472e1c83382783d4702b1
Source-Hash: blake3:28b6945a1c1aec38445415d07ec31c0a282eb6999b281a2d408a58b233dd4fd2
Schema-Version: v1
-->

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
  server. Run them through the plugin launcher, `${CLAUDE_PLUGIN_ROOT:-$CURSOR_PLUGIN_ROOT}/scripts/mcp-launch.sh <args>`
  (bare `basemind` from `PATH` when neither plugin root variable is set).
- If the CLI is not reachable, say so plainly and point at `/bm-doctor`.
- Always end with a one-sentence disclosure that the savings number is heuristic: tools without a
  realistic baseline (memory, document search, git wrappers) report 0 saved.

## See also

The `basemind-stats` skill for the render shape and the `--explain` breakdown.
