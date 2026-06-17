# basemind

Full AI context layer for coding agents — code-map, document RAG, shared memory, web crawl,
git history. 300+ languages, one MCP server.

<!-- markdownlint-disable-next-line MD013 -->
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)
[![PyPI](https://img.shields.io/pypi/v/basemind.svg)](https://pypi.org/project/basemind/)

## Install

```bash
pip install basemind
```

On first invocation, the pre-compiled Rust binary for your platform (macOS, Linux, Windows;
x86_64 + arm64) is downloaded from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) and cached under
`~/.cache/basemind/<version>/`.

Override the binary location with `BASEMIND_BINARY=/path/to/basemind`.

## Quickstart

```bash
cd /path/to/your/repo
basemind scan        # index the working tree
basemind serve       # run the MCP stdio server
```

Wire `basemind serve` into Claude Code or any MCP client.

## Full documentation

See the [main README](https://github.com/Goldziher/basemind#readme) for complete docs,
architecture, MCP tool reference, and per-harness setup instructions.

## License

[MIT](https://github.com/Goldziher/basemind/blob/main/LICENSE).
