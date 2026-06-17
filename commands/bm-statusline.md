---
name: bm-statusline
description: Enable the basemind status line in your Claude Code user settings (one-time setup).
---

# bm-statusline — enable the basemind status line

Wire the basemind status line into the user's global Claude Code settings, then
confirm it renders. Use your tools to do this directly — do not ask the user to
hand-edit any files.

1. **Locate the installed plugin's `statusline.sh`.** Check these in order and
   use the first that exists, resolved to an **absolute** path:
   - `${CLAUDE_PLUGIN_ROOT}/.claude-plugin/statusline.sh` (if that var is set)
   - the newest match of
     `~/.claude/plugins/cache/basemind/basemind/*/.claude-plugin/statusline.sh`

   If none exists, tell the user the basemind plugin isn't installed and stop.

2. **Update `~/.claude/settings.json`** (treat a missing file as `{}`). Set its
   `statusLine` key to:

   ```json
   { "type": "command", "command": "<absolute path>", "refreshInterval": 5 }
   ```

   Preserve every other key. Use an **absolute path** — `$HOME`/`~` are not
   expanded in this field. Verify the file is still valid JSON afterward.

3. **Confirm it renders** by running the script once with a sample payload:

   ```bash
   printf '{"workspace":{"current_dir":"%s"}}' "$PWD" | bash "<absolute path>"
   ```

4. Tell the user it's enabled, and that any other running sessions need a
   relaunch to pick it up.
