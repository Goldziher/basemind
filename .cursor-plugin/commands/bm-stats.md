---
name: bm-stats
description: Show basemind activity dashboard — tool calls, per-tool histogram, estimated tokens saved.
---

# bm-stats — basemind activity dashboard

Call the basemind MCP tool `telemetry_summary` and render the result as a
markdown dashboard.

Default window is `today`. If the user asks for a specific range, map it to one
of `today`, `1h`, `24h`, `all`.

Always end with a one-sentence disclosure that the savings number is heuristic:
tools without a realistic baseline (memory, document search, git wrappers)
report 0 saved.
