# Agent (MCP) demo — capture checklist

The CLI GIF (`demo.gif`) shows the command-line surface, but basemind's real
value is the **MCP tools an agent calls** — and `basemind serve` is a stdio
JSON-RPC server with nothing to _see_. The `mcp-demo.gif` screen recording
captures the agent experience the CLI GIF can't. It is a manual capture (a live
Claude Code session can't be scripted).

## What to show (~20–25s)

1. Use the release being documented. The plugin's MCP launcher runs the binary
   `BASEMIND_BIN` names, or else downloads the one matching the plugin's version
   (`plugin/scripts/mcp-launch.sh`), so the `basemind` on `PATH` may not be the
   one it serves. Pin the release's binary with `BASEMIND_BIN` in the shell that
   starts the session, and check that executable with `"$BASEMIND_BIN" --version`.
2. Open a fresh Claude Code session in a **public** repository with the basemind
   plugin installed (`/plugin marketplace add Goldziher/basemind`, then
   `/plugin install basemind@basemind`). Run `/bm-statusline` once so the
   statusline shows the tokens saved.
3. Ask something that a file outline and a call-site lookup answer, e.g.
   _"where is X defined and what calls it?"_ — show `code_outline`,
   `code_symbols` and `code_refs` returning **paths, line numbers and
   signatures, not file bodies**. The point is structure at a fraction of the
   tokens of grep + Read.
4. Run `/bm-stats` to show the per-tool histogram and estimated tokens saved.

Keep it tight: one clear question → tool calls → the savings dashboard. Every
tool on screen must be one that binary serves (`VERB_TOOLS`,
`src/mcp/types_tools.rs`); no other repository, path or account may appear in
the frame.

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

3. The top-level `README.md` embeds `docs/media/mcp-demo.gif` under its
   intro. Commit the regenerated GIF and drop any caption that says it was
   recorded on an earlier release.
