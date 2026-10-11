# basemind-opencode

OpenCode and Pi package for [basemind](https://github.com/Goldziher/basemind) — full AI context layer for coding agents.

## Install

Add to your `opencode.json` (global or project-level):

```json
{
  "plugins": ["basemind-opencode@latest"]
}
```

Restart OpenCode. You also need the `basemind` binary on your `PATH`:

```bash
pnpm add -g basemind        # or: pip install basemind / cargo install basemind
```

Then scan your repo once before starting OpenCode:

```bash
cd /path/to/your/repo
basemind scan
```

## Pi

The next release adds Pi resources to this existing npm package. Install the
`basemind` binary first, then run `pi install npm:basemind-opencode` after that
release. Git installation is also available:

```sh
pi install git:github.com/Goldziher/basemind
```

Pi loads ten skills and a native extension that injects basemind context at
session start and after compaction. It uses the CLI through its shell tool;
no MCP bridge or binary is bundled. OpenCode continues using the existing
JavaScript and terminal adapters below.

## Package maintenance

`scripts/sync-plugin-skills.sh` mirrors the canonical `.pi/extensions/basemind.ts`
and product skills into this package. `task package:plugin` stages the combined
package from canonical sources and runs a real pnpm pack without changing the
checkout. The existing `publish_opencode` job checks the ten skills, native Pi
extension, and both OpenCode adapters before publishing the packed archive.
The existing package name, version bump, and npm trusted publisher are shared;
Pi does not require another registry package.

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
