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

### Platforms

The plugin runs on macOS (Apple silicon), Linux (x86-64 and arm64, glibc) and Windows (x86-64).
Every manifest starts the server through `scripts/mcp-launch.cmd`, which runs the bash launcher
`scripts/mcp-launch.sh`. macOS and Linux need `bash`. Windows needs bash from
[Git for Windows](https://git-scm.com/download/win): the shim looks for it at
`CLAUDE_CODE_GIT_BASH_PATH`, then in the Program Files and per-user Git installs, then on `PATH`, and
never uses the WSL `bash`. Without it the server exits with a message naming Git for Windows.

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
tools and returns JSON text, over one `basemind serve` session kept open per working directory
(a failed or timed-out one is replaced on the next call). Each turn's inbox read runs only while
a comms daemon accepts connections and gives up after 2 seconds, so an idle turn spawns no
process. At session start the plugin runs `basemind --version` once and, for a missing binary or
one from another minor version, injects a notice naming the install command. Optional direct MCP configuration in `~/.hermes/config.yaml` is:

```yaml
mcp_servers:
  basemind:
    command: basemind
    args: [serve]
```

The wheel includes MCP and PyYAML dependencies, but no basemind binary.

Missing or invalid required skill resources and required registration errors disable the plugin
with a named Hermes error. Reinstall a complete wheel before restarting Hermes; optional
notification failures remain best effort.

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

## Contributing

Plugin sources, generation and publishing are described in
[CONTRIBUTING.md](../CONTRIBUTING.md#plugin-and-packages).

## License

MIT.
