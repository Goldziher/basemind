# basemind-hermes-plugin

The Hermes plugin for [basemind](https://github.com/Goldziher/basemind) adds public skills, slash commands, agent notifications, and a local MCP bridge.

The authoritative package is generated at `plugin/.hermes/package` from
`plugin/.ai-rulez/hermes/index.py`. This directory retains package tests and compatibility tooling.
See the [plugin installation guide](../plugin/README.md#hermes) for installation from source.

Published PyPI version 0.29.3 predates the current skill-registration fix and MCP bridge. Install
the generated local wheel until a newer version includes these changes. Use Python 3.11 or newer
in Hermes's own environment, enable `basemind`, and restart Hermes. Install the basemind binary
separately and keep it on `PATH`.

The generated wheel depends on MCP and PyYAML. Registration itself uses only the standard library;
the MCP SDK is imported when the bridge is called. Skills include `basemind-multi-agent-room`.
The bridge returns JSON text; session and inbox notifications are best-effort.

## License

MIT.
