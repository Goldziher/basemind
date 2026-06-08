# basemind

[![PyPI version](https://img.shields.io/pypi/v/basemind.svg)](https://pypi.org/project/basemind/)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)

Code-map MCP server + scanner — content-addressed, Fjall-backed inverted index over
tree-sitter outlines.

## Install

```bash
pip install basemind
```

On first invocation, the pre-compiled Rust binary for your platform (macOS, Linux,
Windows; x86_64 + arm64) is downloaded from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) and cached under
`~/.cache/basemind/<version>/`.

## Use

```bash
basemind scan        # index the current repo into .basemind/
basemind serve       # run the MCP stdio server
basemind lang list   # show downloaded tree-sitter grammars
```

Wire `basemind serve` into an MCP client (Claude Desktop, Cursor, etc.) per their
config — basemind exposes the full code-map and git tool surface over stdio.

Override the binary location with `BASEMIND_BINARY=/path/to/basemind`.

## Documentation

Full docs at [github.com/Goldziher/basemind](https://github.com/Goldziher/basemind).

## License

MIT.
