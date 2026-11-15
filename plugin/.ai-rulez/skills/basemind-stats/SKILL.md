---
type: Playbook
title: Basemind Stats
description: 'Show a quick dashboard of basemind activity in this session: how many
  code-map tool calls have run, the per-tool histogram, and the estimated tokens saved
  vs a hypothetical grep+Read baseline. Use when the user asks "what has basemind
  done?", "how much is basemind helping?", "show me basemind stats", or invokes `/bm-stats`
  directly.'
x-ai-rulez:
  kind: skill
  id: basemind-stats
  metadata:
    name: basemind-stats
    license: MIT
    metadata:
      author: Goldziher
      repository: https://github.com/Goldziher/basemind
---

# basemind-stats — on-demand usage dashboard

Run `basemind admin telemetry --json` and render the result as a markdown report. The usage summary
is a CLI command, not an MCP tool; it reads the same telemetry log the server writes, with or
without a running server. Launch it through the plugin's version-matched binary,
`${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh admin telemetry --json` (bare `basemind` from `PATH`
when no plugin launcher exists).

## When to use

The user asks "what has basemind done?", "how much is basemind helping?", "show me basemind
stats", or invokes `/bm-stats` directly. This skill is strictly user-invoked — see Notes.

## How to run

1. Run `basemind admin telemetry --window today --json` (`today` is the default). If the
   user asks for a specific range, map it to one of `today`, `1h`, `24h`, `all`.
2. Render a markdown block in this shape:

   ```text
   ## basemind activity (today)
   - **N tool calls** ; top tools: code_outline (18), code_symbols (12), …
   - **~K tokens saved** vs grep + Read baseline
   - recent: code_outline (4ms, 312B), code_symbols (2ms, 180B), …
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
- Rows are keyed by tool name (`code_grep`, `git_log`). A log written by an older version also
  holds `code:outline`-style keys; report them as they appear.
- Don't claim a token-savings number without the disclosure sentence.
