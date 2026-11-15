# Upgrading an existing index

basemind keeps its index in a machine-global cache (`BASEMIND_DATA_HOME`, default
`~/.local/share/basemind`): per-workspace views, LanceDB tables and git history under
`cache/workspaces/<key>/`, and one content-addressed blob store under `cache/blobs/` shared by every
workspace. An upgrade never asks you to wipe it. This page says what happens to the old shape and what
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
| `admin` `rescan` | `index_rescan {paths?, full?}` |
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
| `status`, `rescan`, `cache stats\|gc\|clear` | unchanged |

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
- **Telemetry keys are the tool names** (`code_grep`, not `code:grep`).
- **Annotations are exact per tool.** Everything is read-only except `memory_save` and
  `index_rescan`, so a host that gates on read-only can allow the rest. No tool is destructive.

## Running two basemind versions side by side

The release after 0.33 moves the daemon's socket, lock, log and comms store from the comms directory
(`comms/` under the per-user data directory, or `BASEMIND_COMMS_DIR`) into a protocol-generation
subdirectory, `comms/g1/`. On Windows the named pipe's name gains `-g1` and changes with it.

- A 0.33 or older binary and a newer one no longer share a daemon. Each starts its own, and neither
  stops the other's. Before this, every session start by one version stopped (and if needed killed)
  the other version's daemon.
- The comms store is not carried over: threads, messages and the agent roster start empty in the new
  generation. Agents on a 0.33 binary and agents on a newer one do not see each other's threads.
- The old daemon exits when its sessions end. Its files stay at the top of the comms directory; they
  are small and nothing removes them. Delete them by hand once no 0.33 binary is in use.
- A client that reads `http.addr` or `daemon.log` by path must look in `comms/g1/`.
- Between versions that share a generation, a running daemon is never killed while it answers. A
  newer daemon is used, or the older binary fails with `a newer basemind comms daemon ... is running`
  and says to update. An older daemon is asked to stop and keeps running if it is serving sessions.

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
still lists it in `basemind comms doctor` but cannot talk to it.

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
| Resident term index, name dictionaries, heap retag | In memory only. Nothing on disk, nothing to migrate | none |
| `[resources] onnx_provider` defaults to `cpu` | Behaviour only: models load on the CPU provider instead of the platform default (CoreML on macOS). Set `onnx_provider = "auto"` to restore it. No on-disk change | none |
| Daemon `IndexRead` forwarding | Wire protocol only | none |

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

The plugin launcher keeps one `~/.cache/basemind/bin/<version>/` per release. On every launch of the
current one it removes the other version directories that no running process was started from. It
does not stop processes of other versions: their sessions and daemons keep running, and a daemon
exits on its own once idle. That cache is a few
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
