---
name: basemind-stats
description: >-
  Show a quick dashboard of basemind activity in this session: how many code-map
  tool calls have run, the per-tool histogram, and the estimated tokens saved vs
  a hypothetical grep+Read baseline. Use when the user asks "what has basemind
  done?", "how much is basemind helping?", "show me basemind stats", or invokes
  `/bm-stats` directly.
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:4802a612980221e429ab0d17a26e2a80962f696fa863bbfddff268b28b0a4583
Source-Hash: blake3:813d6191ae6303f4be35bdc1e26b48c822a4743273a80fde66a8dda95fe91320
Schema-Version: v1
-->

# basemind-stats — on-demand usage dashboard

Call `admin` mode `telemetry` and render the result as a markdown report.

## When to use

The user asks "what has basemind done?", "how much is basemind helping?", "show me basemind
stats", or invokes `/bm-stats` directly. This skill is strictly user-invoked — see Notes.

## How to run

1. Call `admin { "mode": "telemetry", "window": "today" }` (the default). If the
   user asks for a specific range, map it to one of `"today"`, `"1h"`, `"24h"`,
   `"all"`.
2. Render a markdown block in this shape:

   ```text
   ## basemind activity (today)
   - **N tool calls** ; top operations: code:outline (18), code:symbols (12), …
   - **~K tokens saved** vs grep + Read baseline
   - recent: code:outline (4ms, 312B), code:symbols (2ms, 180B), …
   ```

3. If `total_calls` is 0, say so plainly ("no basemind activity in the window yet").
   Don't pretend to have data.
4. **Always disclose the savings model.** Add one sentence at the end:

   > Savings are heuristics. Tools with no realistic baseline (memory, document
   > search, git wrappers) report 0 saved — see the `saved_baseline` column on
   > each row.

   The exact wording can vary; the principle (it's an estimate, here's why) cannot.

## When the user asks "--explain"

If they invoke `/bm-stats --explain` or ask how the savings number is
derived, include the per-baseline breakdown from the `per_baseline` field of the
response and call out which tools fall into which bucket. The estimator lives in
`src/mcp/savings.rs` if they want to read the code.

## Notes

- Don't auto-display the dashboard at the start of every conversation. This skill
  is strictly user-invoked.
- Don't pad missing data. If `recent` is empty, say so; don't invent example rows.
- Don't claim a token-savings number without the disclosure sentence.
