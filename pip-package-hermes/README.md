# basemind-hermes-plugin

The [Hermes Agent](https://github.com/NousResearch/hermes) plugin for
[basemind](https://github.com/Goldziher/basemind). It adds what MCP config alone cannot: basemind's
helper **skills**, **slash commands**, and agent-comms **notifications** (session-start context +
per-turn inbox deltas).

This package is pure-Python and stdlib-only. It does **not** install the `basemind` binary — install
that separately (see below). The plugin reaches basemind by shelling out to `basemind` on your
`PATH`, and every hook is fail-open: with no binary or a down comms broker it degrades to a no-op.

## Prerequisites

1. **The `basemind` binary on your `PATH`** — via any channel:

   | Channel | Command |
   |---|---|
   | Homebrew | `brew install Goldziher/tap/basemind` |
   | npm | `npm install -g basemind` |
   | cargo | `cargo install basemind --features full --locked` |
   | GitHub releases | [download a binary](https://github.com/Goldziher/basemind/releases) |

2. **The basemind MCP server wired into Hermes** — this is what gives Hermes the 60+ tools. Add to
   `~/.hermes/config.yaml`:

   ```yaml
   mcp_servers:
     basemind:
       command: basemind
       args: [serve]
   ```

## Install the plugin

Install into the **same Python environment Hermes runs in** (Hermes discovers plugins through the
`hermes_agent.plugins` entry point):

```bash
pip install basemind-hermes-plugin
```

Then enable it (general plugins are opt-in):

```bash
hermes plugins enable basemind
```

Restart your Hermes session so it re-reads the config and loads the plugin.

## What it registers

- **Skills** — `basemind`, `basemind-code-search`, `basemind-git-history`, `basemind-documents`,
  `basemind-comms`, `basemind-cli`, `basemind-doctor`, `basemind-scan`, `basemind-stats`.
- **Slash commands** — `bm`, `bm-init`, `bm-scan`, `bm-doctor`, `bm-stats`, `bm-statusline`.
- **Hooks** — `on_session_start` (operating discipline + condensed comms inbox) and `pre_llm_call`
  (per-turn agent-comms deltas). Best-effort; the MCP tools work regardless.

## License

MIT
