# MCP-session video — capture checklist

The CLI GIF (`demo.gif`) shows the command-line surface, but basemind's real
value is the **MCP tools an agent calls** — and `basemind serve` is a stdio
JSON-RPC server with nothing to _see_. This short screen recording captures the
agent experience the GIF can't. It is a manual capture (a live Claude Code
session can't be scripted).

## What to show (~20–25s)

1. Open a fresh Claude Code session in a real repository with the basemind
   plugin connected (`/plugin marketplace add Goldziher/basemind`, then install).
2. Ask something that triggers a code-map tool, e.g. _"where is X defined and
   what calls it?"_ — show `search_symbols` / `find_references` / `outline`
   returning **paths, line numbers, and signatures, not file bodies**. The point
   is structure at a fraction of the tokens of grep + Read.
3. Run `/bm-stats` to show the per-tool histogram and estimated tokens saved.

Keep it tight: one clear question → tool calls → the savings dashboard.

## Record + embed

1. Screen-record to MP4 (macOS: `⇧⌘5`, "Record Selected Portion"). Trim to
   ≤25s; crop to the terminal/session pane.
2. Upload the MP4 as a comment attachment on any GitHub issue or PR in the repo.
   GitHub rehosts it under a `user-attachments` URL — copy that URL. Do **not**
   commit the MP4 to the repo.
3. In the top-level `README.md`, replace the video placeholder with the embed:

   ```html
   <video src="https://github.com/user-attachments/assets/REPLACE-ME"
          controls width="800"></video>
   ```

   GitHub renders an inline player from a `user-attachments` video URL.
