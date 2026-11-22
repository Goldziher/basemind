# Lookups from a script

Every command takes `--root <dir>`, the checkout it answers about, so a script or an agent can
query a repository other than its working directory, and several repositories in one session,
without an MCP server. The query commands below answer from the index: none of them scans or
re-indexes the repository.

## The recipe

Name an indexed checkout by its absolute path in every command, ask for JSON and bound each call:

```sh
basemind status --root /work/checkout --json
basemind code symbols ExampleService --root /work/checkout --limit 10 --max-tokens 1200 --json
basemind code outline src/example_service.rs --root /work/checkout --json
basemind code callers src/example_service.rs handle --root /work/checkout --limit 20 --json
basemind git touching src/example_service.rs --root /work/checkout --limit 5 --json
```

Quote the path when it holds spaces or backslashes: a POSIX shell such as Git Bash reads an
unquoted backslash as an escape, and PowerShell takes `'C:\work\checkout'` as written.

## How the root resolves

- The directory named is widened to its workspace: the nearest directory at or above it holding a
  `basemind.toml`, `<dir>/.config/basemind.toml` or `<dir>/.config/basemind/config.toml`, up to the top of its
  git repository, else that top. A subdirectory with none of those between it and that top therefore
  queries the whole checkout's index, one under a nested config file queries that workspace's, and
  each worktree has its own.
  `basemind doctor --root /work/checkout` prints the root it resolved.
- A root must be a git repository or hold a `basemind.toml` (`basemind manual indexing`); a scan
  refuses any other directory, and so does a daemon a query is forwarded to.
- Path arguments are relative to the root, not to the shell's directory: `src/lib.rs` is
  `<root>/src/lib.rs` wherever the command runs. Write them with forward slashes.

## Reading the answer

1. **Exit status first.** A nonzero exit is no answer: do not parse its stdout. The error is on
   stderr; `2` is a usage error (an unknown flag, a path that is not indexed), `3` busy (retry),
   `4` an unreachable daemon (`basemind manual exit-codes`).
2. **Stdout is the answer.** With `--json` stdout holds only the response; read diagnostics from
   stderr, never mixed into it.
3. **Index state.** In `basemind status`, read the flags before the counts: `rebuild_in_progress`,
   `rescan_needed`, `partial`, `degraded` or a `notice` say the index is not whole, and while a
   rebuild holds the index `file_count` is 0 because the count is unavailable. With none of them
   set, a `file_count` of 0 means this root has no indexed files. Status describes the index, not
   each file: a query result lists a file edited since the scan in `stale`, and what that means
   depends on the command. `code outline` and `code read` answer from the file as it is now; a
   `code callers` or `code refs` hit keeps the line it had at the scan, so read the current source
   with `basemind code read` before using a stale call site. A missing, stale or busy index is a
   finding to report, not a reason to rebuild: building is a write. When one is allowed,
   `basemind scan --root /work/checkout` rebuilds the workspace (it takes no path) and
   `basemind rescan src/a.rs --root /work/checkout` re-indexes named files.
4. **Complete, partial or not ready.** A `notice` of `warming_up`, `building_index` or
   `rescanning` means retry shortly, and `empty_index` that the root has no index: an empty list
   beside a notice is not "no matches". `next_cursor` means more rows: repeat the call with
   `--cursor <next_cursor>` and the same arguments. `budgeted` means the token budget
   (`--max-tokens`, default 6000) trimmed the list. `truncated` with `truncated_reason` is a cut
   you did not ask for: with `scan_cap` the `total` is a lower bound, an empty git page cut at
   `window` is not the end, and `shallow_clone` makes an absent commit inconclusive
   (`basemind manual paging`).
5. **Candidates, not proof.** `basemind code callers` and `basemind code refs` list call sites
   matched by name, each graded by `resolution`. `resolved_total` counts the ones proven to bind to
   the pinned definition: `resolved_total: 0` beside candidates means none was proven, not that
   none exists, and a candidate may call another type's method of that name. `resolution_note`
   says why resolution could not run.
   `basemind code refs handle --path src/example_service.rs --resolution same_file,imported --root /work/checkout --json`
   keeps the proven rows only (`basemind manual resolution`). Confirm an answer that matters in
   source with `basemind code read src/example_service.rs --symbol handle --root /work/checkout`;
   an empty result does not prove absence.
6. **Lines are 1-based.** Every `line`, `end_line` and `column`, in arguments and responses, counts
   from 1, and a `code symbols` row reads `<path>:<line>: <signature>`: cite the number as it is.
