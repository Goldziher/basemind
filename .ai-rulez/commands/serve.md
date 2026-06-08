---
priority: medium
usage: "/serve"
description: "Start the basemind MCP stdio server"
---

# Serve

Start the basemind MCP stdio server. Useful when manually testing tools via a client like `mcp-cli` or the rmcp REPL.

1. Build if stale: `cargo build --release`.
2. Run:

   ```bash
   ./target/release/basemind serve
   ```

3. The server reads MCP JSON-RPC over stdin and writes responses to stdout. Tool list available via `tools/list`; per-tool schemas via `tools/list` + the `schema` field.

For automated AI-tool integration, prefer the `basemind` entry in `.mcp.json` — `ai-rulez generate` writes it from the `[[mcp_servers]]` block in `.ai-rulez/config.toml`.
