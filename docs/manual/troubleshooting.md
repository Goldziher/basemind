# Troubleshooting

Every step here uses the CLI, so it works while the MCP server is down.

1. **Run `basemind doctor`.** It checks the root, the config, the index, the grammars, the hook
   and the daemon, and exits 1 naming the check that failed.
2. **No index, or `file_count` 0.** Run `basemind scan`. A tool answering with a `notice` of
   `empty_index` says the same.
3. **Empty results that should not be empty.** Look for a `notice` first: `warming_up` and
   `building_index` mean retry shortly. Then check the filter: a `path` or `language` no indexed
   file has is an error, but a narrow query is not. Markdown, config and data files are not in the
   code map: find exact text in them with `basemind code grep <pattern>` (MCP: `code_grep`), a topic
   with `docs_search`.
4. **A result looks out of date.** Run `basemind rescan <path>` (MCP: `index_rescan`). With a
   daemon running the rescan is forwarded to it.
5. **Exit 3 (busy).** Another process holds the workspace writer lock; the error names it. If
   that process is alive, query through it or wait; a dead holder's lock is released by the OS,
   so retry.
6. **MCP tools missing or failing.** A stdio MCP server cannot be restarted from inside the
   session; reconnect it from the host (Claude Code's MCP panel, Cursor's MCP settings). Meanwhile
   the CLI reads the same cache: `basemind code …`, `basemind git …`.
7. **After an upgrade.** Run `basemind comms stop` so the next call starts the new daemon. A newer
   minor release rebuilds the index on its first scan.
8. **Too many daemons, or a daemon that will not serve.** `basemind comms doctor --probe` lists
   each live daemon and asks whether it can serve; it prunes dead registry entries.
9. **A tool says a feature is missing.** The binary was built without it; the release binary has
   every feature. See `basemind manual setup`.

A failed tool call is a result with `isError` whose text names the fix: follow it before
diagnosing further. Its `structuredContent` holds a stable `error` code and a `retryable` flag:
`invalid_argument` (change the call), `unavailable` (not in this workspace, session or build),
`index_unavailable` and `busy` (retry; the overload refusal adds `retry_after_ms`), `cancelled`,
and `internal` (a fault inside basemind). The text writes a data-home path as
`$BASEMIND_DATA_HOME/…` and a repository path relative to the repository, so it can be quoted in an
issue; the server log (for the daemon, the `daemon.log` that `basemind comms doctor` names) keeps
the full text and the cause of an `internal` failure.
