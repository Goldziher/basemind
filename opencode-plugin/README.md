# basemind-opencode

OpenCode and Pi package for [basemind](https://github.com/Goldziher/basemind) — full AI context layer for coding agents.

## Install

Both runtimes need the `basemind` program on your `PATH`; see
[Install the program](https://github.com/Goldziher/basemind#install-the-program):

```bash
pnpm add -g basemind        # or: brew install Goldziher/tap/basemind / pip install basemind
```

A plain `cargo install basemind` lacks `basemind serve`; build with
`cargo install basemind --features full --locked` instead.

### OpenCode

Add to `opencode.json` (project) or `~/.config/opencode/opencode.json` (global):

```json
{ "plugins": ["basemind-opencode@latest"] }
```

Restart OpenCode, then scan your repo once:

```bash
cd /path/to/your/repo
basemind scan
```

### Pi

```sh
pi install npm:basemind-opencode
```

`pi install git:github.com/Goldziher/basemind` installs the same resources from the repository.
Add `-l` to save the package in the current project's configuration. Pi loads ten skills and a
native extension that injects basemind context at session start and after compaction. It uses the
CLI through its shell tool; no MCP bridge or binary is bundled.

## What OpenCode registers

- **MCP server** named `basemind` running `basemind serve` over stdio. Exposes the full
  code-map, git, documents, and memory toolset.
- **Skills** shipped with the package, registered into the skill catalog.
- **Agent-comms notifications** (terminal plugin): new messages in the basemind agent-comms
  inbox are surfaced as a toast when a session starts and after tool calls.

See the [main README](https://github.com/Goldziher/basemind#readme) for the full MCP tool
reference, architecture, and configuration.

## License

MIT
