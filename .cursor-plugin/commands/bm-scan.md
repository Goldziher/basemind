---
name: bm-scan
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
---

# bm-scan — build or refresh the basemind index

Run a `basemind scan` so the code map exists and is current. This uses the basemind **CLI**, so
it works even when the MCP server isn't running — it's the right move when basemind reports
"no index" / "no indexed files", or after large changes when the index is stale.

Optional argument: a path to scan (defaults to the whole repo): `$ARGUMENTS`

Steps:

1. Run the scan from the repository root:

   ```sh
   basemind scan ${ARGUMENTS:-}
   ```

   - No argument → full working-tree scan.
   - A path argument → scope the scan to that path (incremental).
   - If `basemind` isn't on `PATH`, it ships with the plugin; the MCP launcher caches it under
     `${XDG_CACHE_HOME:-~/.cache}/basemind/bin/<version>/basemind` — use that path, or build a dev
     binary with `cargo build --release` and use `./target/release/basemind`.

2. Report the result: files scanned / updated / skipped and elapsed time. Non-extractable files
   (e.g. a source file in a language tree-sitter doesn't map) are **skipped**, not failures.

3. If a `basemind serve` MCP server is already running for this repo, it holds the store lock and
   you'll get a lock error — that's expected. In that case use the `rescan` MCP tool instead (it
   re-indexes in-process), or stop the server first.

After a successful scan the MCP tools (`outline`, `search_symbols`, `find_references`, …) and the
CLI (`basemind query …`) both have a fresh index to work from.
