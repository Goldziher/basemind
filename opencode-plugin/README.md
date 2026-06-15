# basemind-opencode

OpenCode plugin for [basemind](https://github.com/Goldziher/basemind) — a
tree-sitter code-map + git context MCP server.

## Install

Add to your `opencode.json` (global or project-level):

```json
{
  "plugin": ["basemind-opencode@latest"]
}
```

Restart OpenCode. The plugin registers the basemind MCP server and the bundled
skills directory.

You also need the `basemind` binary on your `PATH`:

```bash
npm install -g basemind        # or: pip install basemind / cargo install basemind
```

Then scan your repo once before starting OpenCode:

```bash
cd /path/to/your/repo
basemind scan
```

## What this registers

- **MCP server** named `basemind` running `basemind serve` over stdio. Exposes
  the full code-map and git toolset — `outline`, `search_symbols`,
  `find_references`, `find_callers`, `recent_changes`, `blame_symbol`, etc.
- **Skills directory** with two pre-authored skills (`basemind`,
  `basemind-stats`) that document how to drive the MCP toolset.

See the [root README](https://github.com/Goldziher/basemind#readme) for the
full MCP tool table, architecture notes, and per-language coverage.

## License

MIT
