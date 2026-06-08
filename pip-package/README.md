# basemind

**Give your AI coding agent a brain for your repo.**

basemind is a code-map MCP server: it indexes your codebase into a queryable map
so AI coding agents — Claude Code, Cursor, Continue, anything that speaks
[MCP](https://modelcontextprotocol.io) — get instant semantic answers about your
code. Where is this defined? Who calls it? When did it change? What's churning?

Sub-millisecond queries. 300+ languages out of the box. Local-only. Built in Rust.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)
[![PyPI](https://img.shields.io/pypi/v/basemind.svg)](https://pypi.org/project/basemind/)

## Install

```bash
pip install basemind
```

On first invocation, the pre-compiled Rust binary for your platform (macOS,
Linux, Windows; x86_64 + arm64) is downloaded from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) and cached
under `~/.cache/basemind/<version>/`.

Override the binary location with `BASEMIND_BINARY=/path/to/basemind`.

## Quickstart

```bash
cd /path/to/your/repo
basemind scan        # index the working tree
basemind serve       # run the MCP stdio server
```

Wire `basemind serve` into Claude Code (`~/.claude.json`) or any MCP client:

```json
{
  "mcpServers": {
    "basemind": {
      "command": "basemind",
      "args": ["serve"],
      "cwd": "/abs/path/to/your/repo"
    }
  }
}
```

## Documentation

Full docs, architecture, and the complete MCP tool table at
[github.com/Goldziher/basemind](https://github.com/Goldziher/basemind).

## License

[MIT](https://github.com/Goldziher/basemind/blob/main/LICENSE).
