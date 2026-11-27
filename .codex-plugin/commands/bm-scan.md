---
description: Build or refresh the basemind index by running `basemind scan` via the CLI — works without the MCP server (use it when basemind reports "no index" / "no indexed files").
argument-hint:
    - path
name: bm-scan
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:a16e055baa48cf130db518ff974179e00382d39be3e6d90d422a433c02ec21c2
Source-Hash: blake3:371884a0eb1b3befe0b8912d3df9b94c1e0dcf7bfcf52f371dfc56a35f80c02c
Schema-Version: v1
-->

# bm-scan — build or refresh the basemind index

Run `basemind scan` via the CLI so the code map exists and is current.

## When to use

basemind (or its statusline) reports "no index" / "no indexed files", or `index_status` reports
`degraded` or `rescan_needed`. A healthy index that misses a file you just created needs only
`basemind rescan <path>` for that file, not a whole-tree scan. Files under `scan.extra_roots` are the
exception: `rescan <path>` skips them, so a new or changed one needs a full scan.

## How to use

```sh
basemind scan                  # no argument
basemind rescan "$ARGUMENTS"   # a path argument
```

- No argument → `basemind scan`, a full working-tree scan.
- A path argument (`/bm-scan src/mcp`) → `basemind rescan src/mcp`, which re-indexes only that path
  (incremental) when a working-tree index exists; with none yet, or one that must be rebuilt, it
  runs a full scan instead. `scan` takes no path.
- Prefer the plugin-managed, version-matched binary: run `${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh scan`
  (or `… rescan "$ARGUMENTS"`)
  (in other hosts, `scripts/mcp-launch.sh` in the plugin directory). Use bare `basemind` from `PATH`
  only when no plugin launcher exists, or build a dev binary with `cargo build --release` and use
  `./target/release/basemind`.

## Notes

- Report files scanned / updated / skipped and elapsed time. Non-extractable files are
  **skipped**, not failures.
- If a daemon is running, `scan` is forwarded to it and reports `scan via daemon`. Another
  process holding the store lock (`basemind watch`, another scan) makes `scan` and `rescan` exit 3 and
  name it: stop the watcher, run the command, then restart `basemind watch`.

## See also

The `basemind-scan` skill for the full workflow, binary-resolution order, and `extra_roots`
config for indexing directories outside the repo.
