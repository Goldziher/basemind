---
name: bm-statusline
description: Enable the basemind status line in your Claude Code user settings (one-time setup).
---

# bm-statusline — enable the basemind status line

Wire the basemind status line into the user's global Claude Code settings, then
confirm it renders. Use your tools to do this directly — do not ask the user to
hand-edit any files.

1. **Confirm the plugin is installed.** Check that at least one of these exists:
   - `${CLAUDE_PLUGIN_ROOT}/.claude-plugin/statusline.sh` (if that var is set)
   - `~/.claude/plugins/marketplaces/basemind/.claude-plugin/statusline.sh`
   - a match of
     `~/.claude/plugins/cache/basemind/basemind/*/.claude-plugin/statusline.sh`

   If none exists, tell the user the basemind plugin isn't installed and stop.

2. **Update `~/.claude/settings.json`** (treat a missing file as `{}`). Set its
   `statusLine` key to this **version-independent resolver** — do NOT hardcode a
   version-pinned path, which breaks on the next basemind update when the old
   version dir is pruned:

   ```json
   { "type": "command",
     "command": "bash -c 's=$(ls -d \"$HOME\"/.claude/plugins/cache/basemind/basemind/*/.claude-plugin/statusline.sh 2>/dev/null | sort -V | tail -1); [ -f \"$s\" ] || s=\"$HOME/.claude/plugins/marketplaces/basemind/.claude-plugin/statusline.sh\"; [ -f \"$s\" ] && exec bash \"$s\" || printf \"%s\" \"◆ basemind: run /bm-statusline\"'",
     "refreshInterval": 5 }
   ```

   How it resolves, in order: the **highest-versioned** cached copy (`sort -V`, not
   mtime — an older dir touched more recently must never win; this also makes the bar
   track the newest version the moment `/plugin update` installs it), else the
   marketplace clone, else a one-line hint so the bar is never blank. It re-resolves
   at every render, so version bumps never break it and script improvements land
   automatically. Copy the `command` string
   **verbatim** (it self-resolves — `$HOME` is expanded by the `bash -c` it runs
   under, not the settings field). Preserve every other key and verify the file is
   still valid JSON afterward.

3. **Confirm it renders** by running the resolver once:

   ```bash
   printf '{"workspace":{"current_dir":"%s"}}' "$PWD" | bash -c 's=$(ls -d "$HOME"/.claude/plugins/cache/basemind/basemind/*/.claude-plugin/statusline.sh 2>/dev/null | sort -V | tail -1); [ -f "$s" ] || s="$HOME/.claude/plugins/marketplaces/basemind/.claude-plugin/statusline.sh"; [ -f "$s" ] && exec bash "$s" || printf "%s" "◆ basemind: run /bm-statusline"'
   ```

4. Tell the user it's enabled, and that any other running sessions need a
   relaunch to pick it up.
