---
argument-hint:
    - path
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:410d0b3e84f945fd48be696e9e27f0e8e14b380ef96d1df81fa8ec5bb4b9b8b1
Source-Hash: blake3:01442a6227c34b9cd7b138d6aac12ddc327478a9c13627fd6ee56a05f54a4960
Schema-Version: v1
-->

# bm-scan — build or refresh the basemind index

Run `basemind scan` via the CLI so the code map exists and is current.

## When to use

basemind (or its statusline) reports "no index" / "no indexed files", or `index_status` reports
`degraded` or `rescan_needed`. A healthy index that misses a file you just created needs only
`basemind rescan <path>` for that file, not a whole-tree scan.

## How to use

```sh
basemind scan ${ARGUMENTS:-}
```

- No argument → full working-tree scan.
- A path argument (`/bm-scan src/mcp`) → scope the scan to that path (incremental).
- Prefer the plugin-managed, version-matched binary: run `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh scan ${ARGUMENTS:-}`
  (in other hosts, `scripts/mcp-launch.sh` in the plugin directory). Use bare `basemind` from `PATH`
  only when no plugin launcher exists, or build a dev binary with `cargo build --release` and use
  `./target/release/basemind`.

## Notes

- Report files scanned / updated / skipped and elapsed time. Non-extractable files are
  **skipped**, not failures.
- If a daemon is running, `scan` is forwarded to it and reports `scan via daemon`. Another
  process holding the store lock (`basemind watch`, another scan) makes `scan` exit 3 and name it.

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
