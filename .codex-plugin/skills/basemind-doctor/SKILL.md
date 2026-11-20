---
description: Diagnose and recover basemind when it isn't working — MCP tools missing or erroring, "no index" / "no indexed files", empty results that shouldn't be empty, or the MCP server seems dead. Runs CLI checks (no MCP server required) and gives the client-specific way to reconnect the server.
name: basemind-doctor
license: MIT
metadata:
    author: Goldziher
    repository: https://github.com/Goldziher/basemind
---

<!--
AI-RULEZ :: GENERATED FILE — DO NOT EDIT
Content-Hash: blake3:946805ae34bdf23eee93abb56f5bf5347cee30fb95236969e76d0f5fff5755f7
Source-Hash: blake3:2950a35215b5068ba96984091a9b83afe368eb1dc0a85f06bb09a6ef9e61ba66
Schema-Version: v1
-->

# basemind-doctor — diagnose and recover basemind

Use this when basemind isn't behaving: MCP tools aren't available or return errors, the
statusline says **no index**, queries come back empty when they shouldn't, or the `basemind serve`
MCP server appears dead. Every step here uses the **CLI**, so it works even when the MCP server is
down.

Important: a stdio MCP server (what `basemind serve` is) **cannot be restarted by an agent or by
basemind itself** — a fresh process can't resume the client's MCP `initialize` handshake.
Reconnecting the server is the **MCP client's** job (see step 4). What you _can_ do from here is
make sure the index is healthy and clear anything blocking a restart.

## 1. Is there an index?

Run the `basemind …` commands below through the plugin-managed, version-matched binary,
`${CLAUDE_PLUGIN_ROOT}/scripts/mcp-launch.sh <args>` (in other hosts, `scripts/mcp-launch.sh` in the
plugin directory); use bare `basemind` from `PATH` only when no plugin launcher exists.

```sh
basemind doctor   # root, config, index, grammars, hook, daemon; exits 1 when a check fails
basemind status
```

- Errors / "no index" / `file_count: 0` with blobs present → the index is missing or lost. Build
  it: `basemind scan` (see the `basemind-scan` skill / `/bm-scan`).
- Healthy `file_count` → the index is fine; the problem is the server connection (step 4).
- `code_outline` says a README, JSON, YAML or TOML file "is a document, not code" → expected, not a
  fault. Find exact text in it with `code_grep {"pattern":"…"}`, a topic with
  `docs_search {"query":"…"}`.
- A tool result with `isError` names its own fix: follow it (`index_rescan`, `find_files`, the
  missing Cargo feature) before diagnosing further.
- A `notice` of `warming_up` / `building_index` means the index is still loading: wait and retry
  (`index_status {}` reports the scan state, plus `cache_dir`, this workspace's cache directory,
  and `grammar_dir`, the tree-sitter grammar cache).

## 2. Is a server already running (holding the lock)?

With a basemind daemon running, `basemind scan` is forwarded to it (`scan via daemon`), so the
daemon never makes it fail. If `basemind scan` fails with a lock error (exit 3), another process
(`basemind watch`, another scan) holds the index for this repo. The error names the live holder
(command + pid) — it reads
that from the `.lock.meta` sidecar next to the workspace `.lock`. Both live in this workspace's
directory under the machine-global cache (Linux `~/.local/share/basemind/`, macOS
`~/Library/Application Support/basemind/`; override `BASEMIND_DATA_HOME`), keyed by workspace — not
in the repo.

- If that `pid` is **alive** (`ps -p <pid>`), let it finish, or stop it; meanwhile the MCP tools
  answer, and `index_rescan {}` or `basemind rescan` refreshes the index.
- If that `pid` is **dead**, the lock is stale. The OS releases the advisory lock when a process
  dies, so a fresh `basemind scan` / `basemind serve` should just work — retry it. (You may delete
  the stale `.lock.meta` sidecar in that workspace cache dir to clear the advisory holder record.)

## 3. Rebuild the index if needed

```sh
basemind scan
```

Non-extractable files are skipped, not failed. After this, both the CLI and (once reconnected) the
MCP tools have a fresh index.

## 4. Reconnect the MCP server (client-specific — this is the only way to restart it)

basemind can't relaunch its own stdio server; trigger a reconnect in your client:

- **Claude Code**: reconnect the basemind MCP server from the MCP UI, or restart the session. The
  plugin's launcher re-downloads/execs the binary automatically on the next connection.
- **Cursor / others**: toggle/reconnect the basemind MCP server in the MCP settings.

While disconnected, you are not blocked: use the `basemind-cli` skill (`basemind code …`,
`basemind git …`) — it reads the same machine-global cache directly, no server required.

## When server logs help

`basemind serve` logs its lifecycle to stderr (captured in your client's MCP server logs):
a `MCP server starting` line with pid/version/view at startup, and an explicit
`client disconnected, exiting` (clean) or `exiting on error` (with the cause) at shutdown. If serve
keeps dying, that log line names the reason.
