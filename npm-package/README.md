# gitmind

[![npm version](https://badge.fury.io/js/gitmind.svg)](https://www.npmjs.com/package/gitmind)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/gitmind/blob/main/LICENSE)

Code-map MCP server + scanner — content-addressed, Fjall-backed inverted index over
tree-sitter outlines.

## Install

```bash
npm install -g gitmind
```

The installer downloads the appropriate pre-compiled Rust binary for your platform
(macOS, Linux, Windows; x86_64 + arm64) from
[GitHub Releases](https://github.com/Goldziher/gitmind/releases) on first install.

## Use

```bash
gitmind scan        # index the current repo into .gitmind/
gitmind serve       # run the MCP stdio server
gitmind lang list   # show downloaded tree-sitter grammars
```

Wire `gitmind serve` into an MCP client (Claude Desktop, Cursor, etc.) per their
config — gitmind exposes the full code-map and git tool surface over stdio.

## Documentation

Full docs at [github.com/Goldziher/gitmind](https://github.com/Goldziher/gitmind).

## License

MIT.
