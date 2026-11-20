# basemind plugin

The context and communication layer for coding agents. Coding agents ask a local index of your code, git history, documents and memory over MCP or the CLI, get paths and lines instead of reading files, and coordinate in shared threads.

## Install

The [root installation guide](../README.md#installation) covers every runtime in detail. Gemini CLI
and the curated Pi package install from a checkout:

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
| Kimi Code | `/plugins install https://github.com/Goldziher/basemind` |
| OpenCode | Add `"basemind-opencode@latest"` to the `plugins` array in `opencode.json` |
| Hermes | Install `basemind-hermes-plugin` at the binary's version (below) and enable `basemind` |
| Pi | `pi install npm:basemind-opencode` or `pi install git:github.com/Goldziher/basemind`; `pi install ./plugin` for the curated skills and prompts only |
| Agent Plugins | Load `plugin/plugin.json` and its sibling resources |
| GitHub Copilot CLI | `copilot plugin marketplace add Goldziher/basemind`, then `copilot plugin install basemind@basemind`; uses the shared Agent Plugins layout |

The MCP launcher downloads the matching published basemind binary. Set `BASEMIND_BIN` to select a
local build for launcher-based integrations. Hermes, OpenCode and Pi require `basemind` on `PATH`.

### Hermes

Use Python 3.11 or newer in the environment that runs Hermes, and install the plugin at the version
of your `basemind` binary, so its skills name the tools that binary serves:

```sh
python -m pip install "basemind-hermes-plugin==$(basemind --version | awk '{print $2}')"
hermes plugins enable basemind
```

For a binary built from a checkout, build the wheel from the same checkout:

```sh
uv build --wheel --out-dir dist/hermes plugin/.hermes/package
python -m pip install --force-reinstall dist/hermes/basemind_hermes_plugin-*.whl
```

Restart Hermes after installation. The plugin registers ten skills, six slash commands, scoped
session and inbox notifications, and `basemind_call`. The bridge calls the local MCP server's
tools and returns JSON text. Optional direct MCP configuration in `~/.hermes/config.yaml` is:

```yaml
mcp_servers:
  basemind:
    command: basemind
    args: [serve]
```

The wheel includes MCP and PyYAML dependencies, but no basemind binary.

Missing or invalid required skill resources and required registration errors disable the plugin
with a named Hermes error. Reinstall a complete wheel before restarting Hermes; optional
notification failures remain best effort. Maintainers can run `mise run plugin:hermes:native:test`
with `HERMES_PYTHON` pointing to the Hermes SDK interpreter.

### Pi and OpenCode packages

`basemind-opencode` and the root Git package carry the OpenCode and terminal adapters, ten skills
and the Pi context extension. The curated `pi install ./plugin` package supplies the same ten skills
and six prompts, without the context extension.

## Quick start

Install the [basemind binary](../README.md#install-the-program), then index a repository:

```sh
cd /path/to/your/repository
basemind scan
basemind status
```

If scanning fails, fix the reported configuration or filesystem error before starting the agent.
Load the `basemind` skill in your agent and call a tool directly, such as
`code_outline {"path":"src/store.rs"}`.

## API overview

Each job is its own tool with an exact schema: `code_outline`, `code_symbols`, `code_read`,
`code_grep`, `code_search`, `code_refs`, `find_files`, `docs_search`, `git_log`, `git_blame`,
`git_diff`, `memory_save`, `memory_search`, `index_status` and `index_rescan`. The tools are always
loaded, an unknown argument is rejected, and line numbers are 1-based. Agent messaging is three
deferred tools, `agent_post`, `agent_inbox` and `agent_threads`, found by tool search. Optional
capabilities depend on the installed binary's features. See the [MCP tools reference](https://basemind.ai/reference/mcp-tools/) for arguments and error handling.

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

The new npm package requires its first manual bootstrap and a trusted-publisher configuration
(`npm trust github @goldziher/basemind-plugin --repo Goldziher/basemind --file publish.yaml
--allow-publish`; without it the OIDC token exchange returns 404 and the publish fails);
Hermes publishing requires its PyPI trusted publisher. Configure those account settings before
requesting a release. See the [root README](../README.md).

## License

MIT.
