# basemind

[![npm version](https://badge.fury.io/js/basemind.svg)](https://www.npmjs.com/package/basemind)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)

Code-map MCP server + scanner — content-addressed, Fjall-backed inverted index over
tree-sitter outlines.

## Install

```bash
npm install -g basemind
```

The installer downloads the appropriate pre-compiled Rust binary for your platform
(macOS, Linux, Windows; x86_64 + arm64) from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) on first install.

## Use

```bash
basemind scan        # index the current repo into .basemind/
basemind serve       # run the MCP stdio server
basemind lang list   # show downloaded tree-sitter grammars
```

Wire `basemind serve` into an MCP client (Claude Desktop, Cursor, etc.) per their
config — basemind exposes the full code-map and git tool surface over stdio.

## Documentation

Full docs at [github.com/Goldziher/basemind](https://github.com/Goldziher/basemind).

## License

MIT.
