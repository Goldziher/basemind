# basemind

The context and communication layer for coding agents — a shared code-map, document RAG, memory,
git history, and agent-to-agent comms so multiple agents coordinate while they work.
300+ languages, one MCP server.

<!-- markdownlint-disable-next-line MD013 -->
[![Docs](https://img.shields.io/badge/docs-basemind.ai-965aff)](https://basemind.ai)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)
[![PyPI](https://img.shields.io/pypi/v/basemind.svg)](https://pypi.org/project/basemind/)

Full documentation: **[basemind.ai](https://basemind.ai)**

## Install

```bash
pip install basemind
```

On first invocation, the pre-compiled Rust binary for your platform (macOS on Apple
Silicon, Linux on x86_64 and arm64, Windows on x86_64) is downloaded from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) and cached under
`$XDG_CACHE_HOME/basemind/<version>/` (`~/.cache/basemind/<version>/` when `XDG_CACHE_HOME` is
unset). [Data and uninstall](https://basemind.ai/start/uninstall/) lists what to remove.

Override the binary location with `BASEMIND_BIN=/path/to/basemind`; `BASEMIND_BINARY` is read when it is unset.

## Quickstart

```bash
cd /path/to/your/repo
basemind scan        # index the working tree
```

Then register basemind with your MCP client, which starts `basemind serve` itself (run in a
terminal, `basemind serve` only waits on stdio):

```json
{
  "mcpServers": {
    "basemind": { "command": "basemind", "args": ["serve"] }
  }
}
```

`basemind serve` serves the project of the directory the client starts it in. A client that starts
it elsewhere needs the project named: `"args": ["serve", "--root", "/path/to/your/repo"]`.

The [MCP server guide](https://github.com/Goldziher/basemind#2-as-an-mcp-server) has the command or
file for each client, and [Get started](https://github.com/Goldziher/basemind#get-started) covers
`basemind init`.

## Full documentation

See the [main README](https://github.com/Goldziher/basemind#readme) for complete docs,
architecture, MCP tool reference, and per-harness setup instructions.

## License

[MIT](https://github.com/Goldziher/basemind/blob/main/LICENSE).
