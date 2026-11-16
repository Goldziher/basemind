# Upgrading an existing index

basemind keeps its index in a machine-global cache (`BASEMIND_DATA_HOME`, default
`~/.local/share/basemind`): per-workspace views, LanceDB tables and git history under
`cache/workspaces/<key>/`, and one content-addressed blob store under `cache/blobs/v<schema>/` shared
by every workspace. An upgrade never asks you to wipe it. This page says what happens to the old shape and what
is cleaned up.

## Upgrading to the verb tools (ADR-0018)

The release after 0.33 replaces the mode-dispatched MCP tools `code`, `git`, `memory` and `admin`
with 15 tools, one per job ([ADR-0018](adr/0018-mcp-verb-tools.md)). The change is breaking and has
no aliases: a call to an old tool name fails, and no tool takes a `mode` argument any more. `agents`
is unchanged; the `workspace` tool is removed (see below). The plugin's skills, commands, hooks and rules ship in the same
release and the plugin downloads the matching binary, so update both together.

The tools are always loaded (`_meta["anthropic/alwaysLoad"]: true`): a host that defers MCP tools
needs no tool-search round trip, so call them directly. `tools/list` for them is about 18 KB (about
4.5k tokens), down from about 25 KB (about 6.3k tokens) for the four tools they replace.

### Tool mapping

| Old tool and mode | New tool |
| --- | --- |
| `code` `outline` | `code_outline {path}` |
| `code` `symbols` | `code_symbols {name}` |
| `code` `expand` | `code_read {path, symbol}` |
| `code` `grep` | `code_grep {pattern}` (literal; add `regex: true` for a regex) |
| `code` `semantic` | `code_search {query}` |
| `code` `chunk` | `code_read {path, line, end_line}` on the hit's span (CLI keeps `basemind code chunk`) |
| `code` `references` | `code_refs {name}` |
| `code` `callers` | `code_refs {name, path}` |
| `code` `implementations` | `code_refs {name, relation: "implementations"}` |
| `code` `dependents` | `code_refs {name, relation: "importers"}` |
| `code` `definition` | `code_refs {name, relation: "definition", path, line}` |
| `code` `find` | `find_files {query}` |
| `code` `files` | `find_files {}` |
| `memory` `documents` | `docs_search {query}` |
| `memory` `put` | `memory_save {key, value}` |
| `memory` `get` | `memory_search {key}` |
| `memory` `list` | `memory_search {prefix}` or `memory_search {}` |
| `memory` `search` | `memory_search {query}` |
| `memory` `delete` | CLI only: `basemind memory delete <key>` |
| `git` `recent` | `git_log {}` |
| `git` `touching` | `git_log {path}` |
| `git` `by_path` | `git_log {path: "dir/"}`; an arbitrary regex is CLI only: `basemind git by-path` |
| `git` `search` | `git_log {query}` / `git_log {author}` |
| `git` `show` | `git_log {rev}` |
| `git` `symbol_history` | `git_log {path, symbol}` |
| `git` `churn` | CLI only: `basemind git churn` |
| `git` `blame` | `git_blame {path, line, end_line}` |
| `git` `blame_symbol` | `git_blame {path, symbol}` |
| `git` `status` | `git_diff {}` |
| `git` `diff` | `git_diff {path, from, to}` |
| `git` `diff_outline` | `git_diff {path, structural: true}` |
| `admin` `status`, `repo` | `index_status {}` |
| `admin` `rescan` | `index_rescan {paths?}` (`full` is gone: `{}` is the whole workspace) |
| `admin` `cache_stats` / `gc` / `cache_clear` | CLI only: `basemind cache stats\|gc\|clear` |
| `admin` `telemetry` | CLI only: `basemind admin telemetry` |
| `workspace` `workspaces` / `worktrees` / `branches` | CLI only: `basemind workspace workspaces\|worktrees\|branches` |
| `workspace` `claim` / `release` | Removed, with the CLI commands: worktree claims enforced nothing and nothing read them |

### Renamed arguments

| Old | New | Where |
| --- | --- | --- |
| `path_contains` | `path` | `code_grep` (still a literal substring of the repo-relative path, never a glob) |
| `path_contains` | `path_prefix` | `find_files` (now a prefix) |
| `include_context` | `context` | `code_grep` (default off) |
| `line_start` / `line_end` | `line` / `end_line` | `git_blame` |
| `rev_old` / `rev_new` | `from` / `to` | `git_diff` |
| `entity_category` | `entity` | `docs_search` |
| `keywords_contains` | `keywords` | `docs_search` |
| `start_row` (responses) | `line` / `end_line` | every tool that reports a position |

### Removed arguments

- `mode`, on all 15 tools.
- `format` (`"toon"`): responses are JSON.
- The undocumented aliases (`q`, `needle`, `token_budget` and similar). Only the names in a tool's
  schema are accepted.
- `field` on commit search: pass `query` for messages or `author` for authors to `git_log`.
- `full` on `index_rescan`, and `basemind rescan --full`. `index_rescan {}` (`basemind rescan` with
  no path) already walks the whole workspace, and `full` beside `paths` silently discarded them.

### CLI changes

The group names are unchanged, and every MCP tool has a command. `--json` still prints the tool's
JSON.

| Before | Now |
| --- | --- |
| `code expand <path> <name>` | `code read <path> --symbol <name>` (`expand` stays as an alias of `read`), or `code read <path> --line <n> --end-line <m>` |
| `code grep <regex>` | `code grep <pattern>` is literal; add `--regex` |
| `code grep --path-contains <s>` | `code grep --path <s>` |
| `code files --path-contains <s>` | `code files --path-prefix <s>` / `code find [query] --path-prefix <s>` |
| `code references <name>` | `code refs <name>` (`references` stays as an alias) |
| `code semantic <query>` | `code search <query>` (`semantic` stays as an alias) |
| `code definition <path> <line> [--column]` | `code definition <path> <line> <name> [--column]` |
| `git blame <path> --line-start <a> --line-end <b>` | `git blame <path> --line <a> --end-line <b>` |
| `git diff <path> <old> <new>` | `git diff <path> --from <old> --to <new>` |
| `git search <query> --field author` | `git search <query> --author <name>` |
| `rescan --full` | `rescan` (no path is the whole tree) |
| `status`, `rescan [path…]`, `cache stats\|gc\|clear` | unchanged |

New commands mirror the tools: `code read`, `code search`, `code refs`, `git log` and a `git diff`
that takes no path for the working-tree status. The previous commands remain as shorthands (`code
callers`, `implementations`, `dependents`, `definition`, `find`, `files`; `git status`, `recent`,
`touching`, `search`, `show`, `symbol-history`, `blame-symbol`, `diff-outline`).

Removed flags: `--format` (including `--format toon`), `--l2`, `--path-contains`, `--no-context`,
`--line-start` / `--line-end`, `--field`, `--body-chars`, `--hash-mode`, `--no-embed`, `--no-files`,
and `--rerank` / `--rerank-top-k` on code search. Every list command takes `--limit`, `--cursor` and
`--max-tokens`.

### Behaviour changes

- **`code_grep` is literal by default.** The pattern is plain text unless `regex: true` (`--regex`).
  A script that passed a regex must add the flag, or it searches for the regex source text.
- **Lines are 1-based everywhere**, in arguments and in responses (`line`, `end_line`, `column`).
  Outline, symbol and caller positions were 0-based `start_row` values before; subtract nothing when
  feeding a `line` to `code_read` or `git_blame`.
- **`code_refs` matches the whole identifier.** `open` no longer returns call sites of `open_store`.
  Pass `match: "substring"` for the previous behaviour.
- **Unknown arguments are rejected.** They used to be dropped, so a misspelled filter ran unfiltered
  and reported success. The error names the closest accepted argument and lists all of them.
- **Errors are tool results.** A failure the caller can fix comes back with `isError: true` and text
  naming the fix (a path that is not indexed, an empty index, a compiled-out feature) instead of a
  JSON-RPC error. An empty index is reported with a `notice` (`state: "empty_index"`), never as a
  plain empty list.
- **Default token budget.** Every list honours a default `max_tokens` of 6000 and sets `budgeted`
  when it trims; page with `next_cursor`, or raise `max_tokens`.
- **`git_diff` reads the working tree.** With no arguments it returns the status (staged, modified,
  untracked); with only `path` it diffs that file's working-tree content against HEAD. `to` defaults
  to the working tree and `from` to HEAD.
- **`find_files` lists documents.** Markdown, config and other document-tier files are returned
  together with code files, including on a long-lived server.
- **`index_status.cache_dir`** is basemind's cache directory. It used to report the tree-sitter
  grammar directory.
- **Results are inline.** A call is answered directly; `index_rescan`, `code_search` and
  `docs_search` become a server-side Task only when still running after 3 s. Fast calls are no longer
  returned as background tasks.
- **`limit: 0` is refused** by every tool that takes `limit`. It returned an empty page, with a
  cursor back to the same position where one was issued.
- **A filter that selects no file is an error.** `code_grep` with a `path` or `language`, and
  `find_files` with a `path_prefix` or `language`, that no indexed file passes return an error
  saying what would match (the indexed languages, or that `path` is literal text and not a glob;
  `code_grep` names `docs_search` when only documents match). While the index is still being built
  the empty page is returned with its `notice` instead. `git_log {path}` refuses a path that no
  commit reachable from HEAD touches and that is not on disk, for a file and for a directory.
- **`code_grep {regex: true}` anchors per line.** `^` and `$` match at every line, CRLF-aware, as
  in `grep`; they matched only at the ends of the file. No hit is reported past the last line. A
  line ends at `\n`, with a `\r` before it counted into the ending, as `code_read` numbers lines;
  a `\r` on its own is text inside its line.
- **Edited and deleted files.** `code_outline` and a `code_read` symbol read describe a file edited
  since it was indexed as it is now and set `stale: true`; a `code_grep` hit in such a file has
  `stale: true` and no `symbol`. `code_refs` with `path` (and `relation: "definition"`) answers
  from indexed positions only, so there an edited file is an error naming `index_rescan`. A file
  deleted since the scan gets one error from every tool, also naming `index_rescan`. No tool reads
  an indexed file whole once it has grown past `scan.max_file_bytes`.
- **`code_read` cuts at a line.** A body over 128 KiB ends after its last whole line that fits and
  `end_line` is the last line returned: read on from `end_line + 1`.
- **`git_blame {symbol}` reads the blamed revision.** The symbol's span, and a `line` passed to
  pick one of several, are those of the file at `rev` (default HEAD), not of the working tree. A
  blob over `scan.max_file_bytes` is refused: blame a line span instead.
- **A symbol's history pages past its window.** `git_log {path, symbol}` inspects one window of
  the commits touching the file per page and returns `next_cursor` while older commits remain. A
  page can be short or empty and still not be the last (`truncated_reason: "window"`): follow the
  cursor until it is absent. A symbol the working tree no longer defines has its history read; a
  name that no inspected commit defines either is named in `unmatched` on each empty page and is an
  error on the last. `git_log` with a directory `path` pages the same way, 200 commits per page.
  A cursor of any `git_log` list (recent commits, a file's, a directory's or a symbol's history)
  is refused when the git-history index came up (or stopped answering) since it was issued,
  because the index and the live walk order commits differently: restart without `cursor`.
- **A moved file lists its source.** A commit's `files` (`git_log {}`, `git_log {rev}`) lists the
  source of a move as `deleted` beside its destination as `renamed`, so a directory's history holds
  the commit that moved its last file out. A git-history index built by an earlier version keeps
  the old file lists for the commits it already holds.
- **`code_refs` leaves `resolved` absent where nothing was checked.** With `path`, a hit has
  `resolved: true` (proven to bind to the definition), `resolved: false` (checked and not proven)
  or no `resolved` at all; `resolution` gains `no resolution facts for this definition`.
- **Ranked searches are paged from one retrieval.** `code_search` and `docs_search` cut every page
  from one retrieval of 101 hits (100 can be paged), so a reranker scores up to 101 candidates,
  and a cursor issued before a rescan is refused. The ranked list is kept for up to two minutes, so
  a later page or a repeated call does not rank again. `truncated` says the ranking was cut.
- **`git_diff {}` is bounded.** Each status list is cut at 200 paths, in path order; a `path`
  ending in `/` returns the status under that directory, listing every untracked file on its own,
  with `is_clean` scoped to the directory. A directory that does not exist and has no changed path
  under it is an error, and so is a directory `path` without the trailing slash.
  `structural: true` returns `common` as a count, not a list.
- **`index_rescan {}` is the whole workspace.** The `full` flag is gone (see Removed arguments).
- **Telemetry keys are the tool names** (`code_grep`, not `code:grep`).
- **Annotations are exact per tool.** Everything is read-only except `memory_save` and
  `index_rescan`, so a host that gates on read-only can allow the rest. No tool is destructive.

### Response fields

Every cut is reported in the same words:

- A page that ends at `limit` is paging. `next_cursor` says there is more; nothing else is set.
- A page trimmed to the `max_tokens` budget also sets `budgeted`.
- A cut the caller did not ask for sets `truncated: true` and `truncated_reason`, with `omitted`
  counting the items left out where the count is known. Follow `next_cursor` when the response has
  one, otherwise narrow the call.

A client that read `truncated` as "there is another page" must read `next_cursor` instead. A false
`truncated` is left out on every tool, so a client that requires the field must default it to false.

Four fields keep their own names because they say something else: `skipped_files` (`code_grep`:
files that could not be searched), `limit_clamped` (`find_files`: an over-large `limit` was
lowered), `body_truncated` (`git_log` search: one commit's message was shortened; read it with
`rev`) and `too_large` (`git_diff`: a file over 4 MiB was not diffed). `memory_search` still sets a
bare `truncated` on a listing.

| Tool | Before | Now |
| --- | --- | --- |
| `code_grep` | `truncated` on a `limit` cut, `truncation_reason` | `next_cursor` alone on a `limit` cut; `truncated`, `truncated_reason: "byte_budget"` when files were left unread |
| `code_grep` | none | `skipped_files` (files that could not be searched; not a cut), `stale` on a hit |
| `code_outline` | `imports_truncated` | `truncated`, `truncated_reason: "imports"`, `omitted` |
| `code_read` | `truncated` | `truncated`, `truncated_reason: "size_cap"`, `omitted_bytes`; `stale` on a symbol read |
| `code_symbols` | `truncated` on a `limit` cut, `total_is_partial` | `next_cursor` alone on a `limit` cut; `truncated`, `truncated_reason: "scan_cap"` when a fuzzy sweep stopped at its cap |
| `code_refs` | `total_is_partial` | `truncated`, `truncated_reason: "scan_cap"` (`total` is a lower bound; `next_cursor` resumes the scan) |
| `find_files` | `truncated` | removed: `next_cursor` says a listing goes on |
| `code_search`, `docs_search` | none | `truncated`, `truncated_reason` (`depth`, `rerank_top_k`, `filter_depth`) |
| `git_diff {}` | none | `truncated`, `truncated_reason: "path_cap"`, `omitted` (paths), `path` |
| `git_diff {path}` | `truncated`, `total_hunks` | `truncated`, `truncated_reason` (`hunk_cap`, `byte_cap`), `omitted` (hunks) |
| `git_diff {structural}` | `common` (a list) | `common` (a count) |
| `git_log {rev}` | none | `budgeted`, `omitted` (files left out of `files`) |
| `git_log {path, symbol}` | none | `budgeted`, `truncated_reason: "window"`, `next_cursor` past the window, `unmatched` |
| `git_log {path: "dir/"}` | `truncated` at a fixed 200-commit window | `truncated_reason: "window"`, `next_cursor` past the window, `unmatched` |
| `git_log {query}` | `partial` | `truncated`, `truncated_reason` (`live_walk`, `scan_cap`) |

## Running two basemind versions side by side

The release after 0.33 moves the daemon's socket, lock, log and comms store from the comms directory
(`comms/` under the per-user data directory, or `BASEMIND_COMMS_DIR`) into a protocol-generation
subdirectory, `comms/g1/`, and its record in the daemon registry from `daemons/` to `daemons/g1/`
under the data directory. On Windows the named pipe is named from a hash of the generation
directory, so its name changes too.

- A 0.33 or older binary and a newer one no longer share a daemon. Each starts its own, and neither
  contacts or stops the other's. Before this, every session start by one version stopped (and if
  needed killed) the other version's daemon.
- **A 0.33 plugin left installed does not reach the newer version.** The plugin launcher of 0.33
  and older sends SIGTERM and SIGKILL to every basemind process of another version started from
  its cache, `~/.cache/basemind/bin/<version>/`, each time one of its own sessions starts, and
  deletes the other version directories there. The newer launcher installs under
  `~/.cache/basemind/versions/<version>/`, which the old launcher neither matches nor deletes, and
  it never installs into or launches from `bin/`. Launchers of 0.33 and older still do this to
  each other, and to a newer binary only if it was put under `bin/` by hand; update or remove
  them. The newer launcher reclaims `bin/` once it is abandoned: on each launch it removes a
  version directory there that was installed more than 14 days ago (`BASEMIND_PRUNE_AFTER_DAYS`),
  that no running process was started from and that no install holds the lock for. A 0.33 plugin
  still in use after that downloads its version again on its next launch. A Homebrew, npm or pip
  install does not run a launcher.
- **The workspace index, memory notes and git-history stores are not per generation.** They are
  keyed by workspace and admit one daemon at a time. While the daemon of one generation holds a
  workspace, the daemon of the other cannot serve its index, memory or git history; that lasts
  until the first daemon lets go (it exits ten minutes after its last client). Two generations
  side by side is a state to pass through during an upgrade, not one to work in on the same
  repository.
- **What a session shows while another version holds the repository.** With a 0.33 session still
  open in a repository, a session of the newer version started there comes up without an index.
  Its `initialize` waits ten seconds for the index to come free and is then answered by
  `basemind serve` itself, so the host shows the server as connected and lists its tools. Each
  tool call is answered with the retryable error `backend_unavailable: another basemind holds
  this workspace's index (the basemind v0.33.0 daemon, pid 4242). This session connects by itself
  once that process lets go: a daemon of another basemind version exits when its sessions are
  closed and it is idle, a command when it finishes` (JSON-RPC code `-32003`); so are requests
  for resources and prompts. The parentheses name the lock's holder: a daemon by
  the version it registered under, any other process by its command
  (`` `basemind scan`, pid 4242 ``), and they are left out when the lock records nobody. Closing
  the old version's sessions fixes it: that daemon exits ten minutes after its last client, or at
  once on that version's `basemind comms stop`. The waiting session connects on its next retry,
  at most a minute later, without being restarted by the host, and sends
  `notifications/tools/list_changed`.
- The comms store is not carried over: threads, messages and the agent roster start empty in the new
  generation. Agents on a 0.33 binary and agents on a newer one do not see each other's threads.
- The old daemon exits when its sessions end. Its files (`comms.sock`, `daemon.lock`, `daemon.pid`,
  `daemon.log`, `http.addr` and the `store.fjall/` directory with its message history) stay at the
  top of the comms directory until `basemind comms cleanup` removes them. That command removes the
  pre-generation files, and any other generation's directory, only when no daemon holds the
  rendezvous's `daemon.lock` and its newest file is more than 30 days old (`--older-than <days>`
  changes the age, `--dry-run` lists what would go with sizes). `--older-than 0` drops the age
  test, so the old message store goes as soon as no daemon of that release is running. A directory
  without a `daemon.lock` is not taken for a rendezvous and is left as it is, and so is one whose
  lock cannot be opened. It contacts and signals nothing and never touches the generation of the
  binary that runs it. Nothing runs it automatically.
- `basemind comms stop --all` and `basemind comms doctor` (with or without `--probe`) now cover the
  daemons of their own generation only. To stop or inspect a 0.33 daemon, use the 0.33 binary.
  `doctor` says how many daemons of other generations it counted; all of them count toward the
  ceiling of eight (`BASEMIND_MAX_DAEMONS`).
- A client that reads `http.addr` or `daemon.log` by path must look in `comms/g1/`. With
  `BASEMIND_ALLOW_HTTP`, the HTTP front-end still prefers `127.0.0.1:51786`, but the second daemon
  to start finds it taken and binds a port the OS assigns: read the port from `http.addr` (or
  `basemind daemon ensure`), never assume it. An address set with `BASEMIND_HTTP_ADDR` is not
  substituted.
- A long `BASEMIND_COMMS_DIR` has three fewer bytes to spare: the socket is now
  `<dir>/g1/comms.sock`, and a Unix socket path is limited to 103 bytes on macOS and 107 on Linux.
  A path over the limit fails with a message naming the variable.
- Between versions that share a generation, a running daemon is never killed while it answers. A
  daemon of the same or a newer release is never asked to stop: it is used, or, when it is newer and
  speaks another protocol, the older binary fails with `a newer basemind comms daemon ... is
  running` and says to update. A `basemind serve` session that meets this on reconnect answers its
  pending requests with that message and exits.
- An older daemon is asked to stop and keeps running if it is serving sessions. When it also speaks
  another protocol, sessions of the new version get no daemon until it goes: commands fail with
  `a basemind comms daemon from another build ... refused to stop`, and a `basemind serve` session
  answers each request with that error and retries with a growing back-off (it asks again at most
  once a minute). Close the sessions of the old version (its daemon exits ten minutes after the
  last one) or run `basemind comms stop`, which now reaches a daemon on another protocol version
  and reports its refusal instead of killing it.
- A daemon whose store is broken answers every request with a store error. A client that meets one
  names the store directory and tells you to run `basemind comms doctor --probe` and
  `basemind comms stop --all`.

## Upgrading from 0.28 to 0.29

0.29 bumps the release minor, so the first scan rebuilds the index, and it reshapes the CLI.

1. Stop the old daemon before the first scan: `basemind comms stop` (add `--all` to stop every daemon
   on the machine). Otherwise the previous binary's daemon keeps running with the old schema and
   still owns the index.
2. Run `basemind scan`. Each view's `index.msgpack` and Fjall index are reset on open and every file is
   re-extracted, overwriting stale blobs in place; the git cache refills lazily. The LanceDB document
   tables and the blob store are not wiped, and the scan's cleanup pass reclaims orphaned blobs.
   Expect a full-scan duration once.
3. Re-run `basemind hook install`. The new hook honours `core.hooksPath` and linked worktrees, fails
   open, and it will not overwrite a pre-commit hook it did not write unless you pass `--force` (the
   old one is kept as `pre-commit.bak`).
4. Update scripts for the renamed commands. No aliases remain:

| 0.28 | 0.29 |
| --- | --- |
| `admin status` | `status` |
| `admin rescan` | `rescan` |
| `admin cache-stats` | `cache stats` |
| `admin gc` | `cache gc` (deletes; `--dry-run` reports) |
| `admin cache-clear` | `cache clear` (needs `--yes` outside a terminal) |
| `serve --git-cache-mem`, `--no-git-cache-disk`, `--no-watch`, `--documents-*` | removed (`serve` takes no flags) |

`admin delta`, `admin checkpoint` and `admin waste` moved to the top level in 0.29 and were removed
afterwards, together with `admin compress` and `compress-output`; they have no replacement.

The `web` tool and the `basemind web` commands (`scrape`, `crawl`, `map`) were removed, with the
`crawl` cargo feature; they have no replacement. A `[crawl]` table in `basemind.toml` is still
accepted and ignored. Pages already ingested stay searchable under their `web:<host>` scope
(`docs_search` with `scope: "web:<host>"`, or `memory documents --scope web:<host>`) and are no
longer refreshed.

The `shell` tool and the `basemind shell` commands (`spawn`, `send`, `capture`, `kill`, `list`,
`broadcast`) were removed, with the `shells` cargo feature; they have no replacement. A `[shells]`
table in `basemind.toml` is still accepted and ignored. Stop a shells daemon an older version left
running before upgrading (`basemind shell kill` each session, or kill the process); this version
cannot talk to it and, since it is registered by an older release, does not list it in
`basemind comms doctor`.

The `memory` modes `audit`, `mine`, `proposals`, `accept` and `reject` and their `basemind memory`
commands were removed; they have no replacement. `put`, `get`, `list`, `search`, `delete` and
`documents` were kept (and are now `memory_save`, `memory_search` and `docs_search`, see above), and
stored notes keep every field, including the verdict and importance
an audit wrote, which nothing updates any more. Archived notes and pending proposals stay on disk in
the repo's `memory.fjall` and are never read. The comms protocol version is now 5: the first
upgraded session replaces a running older daemon, and a client and daemon that still disagree fail
the handshake with `proto_skew`.

The `graph` tool and the `basemind graph` commands (`calls`, `neighbors`, `path`, `subgraph`,
`communities`, `map`, `export`, `display`, `open`) were removed, with the `basemind://repo/map`
resource and the daemon's `/ui` page; they have no replacement. The caller, call-site,
implementation, importer and definition lookups (now `code_refs`) answer the who-calls-what
questions. The daemon's HTTP front-end now serves `/mcp` only and takes the bearer
token in the `Authorization` header only: `?token=` is refused.

The `documents` cargo feature was split into tiers. `documents` is document search without OCR or
named-entity recognition; `documents-ocr` adds Tesseract OCR (raster images and PDFs with no text
layer) and `documents-ner` adds ONNX named-entity recognition (`[documents.ner]` and the
`entity` filter of `docs_search`). `full` includes all three, so the published binaries are unchanged except
that they no longer contain PaddleOCR, which is the source-only `documents-ocr-paddle` feature. A
source build that used `--features documents` needs
`--features "documents documents-ocr documents-ner"` (or `full`) to keep OCR and NER. Without
`documents-ocr`, raster images are not indexed as documents and a PDF with no text layer is indexed
with no text, with a warning naming the file and the feature; without `documents-ner`,
`[documents.ner] enabled = true` logs a warning and has no effect, and `docs_search` with
`entity` returns an error. A build with both tiers keeps its cached documents. A build
missing a tier extracts every document again on its first scan, and so does the first scan after
switching to a binary with a different set of tiers.

Archive extraction, LLM-backed (abstractive) summarization and the LLM NER backend were removed, with
HEIC/HEIF, SQLite, WordPerfect, HWP/HWPX and Apple iWork extraction and SVG rasterisation (SVG text
is still read as XML); they have no replacement. These settings are still accepted and ignored, each
with a one-time warning: `[documents] extract_archives = true` (archives are always skipped, and
`BASEMIND_DAEMON_ALLOW_EXTRACT_ARCHIVES` has no effect),
`[documents.summarization] strategy = "abstractive"` (extractive summaries are used),
`[documents.ner] backend = "llm"` (no entities are extracted) and a non-empty `[llm]` table. The
`--llm-*` flags on `basemind scan` and the `BASEMIND_LLM_*` variables are accepted and ignored too.
`max_archive_bytes` and `max_archive_entries` still apply, to the zip containers that Office files
are. A config that set `[llm] model` or `extract_archives = true` has its documents extracted again
once, on the first scan after upgrading; any other config keeps its cached documents.

Also check scripts for these behaviour changes:

- Exit codes are `0` ok, `1` error, `2` usage, `3` writer lock held, `4` daemon unreachable, `130`
  interrupted. `scan` and `rescan` exit `3` (not `0`) when another process holds the lock.
- The timing footer is on stderr; stdout carries only the answer. Human output no longer truncates
  values to 200 characters.
- `agents` and `workspace` `--json` now use the MCP response shapes.
- MCP logging is gone, and the `memory` tool no longer accepts the `documents.*` / `llm.*` override fields.

## What changes on upgrade, and how it migrates

| Change | Mechanism | Cost |
| --- | --- | --- |
| Prose, data and config files (markdown, json, yaml, toml, xml, csv, ini, ...) left the code map for the document tier | The next scan, full or incremental, notices a path indexed in the other tier, purges its code-map rows (file map, fjall `files` / calls / implementations / resolved / bm25 entries, code-search chunks) and indexes it as a document | Documents are chunked, and embedded when `[documents] embed` is on |
| Symbol `signature` now ends at the grammar's body field instead of embedding the body | `EXTRACT_EPOCH` (see below): only files whose entry or blob predates the epoch are re-extracted, in the normal bounded scan | One parse per affected file, no wipe |
| Release-minor schema bump (`0.28.x` to `0.29.x`) | `RELEASE_MINOR` mismatch: each view's `index.msgpack` and fjall index are reset on open and every file is re-extracted, overwriting stale blobs in place | Full re-extraction, by design |
| `grep_bloom` index keyspace (per-file trigram bloom for `code_grep`) | A new keyspace, so `INDEX_SCHEMA_VER` is not bumped. An index without it opens with an empty `grep_bloom` and greps correctly at once (every file is a candidate); the next scan builds the rows from the bytes it reads, and an unchanged file with no row is read once to backfill it | One read per unchanged file, once; about 12.8 % of the indexed code bytes on disk |
| Blob store namespaced by blob schema: `cache/blobs/v<schema>/` instead of `cache/blobs/` | Nothing is moved. A file map records the layout it was written for (`blob_layout`); one from before the namespaces is discarded with its index at the first open, so the first scan re-extracts into the new namespace and embeds every document and code chunk again (the embeddings live in the blobs). The blob GC removes un-namespaced blob files and the namespaces of older schemas once a file is past the 6 hour grace period, and never touches, counts or evicts for the namespace of a newer schema, so two installed versions no longer overwrite each other's blobs. This ships with the 0.34 release-minor bump: an older basemind left installed finds its blobs gone and re-extracts on its next scan | Full re-extraction and re-embedding, once |
| Memory vectors live in `cache/repos/<group>/memory.lance/<model>-<dim>-<hash>/`, beside `memory.fjall`, instead of the workspace's `lance/` | Not migrated. A `memory_search` by meaning embeds the notes that have no current vector row, from `memory.fjall`, up to 256 per call, and says so in `notice` until every note has one; the same happens for an embedding preset used for the first time and after a release-minor bump empties the table. The old `memory` table goes with the workspace's `lance/`, which the release-minor bump wipes at its first open. A store of a preset no longer used is not removed | One embedding per note, once per preset |
| `index.msgpack` fields `rebuild_pending` and `rebuild_pins`; `pending-vectors.msgpack` and `vector-store-id` beside it; `store-id` in the workspace's `lance/` | Additive: a file map written before them reads as owing no rebuild, and a view without the files as owing no vector rows. Vector rows that a development build of this release recorded as owed inside the file map are not carried over: the first scan after the upgrade embeds and writes every row again | none |
| `memory.fjall` keyspace `store_meta` (the notes store's own version) | Written at the first open that holds the workspace lock, by a version that has it. Notes are never wiped. A store stamped by a newer basemind is refused for memory operations with "written by a newer basemind"; the code index of the workspace still opens | none |
| `verify-blobs` marker in the workspace cache directory; `hosting/<pid>` under the data directory, holding the boot the daemon was started in and its process start | The marker is written when a scan of the workspace is found to have died without finishing, or when a process about to scan finds the `hosting/<pid>` of a daemon from an earlier boot of the machine (then for every workspace at once). A daemon that was killed while the machine stayed up leaves no request. The next scan of that workspace is a full one that decodes each combined and document blob instead of trusting it for existing, rewrites the vector rows from the blobs, then removes the marker | One blob read per file and one rewrite of the vector rows, for that one scan |
| Resident term index, name dictionaries, heap retag | In memory only. Nothing on disk, nothing to migrate | none |
| `[resources] onnx_provider` defaults to `cpu` | Behaviour only: models load on the CPU provider instead of the platform default (CoreML on macOS). Set `onnx_provider = "auto"` to restore it. No on-disk change | none |
| Daemon `IndexRead` forwarding | Wire protocol only | none |
| Daemon protocol version 6 (was 5): the batched memory reads and the `verify` flag of a rescan | A client and a daemon of different protocol versions refuse each other at connect. A 0.33 install never meets this daemon: it runs in another protocol generation. Within the generation the rules of [running two versions side by side](#running-two-basemind-versions-side-by-side) apply: a newer client asks an older daemon to stop and has no daemon while that one serves sessions (close them or run `basemind comms stop`), and an older client that finds this daemon is told to update | none on disk |

## The extractor epoch

`SCHEMA_VER` equals the release minor, and a mismatch wipes every cache, so a patch release can never
use it. When an extractor fix changes what an unchanged file's blob should contain but not how it is
serialized, bump `extract::EXTRACT_EPOCH` instead:

- every `FileEntry` and every L1 blob records the epoch it was produced under (`0` when written before
  the field existed, so older data deserializes unchanged);
- the unchanged-file shortcut and the blob-reuse path both require `epoch >= EXTRACT_EPOCH`, so a
  stale file falls through to a fresh parse and its blob is rewritten in place;
- the rewrite keeps the L2 call tier of the old blob, and runs inside the same memory-bounded scan as
  any other changed file;
- the scan reports it as `refreshed` (and `tier_migrated` for tier moves).

Epoch history: `1` is header-only signatures. A binary that predates the epoch can still read the data
(extra fields are ignored); it treats entries as epoch `0`, so the next new-build scan refreshes them
once more.

## Automatic cleanup

Content-addressed blobs are shared and never rewritten on their own, so a migration orphans some of
them: the `.fm` / `.chunk` / `.rref` blobs of a path that became a document keep the same content hash
as its live `.doc` blob.

- The sweep is tier-aware. A `.doc` blob is live only if some workspace's `doc_files` references the
  hash; `.fm`, `.chunk` and `.rref` blobs are live only if a `files` entry does. Before this, the
  shared stem kept the dead code-lane blobs alive forever.
- It reference-counts against every workspace on the machine, keeps blobs younger than 6 hours (a
  concurrent scan may be about to reference them; `BASEMIND_BLOB_GC_GRACE_SECS` overrides), and takes a
  machine-wide `cache/gc.lock` so two sweeps never build the live set at once.
- A scan that migrated a path, refreshed an extraction, or reset a view for a schema bump runs the
  sweep itself and prints `cleanup: reclaimed N orphaned blob(s), B bytes`. In the daemon the same
  condition triggers a sweep one minute later. The hourly daemon sweep and `basemind cache gc` run it
  too; `cache gc` used to be a report-only no-op.
- The result is recorded in `gc-state.json` and shown as `last_gc` by `cache stats`; `orphan_blob_count` there is tier-aware and drops to `0` after the sweep.

A second scan after the upgrade is a no-op: nothing is migrated, refreshed or reclaimed, and the blob
set stops changing.

## Clearing and downgrading

`basemind cache clear --component blobs|views|lance|git-cache|telemetry|all` covers every on-disk
component the migration touches; the term index and name dictionaries are memory only. The sweep lock
(`cache/gc.lock`) is an empty file and is removed with the cache root. `git-history.fjall` is only
removed by `all`: the daemon holds it open. `cache clear` refuses while a writer holds the workspace
lock (and `blobs` while the daemon runs), and everything except `git-cache` needs `--yes` off a terminal.

Opening upgraded data with an older binary fails safe. The index extras are ignored, a binary on the same
schema minor re-maps the files it still treats as code (and drops their document entries), and one on a
different minor resets the view and re-extracts, as for any schema change. Going back and forth costs
re-extraction, never correctness; the next sweep reclaims what is left.

## Per-version binaries

The plugin launcher keeps one `~/.cache/basemind/versions/<version>/` per release (`XDG_CACHE_HOME`
replaces `~/.cache`) and writes a `.last-used` marker into the directory of the version it starts,
on every launch. On every launch of the current version it removes another version's directory
only when all of these hold: nothing has launched that version for 14 days
(`BASEMIND_PRUNE_AFTER_DAYS` changes the number), no running process was started from it, and no
install holds its lock. It removes none when it cannot read the process list (on Git Bash, MSYS2
and Cygwin that is `ps -W`, with the cache path converted by `cygpath`). A version directory left
under `~/.cache/basemind/bin/` by a launcher up to 0.33 goes by the same rule, judged by the
directory's own age since those launchers write no marker. Install locks and
staging directories of other versions go only after ten minutes untouched, a staging directory
only when the process installing into it is gone. It does not stop processes of other versions:
their sessions and daemons keep running, and a daemon exits on its own once idle.
That cache is a few
tens of MB per version and is not touched by the above.

## Adding a new on-disk component (checklist)

A derived index should plug into the same hooks so it upgrades and cleans up like the rest (the grep
trigram bloom, `src/index/grep_bloom.rs`, is a worked example: a versioned row per file, a missing or
older row reads as "no information" and is backfilled by the next scan):

1. Stamp the component with its own format version and treat a mismatch as "rebuild lazily", never as an
   error. If it is per-file, store the stamp on the `FileEntry` (or the blob) the way `extract_epoch`
   does, so only stale files are rebuilt, inside the bounded scan.
2. If it adds a blob suffix, add it to `BLOB_SUFFIXES` and, when a code-map entry owns it, to
   `CODE_LANE_SUFFIXES` (`src/store_gc_live.rs`); a doc-tier suffix needs no entry. A suffix in neither
   list is never swept.
3. If it writes under a workspace, add it to `CacheComponent` / `clear_component_in` and to
   `cache_stats` so `cache clear` and `cache stats` account for it.
4. Count anything the pass displaced in `ScanStats` and fold it into `displaced_artifacts()`; the CLI
   scan and the daemon then sweep promptly.
5. Add a case to `tests/upgrade_from_release.rs` (it skips when no released binary is cached) and a unit
   test next to the new component.
