# basemind-opencode

OpenCode plugin for [basemind](https://github.com/Goldziher/basemind) — full AI context layer for coding agents.

## Install

Add to your `opencode.json` (global or project-level):

```json
{
  "plugins": ["basemind-opencode@latest"]
}
```

Restart OpenCode. You also need the `basemind` binary on your `PATH`:

```bash
npm install -g basemind        # or: pip install basemind / cargo install basemind
```

Then scan your repo once before starting OpenCode:

```bash
cd /path/to/your/repo
basemind scan
```

## What this registers

- **MCP server** named `basemind` running `basemind serve` over stdio. Exposes the full
  code-map, git, documents, and memory toolset.
- **Skills** shipped with the package, registered into the skill catalog.
- **Agent-comms notifications** (terminal plugin): new messages in the basemind agent-comms
  inbox are surfaced as a toast when a session starts and after tool calls.

See the [main README](https://github.com/Goldziher/basemind#readme) for the full MCP tool
reference, architecture, and configuration.

## License

MIT
