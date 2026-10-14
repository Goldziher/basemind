# basemind plugin

Basemind provides code intelligence, git history, document retrieval, shared memory, and agent coordination through MCP.

## Install

Clone the repository, then use the generated `plugin/` directory for local installs:

```sh
git clone https://github.com/Goldziher/basemind
cd basemind
```

| Runtime | Installation or package entry point |
|---|---|
| Claude Code | In-session: `/plugin marketplace add Goldziher/basemind`, then `/plugin install basemind@basemind` |
| Codex | `codex plugin marketplace add Goldziher/basemind`, then `codex plugin add basemind@basemind` |
| Cursor | Import the repository in Team Marketplaces; the marketplace points to `plugin/` |
| Gemini CLI | `gemini extensions install ./plugin --skip-settings` |
| Factory Droid | `droid plugin marketplace add https://github.com/Goldziher/basemind`, then `droid plugin install basemind@basemind` |
| Kimi | Load the generated `plugin/` package with its plugin installer |
| OpenCode | Add the absolute path to `plugin/.opencode/plugins/basemind.js` to the `plugin` array in `opencode.json` |
| Hermes | Install the generated wheel below and enable `basemind` |
| Pi | `pi install ./plugin` |
| Agent Plugins | Load `plugin/plugin.json` and its sibling resources |
| GitHub Copilot CLI | `copilot plugin marketplace add Goldziher/basemind`, then `copilot plugin install basemind@basemind`; uses the shared Agent Plugins layout |

The MCP launcher downloads the matching published basemind binary. Set `BASEMIND_BIN` to select a
local build for launcher-based integrations. Hermes and Pi require `basemind` on `PATH`.

### Hermes

Use Python 3.11 or newer in the environment that runs Hermes:

```sh
uv build --wheel --out-dir dist/hermes plugin/.hermes/package
python -m pip install --force-reinstall dist/hermes/basemind_hermes_plugin-0.29.3-py3-none-any.whl
hermes plugins enable basemind
```

Restart Hermes after installation. The plugin registers ten skills, six slash commands, scoped
session and inbox notifications, and `basemind_call`. The bridge calls the local MCP server's nine
domains and returns JSON text. Optional direct MCP configuration in `~/.hermes/config.yaml` is:

```yaml
mcp_servers:
  basemind:
    command: basemind
    args: [serve]
```

Published PyPI version 0.29.3 predates the current skill-registration fix and MCP bridge. Install
the source wheel until a newer release includes them. This wheel includes MCP and PyYAML
dependencies, but no basemind binary.

Missing or invalid required skill resources and required registration errors disable the plugin
with a named Hermes error. Reinstall a complete wheel before restarting Hermes; optional
notification failures remain best effort. Maintainers can run `task plugin:hermes:native:test`
with `HERMES_PYTHON` pointing to the Hermes SDK interpreter.

### Pi and OpenCode packages

The curated package supplies ten Pi skills and six prompts. It contains no Pi context extension.
The existing root Git package preserves the custom Pi extension, and `basemind-opencode` preserves
its existing OpenCode and terminal adapters. See the [root installation guide](../README.md#installation).

The generated npm identity is `@goldziher/basemind-plugin`. It is not yet published; use the local
OpenCode adapter and Pi directory installation above. Do not substitute that name for the existing
published `basemind-opencode` package until the new package is released.

## Quick start

Install the [basemind binary](../README.md#install-the-program), then index a repository:

```sh
cd /path/to/your/repository
basemind scan
basemind status
```

If scanning fails, fix the reported configuration or filesystem error before starting the agent.
Load the `basemind` skill in your agent and use `code` with an explicit `mode`, such as `outline`.

## API overview

Nine domain tools dispatch through a required `mode`: `code`, `graph`, `git`, `memory`, `admin`,
`web`, `agents`, `workspace`, and `shell`. Optional capabilities depend on the installed binary's
features. See the [MCP reference](../README.md) for modes and error handling.

Install only the public skills with `npx skills add Goldziher/basemind`. Root `skills/` mirrors the
ten product skills, including `basemind-multi-agent-room`; maintainer rules and private agents are
excluded. Claude-style slash commands become prompts or commands where the runtime supports them.

## Configuration

Product sources live in `plugin/.ai-rulez/`; root `.ai-rulez/` governs development. Runtime manifests,
packages, marketplace indexes, and skill mirrors are generated from these sources. Gemini context
lives at `plugin/assets/GEMINI.md`.

## Contributing and publishing

Regenerate and verify through the repository plugin tasks before submitting changes. Do not edit
generated runtime files directly. Publishing must verify the packed npm archive, Hermes wheel,
and all-runtime archive before checking whether a version already exists.

The manual artifact workflow packages the selected checkout only when its core and plugin
versions match the requested existing release tag. Marketplace indexes pin that checkout's
immutable commit; the archive attaches to the requested release. It does not create a product
tag or publish registry packages.

The new npm package requires its first manual bootstrap and a trusted-publisher configuration;
Hermes publishing requires its PyPI trusted publisher. Configure those account settings before
requesting a release. Existing version 0.29.3 registry packages do not contain these unreleased
changes. See the [root README](../README.md).

## License

MIT.
