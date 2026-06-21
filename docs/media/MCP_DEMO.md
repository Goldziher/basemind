# Agent (MCP) demo — capture checklist

The CLI GIF (`demo.gif`) shows the command-line surface, but basemind's real
value is the **MCP tools an agent calls** — and `basemind serve` is a stdio
JSON-RPC server with nothing to _see_. The `mcp-demo.gif` screen recording
captures the agent experience the CLI GIF can't. It is a manual capture (a live
Claude Code session can't be scripted).

## What to show (~20–25s)

1. Open a fresh Claude Code session in a real repository with the basemind
   plugin connected (`/plugin marketplace add Goldziher/basemind`, then install).
2. Ask something that triggers a code-map tool, e.g. _"where is X defined and
   what calls it?"_ — show `search_symbols` / `find_references` / `outline`
   returning **paths, line numbers, and signatures, not file bodies**. The point
   is structure at a fraction of the tokens of grep + Read.
3. Run `/bm-stats` to show the per-tool histogram and estimated tokens saved.

Keep it tight: one clear question → tool calls → the savings dashboard.

## Record, convert, embed

1. Screen-record (macOS: `⇧⌘5`, "Record Selected Portion") to a `.mov`. Keep it
   ≤~30s; crop to the terminal/session pane.
2. Convert to an optimized GIF with ffmpeg (two-pass palette — scaled to 1000px
   wide, 12 fps keeps it small and crisp):

   ```bash
   SRC="$HOME/Desktop/your-recording.mov"
   PAL="$(mktemp -d)/palette.png"
   ffmpeg -y -i "$SRC" -vf "fps=12,scale=1000:-1:flags=lanczos,palettegen=stats_mode=diff" "$PAL"
   ffmpeg -y -i "$SRC" -i "$PAL" \
     -lavfi "fps=12,scale=1000:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3" \
     docs/media/mcp-demo.gif
   ```

3. The top-level `README.md` already embeds `docs/media/mcp-demo.gif` in the
   Quickstart section — just commit the regenerated GIF.
