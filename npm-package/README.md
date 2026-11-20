# basemind

The context and communication layer for coding agents — a shared code-map, document RAG, memory,
git history, and agent-to-agent comms so multiple agents coordinate while they work.
300+ languages, one MCP server.

<!-- markdownlint-disable-next-line MD013 -->
[![Docs](https://img.shields.io/badge/docs-basemind.ai-965aff)](https://basemind.ai)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/Goldziher/basemind/blob/main/LICENSE)
[![npm](https://img.shields.io/npm/v/basemind.svg)](https://www.npmjs.com/package/basemind)

Full documentation: **[basemind.ai](https://basemind.ai)**

## Install

```bash
pnpm add -g basemind
```

The installer downloads the appropriate pre-compiled Rust binary for your platform (macOS on
Apple Silicon, Linux on x86_64 and arm64, Windows on x86_64) from
[GitHub Releases](https://github.com/Goldziher/basemind/releases) on first install.

### Behind a proxy

The download goes through the proxy named by `HTTPS_PROXY` (or `HTTP_PROXY` when `HTTPS_PROXY` is
unset; the lowercase names take precedence), over an HTTP `CONNECT` tunnel, so TLS still runs end to
end with GitHub. The proxy URL must be `http://`; `http://user:password@proxy:3128` sends the
credentials as `Proxy-Authorization`, and a value without a scheme (`proxy:3128`) is read as
`http://`. `NO_PROXY` lists the hosts to reach directly, separated by commas or spaces: `*` matches
every host, `example.com`, `.example.com` and `*.example.com` match the host and its subdomains, and
`example.com:8443` matches that port only.

```bash
HTTPS_PROXY=http://proxy.internal:3128 NO_PROXY=localhost,.internal pnpm add -g basemind
```

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
