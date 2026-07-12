---
name: bm-stats
description: Show the basemind dashboard — resource footprint (disk + RAM) and activity (tool calls, per-tool histogram, estimated tokens saved). Works with or without the MCP server.
argument-hint: [today|1h|24h|all]
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:134dcd4fa2a3179ed99a6d3f30537d0fbd428229cda0872f969ff0496b0f4fde
Source-Hash: blake3:ca37d0bd1accfd4a3156158a1f47fd4568c00994d83f95330f41124a857f34dc
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

1. **Resource footprint.** MCP tool `cache_stats`, or CLI `basemind cache stats` (add `--json` to
   parse). Report: `total_bytes` (matches `du`), the per-component breakdown (blobs / views /
   git-history / lance / git-cache / telemetry / other), and process RAM (`rss_bytes` +
   `peak_rss_bytes`). If `blob_accounting_ok` is `false`, note that orphan accounting was skipped
   (stale/unreadable index — re-scan to restore it); the sizes are still accurate.

2. **Activity.** MCP tool `telemetry_summary`, or CLI `basemind telemetry --window <today|1h|24h|all>`
   (add `--json`). Report call count, the per-tool histogram, and estimated tokens saved for the
   window.

3. **Render** both sections as a compact markdown dashboard.

## Notes

- Prefer the MCP tools when connected, but don't depend on them — the CLI reads the same data
  with no server. If a step's MCP tool isn't available, run its CLI equivalent instead of giving up.
- If neither MCP nor CLI is reachable, say so plainly and point at `/bm-doctor`.
- Always end with a one-sentence disclosure that the savings number is heuristic: tools without a
  realistic baseline (memory, document search, git wrappers) report 0 saved.

## See also

The `basemind-stats` skill for the render shape and the `--explain` breakdown.
