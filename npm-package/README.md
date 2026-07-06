# basemind

The context and communication layer for coding agents — a shared code-map, document RAG, memory,
web crawl, git history, and agent-to-agent comms so multiple agents coordinate while they work.
300+ languages, one MCP server.

<!-- markdownlint-disable-next-line MD013 -->
[![Docs](https://img.shields.io/badge/docs-basemind.ai-965aff)](https://basemind.ai)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)
[![npm](https://img.shields.io/npm/v/basemind.svg)](https://www.npmjs.com/package/basemind)

Full documentation: **[basemind.ai](https://basemind.ai)**

## Install

```bash
npm install -g basemind
```

The installer downloads the appropriate pre-compiled Rust binary for your platform (macOS,
Linux, Windows; x86_64 + arm64) from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) on first install.

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
