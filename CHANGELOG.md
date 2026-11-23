# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- Keep a Changelog repeats Added/Changed/Fixed headings per version. -->
<!-- markdownlint-disable MD024 -->

## [Unreleased]

### Changed

- The blob store shards each namespace into 256 buckets by the first two hex digits of the hash, and a file-view build no longer stats every blob; flat blobs migrate on first read, no rescan needed (#285)

## [0.34.0] - 2026-11-22

**The first scan after upgrading rebuilds every index and the blob store.** `RELEASE_MINOR` goes 33
to 34, so each view's index is wiped at its first open, the blob store moves to `cache/blobs/v34/`,
and every file is re-extracted and every document and code chunk re-embedded once. Read
[`docs/UPGRADING.md`](docs/UPGRADING.md) before installing.

**Highlights:**

- **One MCP tool per job (ADR-0018).** The mode-dispatched `code`, `git`, `memory` and `admin` tools
  are replaced by verb tools with exact schemas and no `mode`; the `agents` tool by `agent_post`,
  `agent_inbox` and `agent_threads` (agent comms v2, ADR-0021). No aliases.
- **Smaller answers.** The locating tools answer with rows as the shell prints them (ADR-0052), and
  the text block of `code_grep`, `code_outline`, `git_diff` and a `find_files` list is no longer JSON
  (#157, ADR-0053).
- **The import graph and analysis commands.** `code_refs` lists a file's importers and the import
  cycles (#121, #135); every call site says how it was resolved (#125, ADR-0047). New CLI commands:
  `code impact` (#121), `code cycles` (#135), `code explain` (#120), `code deadcode` (#123),
  `code migration check` (#130), `overview` (#131), `git hotspots` (#134) and `sessions` (#128).
- **Scoped and size-aware queries.** `changed` limits `code_grep`, `code_symbols`, `code_refs` and
  `find_files` to the files a change touched (#122); `index_status` and `find_files` `min_lines`
  name the large files before they are opened (#127).
- **One index per repository group (ADR-0028)**, shared by every worktree and view, and `docs_search`
  rebuilt on one document store per group (ADR-0029, ADR-0037), with the documentation of the
  repository's dependencies behind `[documents.dependencies]` (#132).
- **Agent launch.** `basemind serve --lead` launches agents in worktrees of their own, off until the
  operator's `agents.toml` enables it (ADR-0040).
- **An offline manual**, `basemind manual [topic]`, compiled into the binary.
- **Removed:** web ingestion, agent shells, the code graph tool, the token-saving text tools, the
  `workspace` tool, memory audit and mining, archive extraction and LLM summarization.

### Added

- An unknown `basemind.toml` key names its TOML table (`[scan]`, `[[x]]` for an array-of-tables
  entry) and lists the keys that table allows instead of the raw JSON-schema message, preceded by the
  closest known key when one is within two edits (#249):
  ``unknown key `folow_symlinks` in [scan]; did you mean `follow_symlinks`? allowed keys: ...``.
- `path` and `language` on `code_symbols` and `code_search` (#252), with the meaning they have on
  `code_grep` and `find_files`: `code_symbols {"name":"Config","path":"services/payments/"}` finds the
  definitions under that directory only. `code_search` keeps the hits in files the filters pass
  among the 1,000 best candidates of each lane, and cites only the copies of a file that pass.
  A filter no indexed file passes is refused. `basemind code symbols|search` take `--path` and
  `--language`.
- The cache budget can be set in a user settings file (#204): `[cache] budget_mb` in `settings.toml`
  in basemind's user configuration directory (`~/.config/basemind/` on Linux,
  `~/Library/Application Support/basemind/` on macOS), read by every basemind process at start, the
  daemon an MCP host spawns included. A set `BASEMIND_CACHE_BUDGET_MB` wins over it; `0` disables
  enforcement in either. `basemind cache stats` reports `budget_warning` when a set budget is larger
  than the cache plus the free space on its volume, which the disk fills before eviction starts.
- `basemind upgrade --plan` (#210, CLI only, `--json` too): a dry run of a release-minor rebuild.
  Per repository group and workspace it prints what the next writer open rebuilds (each view kept,
  rebuilt with its reason, set aside or refused, and whether the group index is replaced), the bytes
  that frees, the free space the rebuild needs by the scan's own estimate (1.5x the group index plus
  the `BASEMIND_MIN_FREE_MB` headroom), a code-map scan estimate, and the `basemind scan --root`
  command that rebuilds one worktree at a time; then the blob namespaces the sweep reaps and the
  space the new namespace takes. The verdicts are the writer open's and the blob sweep's own
  decisions, and the plan writes nothing. See docs/UPGRADING.md, "Planning a release-minor rebuild".
- First-scan progress (#212). During a full scan `index_status` reports `scan_progress`: the
  `phase`, the candidate files extracted (`files_done`) of those the scan works through
  (`files_total`), and `eta_secs`, the seconds extraction has left at the rate it has run so far.
  The scan records its count in its breadcrumb once per drive chunk, so a scan run by another
  process is reported too, and `basemind statusline --root` adds it to the status line
  (`indexing 1.2k/40k files · extract · ~3m left`) in place of the "no index" hint during a first
  scan.
- `--help-json` on every command (#332) prints the command's definition as JSON from the clap tree
  the binary parses with: `positional_count` (`min`, `max`, `null` when unbounded), each positional
  and flag with its names, aliases, `takes_value`, value name, default, possible values and env
  variable, and the subcommands. It is answered before argument parsing, so a command missing its
  required arguments still describes itself, and it touches no workspace, cache or daemon.
- `basemind docs search <query>` (#262) runs `docs_search`, with the flags `basemind memory documents`
  took; `memory documents` stays as a hidden alias.
- `basemind scan --json` and `basemind rescan --json` (#262) print only the `index_rescan` response
  (`scanned`, `updated`, the document and skip counts, `elapsed_ms`, `root`) on stdout, for a scan run
  in the process or forwarded to the daemon. `--json` no longer warns that it has no effect there.
- `basemind memory clear [--yes]` (CLI only) removes every memory note of the repository group, of
  every tier and agent, and empties and purges the vector store of every embedding model. It asks
  for confirmation on a terminal and refuses (exit 2) without `--yes` anywhere else; with the daemon
  running it runs in the daemon, and while another process holds the index without a daemon it is
  refused (exit 3). `basemind cache stats` names the repository group directory that holds the
  notes and their vectors as `memory_dir` (#323).
- `memory_search` with `query` falls back to matching words when no embedding model can embed the
  query (none downloaded, offline, the model refused the text) instead of failing: a note is a hit
  when at least half of the query's words begin a word of its key, value or tags, ranked by how
  many it holds, and the reply carries `"lexical": true` and a `notice` saying why. A search by
  meaning drops the hits past a relevance floor (squared L2 distance `1.0`, a cosine similarity
  below one half) and says how many it left out, so an unrelated query returns no hit instead of
  the ten nearest notes (#326).
- `basemind manual environment` lists every `BASEMIND_*` variable the binary, the plugin scripts and
  the npm and pip packages read, each with its default and scope (#237); a test fails when the shipped
  code reads a variable the table lacks.
- The npm package honours `BASEMIND_BIN` to run another binary, such as a local build, as the plugin
  launcher does (#237). `BASEMIND_BINARY`, the pip package's earlier name, stays an alias in all three:
  the launcher and the npm and pip packages read `BASEMIND_BIN` first and fall back to it.
- `code_refs` over the file-level import graph (#121, #135): `relation: "importers"` with `path`
  lists the files importing that file `by_depth`, up to `depth` hops (default 1, max 5), names those
  in an import cycle with it and says when the walk was cut; `relation: "cycles"` lists the import
  cycles smallest first, or with `path` the one holding that file. Both follow runtime imports only,
  and a `coverage` note names a language this build has no import resolver for. `name` is optional
  for these two relations and still required for the others (`basemind code refs` likewise).
- `basemind code impact <path|symbol>` / `--diff <rev|A..B>` / `--changed` (#121): the files that
  import a file, the files defining a symbol or the files a change touches, transitively to `--depth`
  (1-5) through the file-level import graph, each with its depth, the file it imports and whether it
  is in a cycle. `basemind code cycles [--path --max-size]` (#135) lists the import cycles smallest
  first, each with an import chain that closes it (`a.rs:12 -> b.rs:40 -> a.rs`) and a language note;
  `--check FROM TO` says whether that import would close one. Both report the graph's coverage per
  language and are answered by the daemon (ADR-0047).
- `basemind overview` (#131, CLI only, answered by the daemon): languages with file and line counts,
  top-level directories, build manifests (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`,
  `pom.xml`, `build.gradle`) with their dependencies and declared entry points, the largest and most
  imported files, HTTP routes, naming conventions per language and the latest commits; `--sections`
  picks sections and `--limit` bounds each list. `index_status` gains `most_imported`, the five files
  with the most runtime importers (ADR-0047).
- `basemind git hotspots [--since 90d] [--path P] [--limit N]` (CLI only, #134): the files a commit in
  the window touched where two or more of commits, distinct authors, lines, symbols, the longest
  function and importers are in their top 10%, as raw numbers with no score. The window ends at
  HEAD's commit time and counts the commits authored in it; the walk ends at a run of commits made
  before the window, so a merged or rebased branch whose commits keep older author dates does not
  end it (#299).
- `basemind code explain <symbol|path|path:line>` (CLI only, #120): one briefing composed from the
  outline, `code_refs`, the import graph and git history. A symbol gets its definition (line span,
  signature, doc comment, read across attributes and decorators), its body when it has 60 lines or
  fewer, its callers with their resolution (`--depth 2|3` adds the callers of those over call sites
  proven to bind), the names its body calls (macros excluded), the files importing its file and the
  last 5 commits that changed it. A file gets its outline, imports, importers, the calls into and out
  of it grouped by file, its `git hotspots` row and rank, its last 5 commits and the authors of its
  last 20. List sections hold `--limit` rows; the whole answer stays within `--max-tokens`, or is
  refused with the smallest budget that fits; `truncated_sections` names every cut section with the
  call that continues it. `--sections` and `--no-code` narrow it. The `explain` eval kind runs
  against it, and `admin eval` no longer has a skip path for kinds without an op (ADR-0047).
- `basemind code migration check [<name>]` (CLI only, #130): rules in `.basemind/migrations/<name>.toml`
  name old forms (`call`, `import`, `identifier` or `text`, literal or regex) and optionally their
  replacement; the report gives remaining lines per rule and file, the share migrated, files holding
  both forms and failed presence assertions, and each run is appended to a history in the workspace
  cache dir. `--fail-on-remaining` exits 1; `--ranges` adds read ranges.
- `basemind code deadcode` (CLI only, #123): files no entry point reaches through the import graph
  (entry points from the build manifests, a package's `index.html` `<script>`s, each language's
  conventions and the new `[analysis] entry_points` globs in `basemind.toml`), definitions whose
  name no other identifier in the indexed code spells, and imports that resolve to nothing. Each
  finding has a tier and its reason: a name spelled only in comments is `medium`, only in strings
  `low`, and dispatch, decorators, attributes and importers outside the repository lower a tier.
  Names a package publishes are not reported (Rust `pub` in a library, Go capitalised outside
  `package main`, a JS `export` reached from `exports` or `main`, Python `__all__`, Java `public`
  and `protected`); other exports are reported at a lowered tier. `--path`, `--kind`, `--tier`,
  `--limit`; `--fail-on-dead-code` exits 1 on a `high` finding (or one at `--tier` when given), and
  on a report that could not judge what was asked (`incomplete`) instead of passing. A
  `package.json` script's targets now name every file its command runs (`node gen.js && node
  build.js`), a build output in `main`, `exports` or `types` maps back to its `src/` source, and
  `source`, `module`, `types` and Python `[project.entry-points]` are read as targets.
- `basemind manual [topic]` prints an offline manual compiled into the binary: an index and pages
  on code navigation, resolution, git, documents, memory, agent messaging, indexing, paging, read-only
  lookups from a script with an explicit `--root` (#333), setup, exit codes, environment variables
  and troubleshooting. It needs no index or daemon. The pages in
  `docs/manual/` are also the source of the skills' manual sections, spliced in by
  `scripts/sync_public_skills.py --manual` and checked for drift by `plugin_manifest_smoke` (#133).
- `[documents.dependencies]` (#132, off by default): a full scan indexes the README, CHANGELOG and `docs/**`
  of the dependencies `Cargo.toml` + `Cargo.lock`, `package.json`, `pyproject.toml` + `uv.lock` /
  `poetry.lock` and `go.mod` name, under `deps/<ecosystem>/<name>@<version>/<file>`, so
  `docs_search {path: "deps/npm/react@*/**"}` filters by package and a hit carries `package` and
  `package_version`. Roots outside the repository need `BASEMIND_ALLOW_DEPENDENCY_ROOTS`; what is not
  indexed is listed under `deps_unavailable` in `index_status` and the scan output. A changed manifest
  or lockfile re-resolves them. The `docs` table gains `package` and `package_version` columns, so the
  retrieval store is rebuilt once (ADR-0049).
- `basemind serve --lead` (Unix, `comms` builds): the lead's own `serve` hosts the
  `basemind-agents` supervisor and answers `agent_launch` / `agent_stop` on its stdio; the daemon,
  the HTTP front-end, a plain `serve` and the CLI never do. Off until the operator's user-level
  `agents.toml` enables it; a repository's `basemind.toml` `[agents]` table is ignored with a
  warning. Launched agents run at depth 1 in a worktree of their own, with every approval denied,
  and are audited per lead (ADR-0040).
- `stale` on `code_read` (a symbol found by parsing a file edited since it was indexed), on `code_outline`
  (outlined from the file as it is) and on a `code_grep` hit in such a file (which then carries no
  `symbol`).
- `skipped_files` on `code_grep`: the files it could not search (deleted, not UTF-8, grown past
  `scan.max_file_bytes`, or leading out of the repository).
- `budgeted` and `omitted` on `git_log {rev}`: the commit's file list is held to `max_tokens`.
- `budgeted`, `truncated_reason: "span_limit"` and a `next_cursor` that continues past the window on
  `git_log {path, symbol}` and on `git_log` with a directory `path`. An empty page of either carries
  `unmatched` while nothing inspected so far defines the symbol or touches the directory.
- `resolution_note: "no resolution facts for this definition"` on `code_refs` with `path`.
- `truncated` / `truncated_reason` on `code_search` (`stale_chunks`) and `docs_search` (`depth`,
  `liveness_depth`), and `truncated_reason` wherever `truncated` stood alone (`code_read`: `size_cap`).
- `git_diff` with a `path` ending in `/` returns the working-tree status under that directory, with
  every untracked file listed on its own and `is_clean` scoped to the directory. A directory that is
  not on disk and has no changed path under it is an error, and so is a directory given without the
  trailing slash.
- `code_search` and `docs_search` keep the ranked list of a query for up to two minutes (16 queries,
  4 MiB), so a later page or a repeated call does not run retrieval and the reranker again. A rescan
  drops the lists.
- One index per repository group (ADR-0028): every worktree and view of a repository shares one Fjall database at
  `cache/groups/<group>/index.fjall`, held by one process under the group's `.lock`, in place of one database (and
  one 64 MiB journal) per view. A view keeps its path-keyed rows under its own id; the grep bloom is shared by
  content, and its sweeps keep any content another view's file map still names. A file map records the group
  index incarnation and its view id; one that does not match is emptied and its view rebuilt, leaving every other
  view in place. A process that does not hold the group's index answers what the blobs hold from the blob store
  and refuses `code_refs` scans (see Changed). Every workspace
  rebuilds its index on the first scan. A view's per-view `index.fjall` is deleted at its first writer open,
  and the daemon's cache sweep deletes it in every workspace whose repository's group index exists, opened since
  the upgrade or not, unless a 0.33 daemon is registered (ADR-0054, amending ADR-0028 rule 11, whose 30-day rule
  is dropped); `legacy_indexes_reaped` and `legacy_index_bytes_freed` in the sweep's report count them. With the
  cache over its budget, budget enforcement deletes every per-view index left, adopted or not, before it evicts
  any workspace, under the same 0.33 check, and `cache stats` reports how many are left on the machine and their
  size (`legacy_indexes`, `legacy_index_bytes`).
- The group sweep (ADR-0041): at the end of a full pass that owes it, the group index deletes the symbol and call
  rows of contents no view's file map names, and the rows of views whose view directory is gone. A view's BM25
  postings are shared by key and written once; a holder row per path names who holds a key, so retiring a
  posting is one lookup per key and a retired or rebuilt view's postings are retired by key, a bounded batch
  at a time, instead of by a sweep of the whole posting list. Postings are packed into one row per term and
  content. The group index is rebuilt once on first open (layout 5).
- `changed` on `code_grep`, `code_symbols`, `find_files` and `code_search` (`--changed` on the CLI, #122,
  #254) scopes the query to the files a change touched: a revision's merge base with `HEAD` against the
  working tree (empty: `HEAD`), as `git diff <rev>...` names a branch's own changes plus the uncommitted
  ones; `A..B` tree to tree; or `A...B`, what `B` changed since it diverged from `A` (`main...HEAD`). A
  `+importers` suffix adds the files that import a changed file directly. The answer carries
  `scope_files`, so a change that touched nothing reads apart from one without a hit. `code_search`
  ranks eight times as deep under a scope and keeps the hits in its files.
- `code_refs` call sites take `changed` (#122) as `code_grep` does; it is refused for relations
  other than `callers`. `code_refs` links HTTP
  calls to routes (#126, ADR-0048): a `METHOD /path` name in any parameter syntax, or `path`
  pinning a route symbol, lists the calls with a literal URL whose method and normalized path match
  that route best (a literal segment before a catch-all, exact, then the longest suffix), graded
  `path_match` and `ambiguous` when several routes tie. `path` pinning a handler adds the routes
  naming it and the calls reaching them. Relation `definition` on such a call returns its route,
  or the tied routes under `ambiguous`.
- An exact `code_refs` name no longer lists HTTP calls whose path ends in it: `GET /users/me` is not
  a call of `me`.
- `basemind sessions` (CLI only, #128, ADR-0049): per-session counts of basemind calls against shell
  searches, reads and edits from the local Claude Code and Codex transcripts of this repository and its
  worktrees, counts and repository paths only. Sessions started in a subdirectory count, and subagents
  (Claude Code's `subagents/` transcripts, Codex's subagent rollouts) are reported on their own and
  attributed to their parent session. Codex rollouts are read in the current format, compressed ones
  included: namespaced MCP calls, code-mode `exec` scripts and their nested tool calls, and exit status
  from the output header. History copied by a fork, a revert, a resume or a subagent is counted once,
  and relative paths are taken from the session's working directory. Sessions of a removed worktree
  are not counted.
- The `PreToolUse` guard steers `git diff` to `git_diff {"path":"…"}` and `git status` to
  `git_diff {}` (#263); a diff of the staged index (`--cached`, `--staged`) or `--no-index` is left
  alone. Its `code_grep` suggestion for a `Grep` call carries that call's `path` (relative to the
  repository root, a directory as `dir/`) and `glob` (a glob without `/` as `**/<glob>`) in `path`.
- The OpenCode plugin checks the `basemind` binary on `PATH` at startup (#240): with none, or one
  that cannot report its version within 5 seconds, it skips the MCP server; with one from another
  minor version it warns. Either way OpenCode shows one message naming the install command
  (`pnpm add -g basemind@<version>`, or `pip install basemind==<version>`).
  count as this repository's.
- Configurable admission of expensive calls (#255): `BASEMIND_DAEMON_HEAVY_CONCURRENCY` (default a
  quarter of the cores, 2 to 4; previously a fixed 2) and `BASEMIND_DAEMON_QUEUE_WAIT_MS` (default
  2000) set how many heavy calls the daemon runs at once and how long a call waits before it is
  answered `server_busy`. Whole-corpus reads (`code_grep`, `code_refs` importers and cycles, and the
  CLI's impact, cycles, deadcode and overview) are a new medium class with their own slots,
  `BASEMIND_DAEMON_MEDIUM_CONCURRENCY` (default half the cores, 2 to 8), so a rescan no longer
  holds them back and they no longer run unbounded. `index_status` reports each class's `limit`,
  `in_use` and `queued` under `admission`.
- `ATTRIBUTIONS.md` lists every model basemind downloads (#294): the default and opt-in embedding,
  reranker and NER presets, the Tesseract `eng` data and the o200k tokenizer, each with its upstream
  model card, mirror revision where one is pinned (the Tesseract `eng` data follows an unpinned
  branch) and licence. All are MIT or Apache-2.0, so no default changes.
- A `code_read` line span of more than 2,000 lines carries a `notice` naming `code_outline` (#127):
  read the file's outline first, then the narrower span needed.
- `basemind code cycles --fail-on-cycle` (#135) exits 1 for CI when the list (after `--path` and
  `--max-size`) holds a cycle. A cycle among Rust modules of one crate, which the compiler accepts,
  fails it only with `--strict`; a report from an index that is not ready exits 1 instead of passing.
- The plugin launcher downloads releases from a mirror when `BASEMIND_RELEASE_BASE_URL` is set
  (#207): `<base>/v<version>/` holds the platform archives and `basemind_<version>_checksums.txt`, as
  a GitHub release does, and the archive is verified against that checksums file before it is
  installed. A mirror is `https://`, `file:///` or a loopback `http://` URL, since plain HTTP would let
  a network attacker replace the archive and its checksums together. With a mirror set the launcher asks GitHub for nothing, and a mirror missing the pinned
  release is named in the error with the files it needs.
- `BASEMIND_OFFLINE=1` turns every download off at runtime (#208): grammars, the Hugging Face embedding,
  reranker and tokenizer models (basemind sets `HF_HUB_OFFLINE` for itself and the daemon it starts),
  and the plugin launcher's binary download. The npm and pip installers run before basemind does and
  still download a binary that is not cached. A missing grammar names the grammars and the cache to
  copy them into, a missing model names its file and repository, and the launcher names the release
  archive, runs a cached binary of the pinned minor when it has one, and points at `BASEMIND_BIN`.
  `docs/network.md` lists every host basemind downloads from, what triggers each download, and the
  variables that redirect it (`BASEMIND_RELEASE_BASE_URL`, `HF_ENDPOINT`,
  `TREE_SITTER_LANGUAGE_PACK_MANIFEST_URL`).
- Large files before they are opened (#127): `index_status` reports `large_files`, the count of code
  files of 2,000 lines or more and the five largest with their line and byte counts, and
  `find_files` `min_lines` (CLI `basemind code find --min-lines`) lists every code file of at least
  that many lines, largest first, as path to line count. The file map records each file's line count.
- Every command's `--help` ends with when to use it, what it returns, one example and what to call
  next (#163); `-h` keeps the one-line summary. `basemind --help` describes what basemind is, and the
  `init`, `memory`, `cache` and `comms status` summaries say what those commands do.
- `basemind comms cleanup [--dry-run] [--older-than <days>]` removes the rendezvous files no daemon
  uses any more: the directories of other protocol generations under the comms directory, and the
  files releases up to 0.33 keep directly in it (`comms.sock`, `daemon.lock`, `daemon.pid`,
  `store.fjall/`, `daemon.log`, `daemon.log.1`, `http.addr`, `last-fatal.json`). A rendezvous goes
  only when it has a `daemon.lock` the command can take and its newest file is older than 30 days;
  one without a `daemon.lock`, or with one that cannot be opened, is left alone. `--older-than 0`
  drops the age test, so the previous release's message store goes as soon as none of its daemons
  is running. A directory is renamed aside under the lock before it is deleted, so a daemon
  starting meanwhile finds its generation whole or absent. The command reads locks and file ages
  only, never contacts or signals a daemon, and never touches the generation of the binary that
  runs it (ADR-0024 rule 7). It also removes the daemon-registry records other generations left
  for a daemon whose directory is gone or whose lock is free; with a reused pid such a record
  counted toward the ceiling of live daemons for good.
- Agent comms: a read says how many messages a sweep removed before the reader read them (#306).
  The sweep keeps the newest 1,000 messages of a thread and 30 days of them whatever each member
  has read, and the inbox used to step over the gap without a word. A marking read, a hook's
  announcing read and the first page of a peek now carry a notice for each thread whose first stored
  message lies past the reader's position, with the count, until a read takes the reader past it;
  a waiting read returns at once with it. History reads and reads by id carry none.

### Changed

- The watcher drops events under excluded paths (`node_modules`, `target`, `.git`, `.basemind`, skipped
  submodules) in the debouncer callback, before they are queued or coalesced, and checks the exclude globs
  before it stats an event path (#286).

- The default cache budget fits the disk (#291). With `BASEMIND_CACHE_BUDGET_MB` unset, each sweep
  holds the cache to half of what it could occupy (its own size plus the free space on its volume),
  at most 20 GiB, instead of a fixed 20 GiB that on a disk with less free never evicted before the
  disk filled. The footprint counts each workspace's git cache, which eviction already removed. A
  set `BASEMIND_CACHE_BUDGET_MB` is enforced as given; `0` still disables enforcement.
  `cache_stats.last_gc.cache_budget_bytes` reports the budget the sweep applied.

- The server instructions, the session-start hook, the rules and the skills describe the agent tools
  without assuming a host feature (#259): they are listed separately and needed only when
  collaborating, instead of "deferred, find them by tool search". On Claude Code, which also shows the
  server instructions, the session-start hook points at them instead of repeating the routing.

- **`[documents] embed_exclude` now defaults to the structured data formats**
  ([#260](https://github.com/Goldziher/basemind/issues/260)): `*.json`, `*.jsonc`, `*.json5`,
  `*.jsonl`, `*.ndjson`, `*.geojson`, `*.yaml`, `*.yml`, `*.toml`, `*.csv`, `*.tsv` and `*.xml`. They are still extracted, chunked and
  keyword-ranked by `docs_search`, and `code_grep` still searches them, but they get no vectors, so a
  default scan embeds prose documents only. Setting the key replaces the list; `embed_exclude = []`
  embeds them as before. The default is part of the embed-policy fingerprint, so the first full scan
  of an existing index deletes their vector rows without re-extracting or re-embedding anything
  (`docs/UPGRADING.md`).

- **BREAKING:** the HTTP front-end refuses a non-loopback `BASEMIND_HTTP_ADDR` unless
  `BASEMIND_ALLOW_HTTP_REMOTE` is also truthy in the daemon's environment (#241). The transport is
  plain HTTP, so off loopback the bearer token crosses the network in cleartext; `BASEMIND_ALLOW_HTTP`
  alone used to bind such an address with only a warning. The refused front-end logs the reason and
  the rest of the daemon keeps running. The warning on a granted remote bind now says what the
  bind does: no TLS, basemind's own Host-header check off, rmcp's loopback `Host` allowlist still on;
  it no longer points to an `allowed_hosts`/`origins` setting basemind does not have.

- **BREAKING:** `path` is the one path filter of `code_grep`, `find_files` and `docs_search` (#252):
  a repo-relative prefix (`src/mcp/`), or a glob over the whole path when it holds `*`
  (`src/**/*.rs`; `*` stays within one segment, `**` crosses `/`). It was a substring on
  `code_grep`, a prefix named `path_prefix` on `find_files` and always a glob on `docs_search`, so
  `mcp/` no longer selects `src/mcp/tools.rs` in `code_grep`: pass `src/mcp/` or `**/mcp/**`.
  `find_files`' `path_prefix` and `basemind code find|files --path-prefix` are renamed `path` /
  `--path`. On `code_grep` and `find_files` a `path` no indexed file passes is refused with the rule it
  was read by (`docs_search` returns no hits), and a value holding `*` that is not a glob is
  refused with an example.

- **BREAKING:** `memory_search` takes `tags`, a list, where it took one `tag` (#252), as
  `memory_save` does: a search or a listing keeps the notes carrying any of them.
  `basemind memory search|list --tag` is repeatable.

- **BREAKING:** every `isError` tool result carries `structuredContent: {"error", "retryable"}`
  next to its unchanged text (#256). `error` is one of `invalid_argument`, `unavailable`,
  `index_unavailable`, `busy`, `cancelled` and `internal`; `retryable` is true for
  `index_unavailable`, `busy` and `cancelled`. The overload refusal says `busy` where it said
  `server_busy`: in its code, at the start of its text and as the message of the JSON-RPC error a
  resource read gets; it keeps `retry_after_ms`. A CLI command that fails prints the error's code
  and message without its JSON `data`.

- Whole-corpus reads cost less (#280). `code_grep` stops reading files once those read hold more
  matches than `limit`: a common pattern reads only the files its page needs, `total` then counts
  those files and `total_is_partial` marks it a lower bound; `next_cursor` pages on as before. The
  import graph behind `code_refs` importers and cycles and the CLI's impact, cycles, deadcode and
  overview is built once per index generation however many calls ask for it at the same time, and
  from its rows as they are read instead of from a copy of all of them.

- **BREAKING:** every boolean `BASEMIND_*` variable reads `1`, `true`, `yes`, `on` as true and `0`,
  `false`, `no`, `off` as false, case-insensitive, and a blank value as unset (#293). Any other value
  is an error naming the variable: the command exits before it runs, the `--documents-*` boolean
  flags and their `BASEMIND_DOCUMENTS_*` variables included, and the status line shows the error for
  `BASEMIND_STATUSLINE_CONTEXT` / `BASEMIND_STATUSLINE_VERSION`. A misspelt grant such as
  `BASEMIND_ALLOW_HTTP=enabled` used to read silently as false. `BASEMIND_ALLOW_EXTRA_ROOTS` and
  `BASEMIND_ALLOW_DEPENDENCY_ROOTS` still take a list of workspace roots in place of a boolean.

- A usage error a tool raises through the CLI (#262) names the command and its flags as typed:
  `` `basemind code grep`: `--limit` must be at least 1 `` instead of `` code_grep: `code_grep`: `limit` … ``.
  Busy errors exit `3` because the tool marks them busy, no longer because their text says "index
  not available".

- **BREAKING:** the MCP text block of `code_grep`, `code_outline`, `git_diff` and a `find_files`
  list is no longer JSON (#157, ADR-0053, amending ADR-0034 and ADR-0052). `code_grep` writes its fields
  as `key: value` lines, then `# <path>` and `## <symbol>` headers over its rows, as `rg --heading`
  groups them; the other three are [TOON](https://toonformat.dev), which writes a list of objects as
  one table. The `basemind://outline/{path}` resource mirrors its tool: TOON, served as `text/toon`.
  Every other tool keeps JSON, which TOON did not shorten on the value-gate corpora;
  `structuredContent` and `--json` stay JSON everywhere. Median answer over every task of the kind,
  as `gate.md` prints it, JSON to now (basemind / tokio): `grep` 44 / 58 to 39 / 53 tokens,
  `grep_changed` 50 / 79 to 47 / 74, `read_around` 366 / 249 to 340 / 231 (1.31x / 1.52x to 1.43x /
  1.61x against `rg -n -C5`), `outline` 875 / 500 to 669 / 359, `diff` 401 / 219 to 381 / 209,
  `diff_structural` 133 / 84 to 114 / 74, `find` 12 / 14 to 11 / 13, with recall unchanged.
  `max_tokens` charges what each tool writes. `admin eval` scores the text block
  (`basemind::wire::decode`); an answer it cannot read is a task error. The `toon-format` crate
  (MIT, without its CLI feature) encodes and decodes TOON.

- **BREAKING:** `code_symbols`, `code_grep`, `find_files`, `git_log`, `git_blame` and `code_refs`
  (importers) answer with rows as the shell prints them (ADR-0052, amending ADR-0034): a `code_symbols`
  row is `"<path>:<line>: <signature>"`, `code_grep` hits are
  `{"<path>": {"<symbol>": ["<line>: <text>"]}}` with context lines as `"<line>- <text>"`, a
  `git_log` commit is `"<sha12> <summary>"` (`"<sha12> <author>: <summary>"` in a search by `author`)
  with the recent and directory lists' files in one `files` map, and `git_blame` hunks are
  `"<line>[-<end_line>] <sha12>"` with each commit's `author`, `time` and `summary` once in `commits`.
  `total` appears only beside `next_cursor`, `find_files` with `min_lines` maps path to lines, and
  `in_cycle` on importers is a count, with `deeper: true` when importers lie past `depth`. On the
  value-gate corpora the median answer of each changed kind fell by 13% to 58% (basemind / tokio):
  `dependents` 21 / 31 to 17 / 27 tokens, `symbols` 91 / 155 to 46 / 94, `grep` 56 / 83 to 44 / 58,
  `git_path` 566 / 265 to 286 / 138, `git_symbol` 73 / 72 to 32 / 30, `blame_symbol` 162 / 175 to
  81 / 84, `find_large` 162 / 156 to 93 / 87, with recall unchanged; `callers`, `references`,
  `implementations` and `cycles` did not change. `find` stays at 0.67x on basemind, just under the
  locating floor (ADR-0052). The mapping from every removed field is in
  [docs/UPGRADING.md](docs/UPGRADING.md#lean-rows).

- **BREAKING:** `code_grep` `context` is a line count, 0 to 10, where it was a boolean for one line
  (`basemind code grep --context N`). The N lines before and after each hit are rows
  `"<line>- <text>"`; where windows overlap each line is written once, as `rg -C N` prints it, and a
  window stops at the file's last line. The token budget charges each context row once. `context:
  true` is refused naming the field; pass `1`. The `read_around` eval kind is now this one call
  (`context: 5`) instead of `code_grep {ranges}` and a `code_read` per file, and counts against
  `code_grep`: its median answer fell from 385 / 270 to 366 / 249 tokens (basemind / tokio), 1.26x /
  1.35x to 1.31x / 1.52x against `rg -C5`.

- `admin eval`'s value gate holds a locating kind to a median ratio of exactly 2/3, ADR-0031's "at
  most one and a half times the shell's", where it used 0.67 and failed an answer at exactly 1.5x
  (basemind `find`, 12 tokens against 8, read `median 0.67x < 0.67x`). A failed ratio is printed with
  the digits that tell it from its floor, and the report's `build.features` names `code-intel` for a
  build with both its parts.

- `admin eval`'s value gate holds `read_around` to a median ratio of 1.4 instead of the 1.5 of the other
  reducing kinds: its rows keep their `## <symbol>` headers (ADR-0055).

- **BREAKING:** every `code_refs` call site carries a `resolution` (#125, ADR-0047): `same_file`,
  `imported`, `scope_resolved`, `elsewhere`, `name_only` or `external`, in place of the `resolved`
  flag. A row leaves out the page's `resolution_default`; `resolution_counts` gives the rows per
  value when they differ, and the `resolution` argument keeps the values asked for. The free-text
  `resolution` on callers and on relation `definition` is now `resolution_note`.

- `code_symbols {"name":""}` is refused, as an empty `code_grep` pattern is, instead of answering an
  empty list.

- **BREAKING (library):** `basemind::scanner::scan`, `scan_paths` and the other public scan entries
  take the `Retrieval` binding as an argument; a library caller passes
  `basemind::retrieval_wiring::backend()`.

- **BREAKING (library):** the library no longer reads `BASEMIND_DATA_HOME`. The data home is
  `basemind::engine::EngineConfig::data_root` (a `basemind::data_root::DataRoot`), installed by
  `basemind::engine::Engine::new`; `basemind::cli::env_config::install_from_env()` builds it from the
  environment as the binary does, and a process that installs none uses the platform data dir.
  `basemind::store::cache_root()` is removed (use `DataRoot::current()`), and the machine-wide GC,
  cache-stats and registry entries (`store_gc::{gc_global_blobs, run_gc, gc_report_only,
  reap_gc_and_enforce_budget, reclaim_displaced_blobs, cache_stats, clear_component, read_gc_state}`
  and the rest of that family, `scan_evidence::stale_inflight_scans`,
  `registry::Registry::from_data_home`) take a `&DataRoot`. The variable keeps its meaning for the CLI
  and the daemon, except that a relative value is now resolved against the current directory when the
  process starts, so a daemon it spawns (which runs in the comms dir) uses the same home. The
  `basemind::engine` and `basemind::data_root` modules are public, and `Store` has a public
  `data_root` field.

- **BREAKING (library):** a process has one engine configuration (ADR-0050). A second
  `Engine::new` with the installed configuration returns a handle on it; one with any other
  configuration, or another data home, is refused with `EngineError::Unavailable`. Data homes are
  compared as directories, so a symlinked spelling is the same home.

- The comms daemon a process spawns, and an agent `serve --lead` launches
  (`basemind_agents::LaunchConfig::data_home`), get the process's installed data home in
  `BASEMIND_DATA_HOME` rather than inheriting the variable.

- Downloaded tree-sitter grammars are cached under the data home (`<data home>/grammars`) instead of
  the platform cache dir; `TREE_SITTER_LANGUAGE_PACK_CACHE_DIR` still moves them, and the grammars are
  downloaded again once after the upgrade.

- `mcp::serve_in_memory` refuses to run when `BASEMIND_DATA_HOME` names another data home than the
  one the process installed.

- **BREAKING (library):** the operator grants (`BASEMIND_ALLOW_REPO_CREDENTIALS`,
  `BASEMIND_ALLOW_ANY_ROOT`, `BASEMIND_ALLOW_FOLLOW_SYMLINKS`, `BASEMIND_ALLOW_EXTRA_ROOTS`) are
  `EngineConfig::trust`, which the library reads instead of the environment.
  `basemind::scanner::allow_extra_roots` is removed (set `trust.extra_roots`), and
  `config::trust::Grants::from_env` is `Grants::installed`. A set variable of a removed setting, an
  unparseable `BASEMIND_CACHE_BUDGET_MB` or `BASEMIND_CACHE_GRACE_MINUTES`, and a relative
  `BASEMIND_ALLOW_EXTRA_ROOTS` entry are now reported once when the process starts, not when a
  `basemind.toml` is loaded or a sweep reads the value; `basemind statusline` and `basemind hook`
  do not report them.

- **BREAKING (library):** `BASEMIND_PARSE_TIMEOUT_MS`, `BASEMIND_GRAMMAR_OFFLINE`,
  `BASEMIND_CACHE_BUDGET_MB`, `BASEMIND_CACHE_GRACE_MINUTES`, `BASEMIND_BLOB_GC_GRACE_SECS`,
  `BASEMIND_MIN_FREE_MB` and `BASEMIND_MAX_DAEMONS` are `EngineConfig::{parse_timeout, grammars, gc,
  min_free_bytes, max_live_daemons}`; the library reads the configuration, and the parse-timeout cache
  static is gone.

- **BREAKING (library):** the git, index and grep knobs (`BASEMIND_GIT_DELTA_CACHE_BYTES`,
  `BASEMIND_BLAME_MAX_BYTES`, `BASEMIND_BLAME_MAX_LINES`, `BASEMIND_GIT_CACHE_LOG_MAX_BYTES`,
  `BASEMIND_GIT_DISCOVER_PARENTS`, `BASEMIND_GH_INDEX`, `BASEMIND_INDEX_CACHE_BYTES`,
  `BASEMIND_GREP_BLOOM`) are `EngineConfig::{git, index_cache_bytes, grep_bloom}`, and the
  `BASEMIND_DAEMON_MAX_*` ceilings are `EngineConfig::daemon_caps` (`config::daemon::DaemonCaps`).
  The library reads none of them from the environment.

- The tokio runtimes below the CLI are built in one module, `basemind::lance::runtime`: document
  extraction's (`extraction()`, 2 to 4 workers, a blocking pool capped at 256, was 16, so 64
  abandoned and 64 running extractions each keep two threads) and one current-thread runtime per
  vector store (`store()`, a blocking pool capped at 64, was tokio's 512). The two never share a
  blocking pool, so a hung document parse cannot hold up a search. Dropping a store shuts its
  runtime down, ending the tasks lance spawned for it before its directory can be removed.

- A panic while a verb tool runs (an `index_rescan` scan, say) comes back as an internal-error
  result reading `tool task failed: …`, where it read `scan join: …` or
  `host rescan task panicked: …`.

- A one-shot `basemind scan` gives each ONNX session the scan's cores minus the embed pool's other
  workers as intra-op threads (3 on 4 cores, was 2), and a set `[resources] embed_threads` now sets
  them too. The daemon keeps 2 and `serve` keeps `max(2, cores / 4)`: they share the machine.

- **BREAKING: `docs_search` is rebuilt on one document store per repository group (ADR-0029,
  ADR-0037).**
  - A hit carries `chunk`, `cite` (pages, section, headings inside the hit, lines, bytes), `context`,
    `also_at` and `title`; the response reports `lanes_ran`, `lanes_unavailable`, `lexical_fuzzy`,
    `vector_mode` and `rerank`. Per-hit `keywords`, `entities` and `summary`, `chunk_idx`, `distance`
    and `byte_start` / `byte_end` are gone.
  - `scope` is removed. `format` and `path` are new filters, `keywords` matches whole keywords, and every
    filter runs before ranking.
  - A vector lane and a full-text lane are fused; a search works without the embedder, on the full-text
    lane, and says so.
  - Plain text, logs, CSV, TSV, NDJSON, JSON, YAML and TOML are chunked from the file itself, so every
    chunk cites its lines.
  - The rows live at `cache/groups/<group>/lance/<model>-<dim>-<hash>/`, shared by every worktree of
    the repository. The first scan deletes each view's old document store and re-extracts and
    re-embeds every document once (extraction digest `doc/4`, vectors keyed by model).

- `code_refs` call sites, `callers` (with `path`) and `implementations` in a session that holds no
  index — a CLI command or a second `serve` beside a writer, with no daemon — are refused as busy
  (exit `3`), naming `basemind comms start`, instead of answering from an in-RAM projection of the
  blobs that could be truncated. The `projections_capped` notice is gone with it. `code_grep` still
  sweeps every file there, and relation `definition` still reads the intra-file facts.

- A daemon-hosted session reads the daemon's index directly: its store borrows the pool writer's
  index handle per call, so reads are not held up by a scan, and it shares the daemon's git-history handle.

- `cache gc` removes the whole `cache/groups/<group>/` directory, index included, of a group no workspace names
  (memory notes under `cache/repos/` stay); `group_stores_reaped` in its report counts those directories. A group
  that cannot be removed is logged and kept, and the rest of the GC still runs.

- With the comms daemon up, a CLI tool command on the working view forwards its whole call to the
  daemon over one relay session (a `cli` client in the relay hello) instead of opening the store
  itself; the output and exit codes are unchanged. Against a daemon too old to take the call it exits
  `4` and names `basemind comms stop` (ADR-0039).

- A CLI command beside another process that holds the workspace's index, with no daemon up, refuses
  `code_search`'s keyword lane, and a hybrid search none of whose lanes could run, as busy (exit `3`)
  naming `basemind comms start`, where it was an internal error (exit `1`). It no longer starts a
  daemon to read the lanes. It also no longer retries the git-history database such a process holds
  (about 1.5 s per command): history tools live-walk at once.

- A CLI or hook resolving its agent id no longer creates a workspace cache directory that holds only that id: the
  id is derived from the workspace's cache path. A workspace whose id an older basemind persisted keeps it.

- **BREAKING: the MCP tools are one per job.** The mode-dispatched `code`, `git`, `memory` and `admin`
  tools are replaced by 15 tools with exact schemas (ADR-0018, superseding ADR-0011): `code_outline`,
  `code_symbols`, `code_read`, `code_grep`, `code_search`, `code_refs`, `find_files`, `docs_search`,
  `git_log`, `git_blame`, `git_diff`, `memory_save`, `memory_search`, `index_status` and `index_rescan`.
  There are no aliases: the old tool names and the `mode` argument are gone. The mode-dispatched
  `agents` tool is replaced separately (below). The plugin's skills, commands, hooks and rules are
  updated in the same release.
  [`docs/UPGRADING.md`](docs/UPGRADING.md) lists the renamed arguments and CLI flags.

  <!-- markdownlint-disable MD013 -->

  | Old tool and mode | New tool |
  |---|---|
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

  <!-- markdownlint-enable MD013 -->

  Contract changes:

  - The 15 tools are always loaded (`_meta["anthropic/alwaysLoad"]: true`), so a host that defers MCP
    tools needs no tool-search round trip; the agent tools are not marked and stay deferred
    (ADR-0021). `tools/list` for the 15 is about 19 KB (about 4.7k tokens), down from about 25 KB (about
    6.3k tokens) for the four tools they replace; a test enforces a 20,000-byte ceiling. No tool declares
    an output schema.
  - Required arguments are required in the schema, and an unknown argument is rejected with a did-you-mean
    suggestion and the accepted list. A stringified integer or boolean (`"limit": "10"`) is coerced; any
    other type error names the field (and the element, for a list), and every problem with a call is
    reported in one message. An integer written as a float (`10.0`) is accepted. The
    hidden aliases (`q`, `needle`, `token_budget`) and the `format` argument (`"toon"`) are gone, and
    `[documents.output] format = "toon"` no longer applies to these tools.
  - `code_grep` treats `pattern` as literal text unless `regex: true`; `path` replaces `path_contains`,
    and `context` (default off) replaces `include_context`. A hit on
    a line over 400 bytes shows a window around the match.
  - A path argument is read from the working tree only when it is indexed and stays inside the
    repository: an absolute path, a `..` component and a symlink pointing outside are refused. With
    `[scan] follow_symlinks` granted, a link the scan indexed is read by every tool, as `code_grep`
    reads it. `git_diff` between two revisions reads only git objects, so it diffs a tracked symlink
    as the target path git stores.
  - A cursor is bound to the call that returned it. Passed to another query, or used after a rescan or
    a new commit, it is an error telling the caller to repeat the call without `cursor`; no tool
    answers a stale cursor with an empty page (`cursor_invalidated` is gone).
  - Every position is 1-based: `line`, `end_line` and `column`, in arguments and responses. The 0-based
    `start_row` is gone.
  - `code_refs` matches the whole identifier; `match: "substring"` restores the previous matching.
    `relation: "importers"` matches whole segments of an import (`os` is not `posixpath`, `react` is not
    `react-dom`). With `path`, a
    file that is not indexed or defines no such symbol is an error, and a name several definitions in
    the file share is an error listing each with its lines until `line` picks one; `code_read`,
    `git_blame` and `git_log` with `symbol` take the same `line`. `resolution` tells a language without
    resolution (`unsupported for <language>`) from a file that bound nothing
    (`no resolution facts for this file`) and from an index whose resolve pass did not finish
    (``resolution has not finished for this index; a running scan will complete it, otherwise run
    `index_rescan` ``). An exact scan stops after examining 200,000 index keys, sets `truncated` with
    `truncated_reason: "scan_cap"` and returns a `next_cursor` that continues it.
  - Every list takes `limit` and `cursor`, returns `next_cursor`, and honours a default `max_tokens` budget
    of 6000, setting `budgeted` when it trims.
  - A failure the caller can fix is a tool result with `isError: true` that names the fix, not a JSON-RPC
    error; an empty index is reported with a `notice` (`state: "empty_index"`).
  - Results are inline. `index_rescan`, `code_search` and `docs_search` become a server-side Task only when
    still running after 3 s.
  - `git_diff` with no arguments returns the working-tree status, and with only `path` diffs that file's
    working tree against HEAD; `from` / `to` replace `rev_old` / `rev_new`. A diff is cut at 200 hunks or
    128 KiB of hunk text (a single larger hunk is clipped to that) and then sets `truncated`,
    `truncated_reason` and `omitted`; a binary file reports `binary`, and a file over 4 MiB on either side is not diffed
    and reports `too_large`.
    `git_blame` takes `line` / `end_line` in place of `line_start` / `line_end`, returns 100 hunks per
    page by default and honours `max_tokens`.
  - `git_log` matches `query` against the commit message and `author` against the author, each on its
    own field. A directory `path` inspects 200 commits per page and returns a `next_cursor` to the
    next window while history continues. A search answered before the git-history index is ready
    sets `truncated` with `truncated_reason: "unindexed_scan"` (subjects and authors of the newest 10,000
    commits only) in place of `partial`.
  - `memory_save` reports `embedded`. When the note cannot be embedded the save still succeeds, with a
    `notice`, instead of answering with an error after the write.
  - A call rejected because the server is at its limit of heavy calls is an `isError` result
    (`busy`) that says to retry, not a JSON-RPC error.
  - `docs_search` takes `entity` and `keywords` in place of `entity_category` and `keywords_contains`.
  - Annotations are exact per tool: everything is read-only except `memory_save` and `index_rescan`, and
    nothing is destructive.
  - Telemetry keys are the tool names.
  - CLI: `code read`, `code search`, `code refs`, `git log` and a path-less `git diff` mirror the tools, and
    the previous commands remain as shorthands. `code grep` is literal unless `--regex`; `code expand <path>
    <name>` is `code read <path> --symbol <name>`; `git diff <path> <old> <new>` is `git diff <path> --from
    <old> --to <new>`; `code definition` takes `<path> <line> <name>`. The flags `--format`, `--l2`,
    `--path-contains`, `--no-context`, `--line-start` / `--line-end`, `--field`, `--body-chars`,
    `--hash-mode`, `--no-embed` and `--no-files` are removed. CLI parity is now "every MCP tool has a
    command" (ADR-0014 as amended): churn, changed-path regex search, chunk fetch, memory deletion, cache
    maintenance and telemetry are CLI only.

- **BREAKING: the verb tools refuse calls that can only come back empty, bound what they return, and say
  every cut in one vocabulary.** These change responses and behaviour of the tools above.
  - One vocabulary for a cut. A page that ends at `limit` is paging: `next_cursor` says there is more
    and nothing else is set. A page trimmed to the token budget also sets `budgeted`. A cut the caller
    did not ask for sets `truncated: true` and `truncated_reason`, with `omitted` counting the items
    left out where the count is known. `code_grep`'s `truncation_reason` is renamed `truncated_reason`
    and is only ever `byte_budget`; `code_symbols` and `code_grep` no longer set `truncated` on a
    `limit` cut and `find_files` has no `truncated`; `total_is_partial` (`code_symbols`, `code_refs`)
    and `git_log {query}`'s `partial` are `truncated` with a reason (`scan_cap`, `unindexed_scan`);
    `git_diff`'s `total_hunks` is replaced by `truncated_reason` (`hunk_cap`, `byte_cap`) and `omitted`;
    `code_outline`'s `imports_truncated` is replaced by `truncated`, `truncated_reason: "imports"` and
    `omitted`; `code_read` counts the bytes it left out in `omitted_bytes`. Four fields keep their own
    names because they say something else: `skipped_files` (files that could not be searched),
    `limit_clamped` (`find_files` lowered an over-large `limit`), `body_truncated` (one commit's
    message in a `git_log` search hit) and `too_large` (`git_diff` did not diff a file over 4 MiB).
    A `memory_search` listing no longer sets `truncated` on a `limit` cut; it sets `truncated` and
    `truncated_reason: "scan_cap"` when its `total` stopped counting at the scan cap.
  - `git_diff {structural}`: `common` is a count, not a list. Two definitions sharing a name and kind are
    counted once.
  - `git_diff {}`: each status list is cut at 200 paths, in path order; `omitted` counts the rest and the
    cut sets `truncated` with `truncated_reason: "path_cap"`. A directory `path` narrows the status and
    lists every untracked file under it, whatever `status.showUntrackedFiles` collapses.
  - `code_grep`, `find_files`: a `path` / `language` filter that no indexed file passes is
    an error saying what would match; `code_grep` names `docs_search` when the filter matches only
    documents. While the index is still being built the empty page is returned with its `notice` instead.
  - `git_log {path}`: a path that no commit reachable from HEAD touches and that is not on disk is an
    error, for a file and for a directory. An empty indexed answer is checked against the live walk first.
  - `limit: 0` is refused by every tool that takes `limit`.
  - `code_grep {regex}`: `^` / `$` match at every line, CRLF-aware, and no hit is reported past the last
    line. A line ends at `\n` (a `\r` before it is part of the ending), as in `code_read`; a `\r` on its
    own ends no line and no anchor matches at it.
  - `git_blame {symbol}`: the span, and the `line` that picks one of several definitions, are taken from
    the blamed revision. An error names that revision, and a blob over `scan.max_file_bytes` is refused.
  - `code_read`: an oversized body ends at a whole line and `end_line` is the last line returned (read on
    from `end_line + 1`); a symbol read refuses a file over `scan.max_file_bytes`.
  - `code_search` / `docs_search`: every page is cut from one retrieval of 101 hits (the 100 that can be
    paged and one that says whether the ranking goes on), and a cursor issued before a rescan is refused.
    A `docs_search` ranking that goes on past that depth sets `truncated_reason: "depth"`. Its filters run
    before ranking, and its reranker reorders the first `[documents.reranker] top_k` fused hits (at most
    50) and drops none (ADR-0029, ADR-0037).
  - `git_log {rev}` and `{path, symbol}` now honour `max_tokens`. A symbol's history is paged by a cursor
    over the commits touching the file, so a change older than the first window can be reached, and a
    symbol the working tree no longer defines has its history read. A name no commit defines either is
    said on each empty page (`unmatched`, with the names the file does define) and is an error on the
    last one.
  - A file edited since it was indexed: `code_outline` and `code_read` describe it as it is now and set
    `stale`; `code_refs` with `path`, and `relation: "definition"`, return an error naming `index_rescan`.
    A file deleted since the scan gets one error from every tool, naming `index_rescan`. No tool reads an
    indexed file whole past `scan.max_file_bytes`.

- **The `documents` cargo feature is split into tiers.** `documents` is now document search over local
  files without OCR or named-entity recognition: PDF (text layer), Office formats, spreadsheets, email,
  HTML, XML (SVG text is read as XML), Markdown/MDX and Jupyter notebooks, with chunking, language
  detection, quality scoring, keywords, extractive summaries, the cross-encoder reranker and embeddings.
  `documents-ocr` adds Tesseract OCR for raster images and PDFs with no text layer, `documents-ner` adds
  ONNX named-entity recognition (`[documents.ner]` and the `docs_search` `entity` filter), and
  `documents-ocr-paddle` adds PaddleOCR as an optional fallback stage. `documents-heic` adds
  HEIC/HEIF photos to OCR, decoded through a dynamically linked libheif with the libde265 HEVC
  decoder (libheif 1.19 or newer). `full` is now `documents`, `documents-ocr`, `documents-heic`,
  `documents-ner`, `memory`, `code-search`, `comms` and `code-intel`, so the published binaries
  keep OCR, HEIC and NER. A source build that used `--features documents` needs
  `--features "documents documents-ocr documents-ner"` (or `full`) to keep both. Without `documents-ocr`,
  raster images are not indexed as documents and a PDF with no text layer is indexed with no text, with a
  warning that names the file and the feature. Without
  `documents-ner`, `[documents.ner] enabled = true` logs a warning and has no effect, and a `docs_search`
  call with `entity` returns an error naming the feature instead of an empty result. `memory`
  no longer compiles keyword extraction or NER code, which it never used.
  A build with both tiers keeps its cached documents. A build missing a tier stamps its documents as
  such, so its first scan extracts every document again, and so does the first scan after switching
  to a binary with a different set of tiers.
  The `full` build drops 67 locked packages (1204 to 1137) and 76 compiled crates (990 to 914); on
  aarch64-apple-darwin the release binary goes from 315.2 MB to 290.2 MB, its `__text` section from
  129.3 MB to 122.0 MB.

- `xberg` 1.4.0 is now consumed from crates.io instead of a git revision. The lockfile package
  count is unchanged (1137); `xberg` was the last git dependency in the manifest.

- **BREAKING: agent comms v2 (ADR-0021).** The mode-dispatched `agents` MCP tool (18 modes) and the
  `basemind agents` CLI group are removed, with no aliases. Three deferred tools replace them (found by
  tool search; the session-start hook and the server instructions name them): `agent_post`,
  `agent_inbox` and `agent_threads`, each with an optional `as` for a sub-identity. The CLI is
  `basemind agent post|inbox|threads`, plus `basemind agent status` and `basemind agent cleanup`.
  Reading now marks read (`peek: true` does not), bodies come inline (4 KiB, `id` reads one whole),
  a thread is open to its repository or invitation-only, and membership belongs to the principal.
  The comms protocol version goes from 4 (0.33.0) to 7 and the relay protocol from 1 to 2, in one
  step with the protocol changes listed below: a client and a daemon of different versions refuse
  each other at connect, so restart every session and run
  `basemind comms stop`. The agent store moves to `agents.fjall` in the protocol generation's
  directory and starts empty: threads and messages of earlier releases are not carried over. The
  legacy `store.fjall` is left in place and never read; `basemind comms cleanup` removes the
  pre-generation one. Messages are kept 30 days (at most 1,000 per thread) where idle threads used to
  archive after three days. The plugin launchers set `BASEMIND_HOST` (`claude`, `cursor`, `copilot`,
  `gemini`, `codex`), the `inbox-notify` hook announces unread messages with the daemon's own mark
  instead of a per-session file, and a new `PreToolUse` hook gives each session and subagent its own
  `as` on Claude Code and Codex. [`docs/UPGRADING.md`](docs/UPGRADING.md) has the upgrade steps;
  the spec is [`docs/comms-v2.md`](docs/comms-v2.md).

  <!-- markdownlint-disable MD013 -->

  | Old `agents` mode | New |
  |---|---|
  | `register`, `list` | removed: identity is resolved, not registered; `basemind agent status` shows yours |
  | `thread_start` | `agent_post {subject, body, to?, paths?}` |
  | `thread_list` | `agent_threads {query?, path?}` |
  | `join` / `leave` | `agent_threads {thread, join: true}` / `{thread, leave: true}` |
  | `members` | `agent_threads {thread}` |
  | `add_member` | `agent_threads {thread, invite: [ids]}` |
  | `remove_member` | removed: no member can remove another; a member can `leave` |
  | `archive` | `agent_threads {thread, archive: true}` (creator; `basemind agent cleanup --archive` for an operator) |
  | `post` | `agent_post {body, thread}` / `{body, reply_to}` |
  | `history` | `agent_inbox {thread, unread: false}` |
  | `message` | `agent_inbox {id}` |
  | `inbox` | `agent_inbox {}` (`peek: true` to read without marking) |
  | `ack` | removed: reading marks read |
  | `wait` | `agent_inbox {wait_secs}` (0 to 40) |
  | `status`, `cleanup` | CLI only: `basemind agent status`, `basemind agent cleanup` |

  <!-- markdownlint-enable MD013 -->

  Removed arguments: `as_agent` (now `as`, `[a-z0-9_-]{1,32}`, acting as `<principal>.<name>`),
  `idempotency_key` (the relay replays with its own key), `since_hours`, `mark_read` and `message_ref`.
  A `BASEMIND_AGENT_ID` or `comms.agent_id` containing `.` is no longer valid and falls through with a
  notice. A thread needs a `subject`, not two of subject, path-glob and members.

  Operator-visible: `basemind comms status` prints `agent_store=…` where it printed
  `threads=… subscribers=…`, and `basemind comms doctor --json` reports `"agent_store"` where it
  reported `"threads"`. The agent tools are not listed over the HTTP front-end. The `PreToolUse` hook injects a session `as` only on Claude Code
  and Codex; on other hosts the hooks act as the principal, like the tool calls.

- **BREAKING: `basemind init` without a terminal writes only with `--yes`.** It used to append to
  `.gitignore` and write `basemind.toml` whenever stdin was not a TTY; it now prints what it would
  change. `/bm-init` passes `--yes` once the user has chosen.

- `basemind init --settings-target` lists each read-only tool in `permissions.allow`, under
  `mcp__plugin_basemind_basemind__` and `mcp__basemind__`, instead of the
  `mcp__plugin_basemind_basemind__*` wildcard, which also approved `memory_save`, `index_rescan` and
  the agent tools. It removes that wildcard and entries for retired tools.

- The plugin ships `scripts/statusline.sh`; `/bm-statusline` runs the installed plugin's copy
  instead of the marketplace clone's.

- A GitHub release's notes are its version's `CHANGELOG.md` section (`scripts/changelog_section.py`),
  and a release whose version has no section, or an empty one, stops before anything is built.

- A `plugins-only` publish dispatch runs only from the release tag that names the plugin version.

- cargo-deny checks the `full` dependency graph the release binaries are built from.

- Packaging a release fails, naming each one, when a native library outside the per-platform
  allowlist in `scripts/check-bundled-libs.sh` would be bundled (#231). The archives no longer carry
  libaom, libvmaf, libsharpyuv or x265; the allowlist names only libheif and libde265 (below) and, on
  Linux, `libstdc++.so.6`.

- Release archives bundle libheif 1.23.7 and libde265 1.1.3 as shared libraries (LGPL-3.0-only),
  built with the libde265 decoder and no other codec, and no longer bundle x265 (a GPL HEVC
  encoder), libaom, libvmaf or libsharpyuv. libheif is built from its pinned release on Linux and
  macOS (`scripts/build-libheif.sh`) and comes from a pinned vcpkg with the dynamic `x64-windows`
  triplet on Windows, whose archive ships `heif.dll` and `libde265.dll` beside `basemind.exe`. The
  Linux archives also ship `libstdc++.so.6`. Packaging a release fails, naming each one, when a
  library outside the per-platform allowlist in `scripts/check-bundled-libs.sh` would be bundled,
  or when x265 would ship: a library named for it, or x265's banner or libheif's x265 plugin name
  in the binary or a bundled library.

- A watcher batch patches the in-RAM file view, term index and document paths instead of copying
  them (#279). They are sorted maps of shared pages of about 256 entries, so the map a batch derives
  shares every page the batch did not touch, and one saved file in a 100k-file workspace rebuilds
  one page of each rather than allocating every entry again.

- `basemind init` says, when it writes the rules to `AGENTS.local.md`, that no agent host loads that
  file by itself and that OpenCode reads it once `~/.config/opencode/opencode.json` lists it under
  `instructions` (#329).

- Every release archive, and so the npm and pip installs that unpack it, carries
  `THIRD_PARTY_LICENSES`: the licence text of every Rust crate in the binary, with its crates.io
  source, generated by cargo-about from `Cargo.lock` over the `full` feature set. Each bundled
  library ships its licence text as `licenses/<library>.txt` (libheif's and libde265's LGPL-3.0, with
  their pinned source and how to relink basemind against a modified copy; in the Linux archives
  also libstdc++'s GPL-3.0 with the GCC Runtime Library Exception) and has an `ATTRIBUTIONS.md`
  entry; the allowlist check fails for an entry without both, and the archive smoke test fails when
  a notice is missing.

- Every release archive also carries the licence texts of the native code linked into the binary,
  which cargo metadata does not see: ONNX Runtime 1.31.0 (`licenses/onnxruntime.txt` and its
  `ThirdPartyNotices.txt`), Tesseract 5.5.4 and Leptonica 1.87.0. `ATTRIBUTIONS.md` covers each,
  and the Tesseract models xberg downloads on first use. CI renders `THIRD_PARTY_LICENSES` on every
  pull request, and `scripts/check_native_notices.py` fails when `Cargo.lock` moves `ort-sys`,
  `xberg-tesseract` or `tree-sitter-language-pack` off the version `ATTRIBUTIONS.md` was checked
  against.

- Every published crate declares `rust-version = "1.100.0"`, so `cargo install basemind` on an older
  Rust stops with that minimum instead of a dependency's compile error. CI checks the crates with
  exactly that release, and nightly builds `full` against the newest compatible dependencies, as an
  install without `--locked` resolves them (#275).

- The hardening soak waives the daemon's memory climb until lsm-tree evicts the blocks of deleted
  tables (#200, #164), within a stated bound: the 296 MiB of Fjall block cache the daemon holds for
  one workspace, spread over the judged load minutes (16.5 MiB/min over 18 minutes, 6.9 MiB/min
  over 43). `analyze.py` fails R2 or R2f on a climb above it, marks a waived rule `waived` in its
  report and the CI summary, and `--lsm-cache-mib 0` lifts the waiver. On the soak's generated
  fixture the daemon climbed +4.7 MiB/min (RSS, Linux), +4.8 MiB/min (footprint, macOS) and
  +4.1 MiB/min (footprint, Windows) over 18 minutes, each within the bound. The soak also records
  the daemon's descriptors by kind (`fd_kinds`), and R3t lists the kinds that moved over load.

- The hardening harness compares each run with committed per-machine baselines
  (`benchmarks/harden-baselines.json`, `scripts/harden_baseline.py`; a regression above 20% warns,
  `BASEMIND_HARDEN_BASELINE_STRICT=1` fails), samples the same file and symbol on every run and
  records them, and calls `code_refs` on a very common name on purpose (#159, #445).

- The value gate's tooling: the `x` column prints four digits, a build's reported features are mapped to
  its build flags, the gate refuses to count tokens as `o200k` unless the tokenizer proved itself on a
  fixed probe, `dependents` has a gold-recall floor independent of the baseline, `docs-eval` fails under a
  recall@10 floor, and the committed `run.tsv` header is tested (#158). `code_grep` `read_around` has its
  own reducing floor of 1.4x (ADR-0055).

- The README demos are re-recorded on the verb tools and the agent CLI, driven by `scripts/demo-record.sh`
  (vhs); the semantic and code-review demos are gone, a comms demo replaces them (#235).

### Removed

- **BREAKING: `ranges` on `code_grep`, `code_refs` and `code_read`** (#129), and the CLI's
  `code grep --ranges`, `code refs --ranges` and `code read --ranges`. `code_grep {context: N}`
  returns the N lines around each hit in one call, which is what the span list was read for; a
  call that still passes `ranges` is refused as an unknown argument (ADR-0053). `code migration
  check --ranges` stays.
- `BASEMIND_MCP_LEAN` and its three wrapper tools (`list_tools`, `get_tool_schema`,
  `invoke_tool`). The verb tools are the surface (ADR-0018); the wrapper path skipped the task
  offload and cancel handling of a direct call and advertised no `destructiveHint`.
- The `store_error` exception of `basemind comms stop` and the takeover, and the `last-fatal.json`
  record `basemind comms doctor` reported. No daemon of this generation answers `store_error` or
  writes the record: a broken agent store shows as `agent_store` in `Status`. `--clear-fatal` now
  acknowledges only the records of scans that died without unwinding.
- **BREAKING: the `[documents.output] format` setting, `--documents-output-format` and
  `BASEMIND_DOCUMENTS_OUTPUT_FORMAT`.** No tool read them: the verb tools answer in JSON. The flag is
  now rejected, so a host config that still passes it fails to start `basemind serve`. A
  `[documents.output]` table still loads, and a set `BASEMIND_DOCUMENTS_OUTPUT_FORMAT` is ignored;
  each logs a warning to remove it.
- The daemon socket's forwarded `memory`, `resolved_refs`, `index_read` and `code_search_lanes`
  requests. No process sends them any more: a CLI next to the daemon forwards its whole call, and a
  daemon-hosted session reads through the daemon's pool in process. The comms protocol version stays
  7, so an older basemind of the same generation keeps using a newer daemon; one that still sends a
  removed request has that link closed (ADR-0039).
- **BREAKING: the shared worktree index of ADR-0016** (`BASEMIND_INDEX_LAYERED=1`), its sibling seeding of a new
  worktree's view (`BASEMIND_NO_SEED`) and `scan --verify-shared-base`. The group index replaces them.
- **BREAKING: `index_rescan`'s `full` argument and `basemind rescan --full`.** `{}` and `{"full": true}`
  ran the same walk; the flag's only effect was to discard `paths` when both were passed. `index_rescan {}`
  (`basemind rescan` with no path) is the whole workspace, and a call that still passes `full` is refused
  as an unknown argument. The empty-index notice and error name `index_rescan {}`.
- The `insta` and `wiremock` dev-dependencies, which nothing used.
- **BREAKING: the `workspace` MCP tool and worktree claims.** Advisory worktree claims enforced nothing
  and nothing read them, so they are gone everywhere: the `workspace` tool (all five modes), the
  `basemind workspace claim` and `basemind workspace release` commands, the `claimed_by` field of a
  worktree row (and the fourth, claimant column of the `basemind workspace worktrees` text output),
  the daemon's `worktree_claim` / `worktree_release` requests, and the
  `worktree-coordination` capability of `basemind init`. The read-only listings stay as CLI commands
  with no MCP tool: `basemind workspace workspaces`, `worktrees <repo-id>` and `branches <repo-id>`.
  The comms protocol version is unchanged: a client older than this release that still sends a claim
  has that one call fail with a closed connection, then reconnects. An existing registry snapshot
  loads as before; the claim it carried is dropped.
- **The token-saving text tools.** The `admin` modes `compress`, `delta`, `checkpoint` and `waste` are gone
  from the MCP tool, together with their CLI commands `basemind admin compress`, `basemind compress-output`,
  `basemind delta`, `basemind checkpoint` and `basemind detect-waste`. The plugin no longer ships the
  `compress-output` and `read-cache` PostToolUse hooks, so `BASEMIND_COMPRESS_OUTPUT` and
  `BASEMIND_DELTA_READS` have no effect; snapshots the `read-cache` hook left under
  `${XDG_CACHE_HOME:-~/.cache}/basemind/read-cache/` can be deleted. Reading a symbol's body (now `code_read`) and the
  search guard (`BASEMIND_GUARD`) are unaffected.
- **Web ingestion.** The `web` MCP tool (`scrape`, `crawl`, `map`), the `basemind web` CLI group and the
  `crawl` cargo feature are gone, along with the `crawlberg` dependency; `full` no longer includes `crawl`,
  so `--features crawl` now fails the build. A `[crawl]` table in `basemind.toml` is still accepted and
  ignored (`basemind init` used to write an empty one, and no longer does); one with keys in it logs a
  warning. `BASEMIND_ALLOW_PRIVATE_HOSTS` and `BASEMIND_DAEMON_MAX_CRAWL_PAGES` / `_DEPTH` / `_BODY_BYTES`
  have no effect. Pages already ingested lived only in the per-view document stores, which the first scan
  of this version deletes (see the `docs_search` entry under Changed); `docs_search` has no `scope`.
- **Agent shells.** The `shell` MCP tool (`spawn`, `send`, `capture`, `kill`, `list`, `broadcast`), the
  `basemind shell` CLI group and the `shells` cargo feature are gone, along with the `rmux` git dependencies;
  `full` no longer includes `shells`, so `--features shells` now fails the build. A `[shells]` table in
  `basemind.toml` is still accepted and ignored; one with keys in it logs a warning. `BASEMIND_SHELLS_SOCKET`
  has no effect. Kill any shell sessions before upgrading: a shells daemon an older version left running is
  still listed by `basemind comms doctor` for one more release, but this version cannot stop it.
- **Memory audit, mining and proposals.** The `memory` modes `audit`, `mine`, `proposals`, `accept` and
  `reject` are gone from the MCP tool, together with the matching `basemind memory` commands; a rescan no
  longer audits stored notes. Saving, reading, listing and searching notes and document search are kept
  (see Changed for their new tool names). Stored
  notes keep their `verified`, `last_verified` and `importance` fields, which nothing updates any more. The
  `memory_archive` and `proposals` keyspaces an older version wrote stay in the repo's `memory.fjall`
  untouched and are never opened. The `governance` request was removed from the comms protocol (part of its
  step from 4 to 7, see agent comms v2 above): the first upgraded session replaces a running older daemon, and a client and daemon
  that disagree fail the handshake with `proto_skew`.
- **The code graph.** The `graph` MCP tool (`calls`, `neighbors`, `path`, `subgraph`, `communities`, `map`,
  `export`, `display`, `open`) and the `basemind graph` CLI group are gone, along with the
  `basemind://repo/map` resource and the daemon's `/ui` page. The caller, call-site, implementation, importer and
  definition lookups (now `code_refs`) never used the graph and keep working. The daemon's
  HTTP front-end serves `/mcp` only and no longer accepts the bearer token as `?token=`; send it as
  `Authorization: Bearer <token>`. Document-to-code links are no longer produced or stored: a
  `doc_links.lance` table an older version wrote is never opened again and is reclaimed with the rest of a
  workspace's cache. Inline rationale notes (`WHY:`, `NOTE:`, ADR citations) are still extracted into each
  outline blob so the blob format stays unchanged until the next schema bump, but nothing surfaces them.
- **Archive extraction.** The contents of archives (zip, tar, 7z, gz) are no longer extracted in any build;
  archives are always skipped. `[documents] extract_archives` is still accepted and ignored, `true` logs a
  one-time warning, and `BASEMIND_DAEMON_ALLOW_EXTRACT_ARCHIVES` has no effect. `max_archive_bytes` and
  `max_archive_entries` still apply: they bound the zip containers that Office files (docx, pptx, xlsx,
  odt) are. A config that set `extract_archives = true` has its documents extracted again once, on the
  first scan after upgrading.
- **BREAKING: lean rows on the locating tools.** `code_symbols`, `code_grep`, `code_refs`,
  `find_files`, `git_log` and `git_diff` answer with what the shell would print plus what they add,
  and drop what repeated per row or echoed the call (ADR-0031 holds a locating tool to 1.5 times the
  shell's tokens). None of them carries `elapsed_us` any more; the CLI times the call itself.
  `code_grep` and `code_refs` list hits under their file (`{"<path>": [hit, ...]}`), with a
  `column` only where the line does not place the hit, decided over the whole result and not the page;
  a grep hit's `symbol` is the enclosing symbol's name, `total_matches` is `total`, and
  `pattern`, `regex` and `total_files_matched` are gone; a call site writes `callee` and an
  implementation `trait_name` only when it is not the name asked for, and `name`, `match` and
  `module` are no longer echoed. A `code_symbols` row is `path`, `line`, `kind`, `name` and
  `signature` (no `column`, no `match`). `find_files` lists paths alone (no `language`,
  `size_bytes`, `score` or `returned`). A `git_log` row is `sha` (its first 12 hex digits),
  `summary`, `author` and `author_time_unix`, with `files` as `{"<path>": "<change>"}`; `short_sha`,
  the `path` / `pattern` / `name` echoes and a search hit's `body`, `author_email`,
  `body_truncated` and `body_bytes` are gone (`git_log {rev}` keeps the full sha, body and email).
  `git_diff` drops a hunk's `kind`, writes `present_at_from` / `present_at_to` only when false, and
  leaves an empty status list out. The CLI's `--json` output has the same shapes;
  [`docs/UPGRADING.md`](docs/UPGRADING.md) lists every field.
- **LLM summarization and the `[llm]` table.** LLM-backed (abstractive) summarization and the LLM NER
  backend are gone. `[documents.summarization] strategy = "abstractive"` is still accepted, logs a one-time
  deprecation warning and produces extractive (TextRank) summaries. `[documents.ner] backend = "llm"` is
  still accepted, logs a warning and extracts no entities; that backend was never compiled in, and the
  earlier warning that claimed an ONNX fallback was wrong. Nothing else read `[llm]` (the reranker is a
  local ONNX cross-encoder), so the table (`model`, `api_key`, `base_url`, `temperature`, `timeout_secs`,
  `max_retries`, `max_tokens`), the `--llm-*` flags on `basemind scan` and the `BASEMIND_LLM_*` variables
  are accepted and ignored, with a one-time warning when `[llm]` has content. `BASEMIND_ALLOW_REPO_LLM` has
  nothing left to grant, and a repository's `[llm] base_url` and `api_key` are no longer stripped on
  load. A config that set `[llm] model` has its documents extracted again once, on the first scan
  after upgrading; one that left it unset keeps its cached documents.
- **Document formats and unused xberg capabilities.** SQLite databases, WordPerfect,
  HWP/HWPX and Apple iWork (Pages, Numbers, Keynote) files are no longer extracted, and SVG is no longer
  rasterised or mined for diagrams (its text is still read as XML). Model-based layout detection, PII
  redaction, QR-code decoding and document diffing are no longer compiled in, and auto-rotate only comes
  with `documents-ocr-paddle`; none of them was reachable from basemind's config or tools.
- **PaddleOCR in the published binaries.** PaddleOCR is now the opt-in `documents-ocr-paddle` feature,
  which `full` does not include, so the Homebrew, npm, pip and GitHub release binaries no longer contain
  it. Tesseract OCR is unchanged.

### Fixed

- **A refused scan no longer deletes the old release's index (#438).** The per-view `index.fjall` of
  0.33 was deleted at the writer open, before the scan's free-space check could refuse, leaving no
  usable index under either release. It is now deleted by the scan that passes the check.
- **`upgrade --plan` asks for what the scan asks for (#439).** The plan counted 1.5x the index plus
  the headroom; it now uses the scan's estimate, charging the source the older blobs came from at the
  scan's per-source-byte ratios. `docs/UPGRADING.md` also says that a 0.33 daemon which touched a
  repository must be stopped before it is scanned, and that a forwarded scan reads the daemon's
  environment (#440).
- `docs_search` with a `keywords` or `entity` filter that finds nothing says why when `[documents.keywords]`
  or `[documents.ner]` is off, or when the `entity` category is one the extractor never emits, naming the
  config key or the accepted categories instead of returning no hits (#443). A filter that finds hits is
  never refused, so a scan run with CLI or environment overrides keeps working.
- A detached server that outlived its session is reported (#201).
- `code_read` of a span ending on the last line of a CRLF file no longer keeps that line's trailing `\r`.
- Error text that names a path no longer rewrites a home or repository root that only appears inside
  a longer path (`/mnt/home/you/x` is not `/mnt~/x`).
- A repository group opens outside the global registry lock, so one slow open no longer blocks another,
  and a poisoned lock is recovered instead of propagating a panic (#221).
- `git_diff` no longer fails on a submodule, a directory or a symlink (#302). A submodule diffs as
  its two `Subproject commit <id>` lines, a tracked symlink as its link text on both sides (the
  file it points to is not read), and a directory is refused with an invalid-argument error that
  names `git_log` for its history.
- `basemind memory put` and the other memory calls in a workspace nothing has scanned said another
  process holds the index lock. They now say the workspace has no index and to run `basemind scan`;
  a genuinely held index still exits `3` (#430).
- An unknown `[languages.<name>]` key with no near match among the downloaded grammars now says
  that suggestions come only from downloaded grammars and names `basemind lang install` (#432).
- Lowering `[documents] max_file_bytes` below the size of an already indexed document now drops it
  on the next scan instead of keeping it as unchanged (#433).
- An exported but empty `BASEMIND_*` variable counts as unset everywhere the engine configuration
  reads it; an empty `BASEMIND_CACHE_BUDGET_MB=` no longer turns cache-budget eviction off.
- `basemind lang clean` refuses (exit 2) while grammars are offline (`BASEMIND_OFFLINE` or
  `BASEMIND_GRAMMAR_OFFLINE`) unless `--yes` is passed, so it no longer empties a hand-copied
  grammar cache that cannot be downloaded again.
- `basemind hook install` refreshes the pre-commit hook in place only when it equals the shipped
  hook up to line endings (a CRLF copy from `core.autocrlf` still refreshes); a hook that carries the
  marker but has lines added is treated as foreign (refused, or backed up with `--force`) instead of
  being overwritten, and `basemind doctor` warns about it instead of reporting it installed.
- A blank `BASEMIND_DATA_HOME` or `BASEMIND_COMMS_DIR` (empty or whitespace only) counts as unset, as
  the other `BASEMIND_*` variables do, instead of naming the current directory.
- In a repository without an `origin` remote, the memory scope (and the registry and comms repository
  key) is now the canonical main worktree path, so every worktree of it shares its notes; before, a
  linked worktree filed and read notes under its own path (#319). Notes saved earlier from a linked
  worktree, or from a checkout reached through a symlink, stay under the old key.
- On Linux, a directory the inotify watch limit refused a watch no longer goes stale without a
  word (#334). `index_status` gains `watches`: the watches the daemon holds (`held`) and, once
  `fs.inotify.max_user_watches` is exhausted, the directories left unwatched (`unwatched`), the
  current limit (`limit`) and a `note` naming the sysctl to raise. The daemon log warns once per
  pass that leaves more directories unwatched instead of once per directory. A refused directory is
  retried whenever its subtree is armed again.
- A daemon started implicitly by a session or a CLI call now drops `BASEMIND_ALLOW_DEPENDENCY_ROOTS`
  with the other operator grants; it kept it, so the shell that happened to start the daemon granted
  dependency roots outside the repository to every workspace it served. `basemind comms start`
  still passes it on.
- A `code_search`, `docs_search` or `code_grep` that runs past 150 s is stopped (#217): it answers
  `deadline_exceeded` (`-32004` for a CLI), its cancellation token fires, and its heavy-admission
  permit moves to an overflow pool as large as the heavy lane, so the next heavy call is admitted.
  A hung read used to keep its permit until the daemon restarted, and every later heavy call
  (`docs_search`, `index_rescan`, CLI `code_search`) was refused with `server_busy`. A client that
  cancels such a read frees the lane the same way, and so does the stdio relay: a request it answers
  with `backend_timeout` is cancelled on the daemon (`notifications/cancelled`). Abandoned reads
  beyond the overflow pool keep their lane permit, so hung work stays bounded.
- An author name or email that is not UTF-8 is decoded by the commit's `encoding` header, or as
  Windows-1252 when it has none or the header does not read it. It was `?` with an empty email, so
  `git hotspots` counted every such author as one, and `git log`, blame and `code explain` named
  them `?`. `GIT_HISTORY_SCHEMA` and `GIT_CACHE_SCHEMA` are bumped: the git-history index rebuilds
  once on the next scan and cached git answers are recomputed (#305).
- `basemind init` writes the rules file, `.claude/settings*.json`, `basemind.toml` and `.gitignore`
  atomically (#327): each goes to a temporary file beside it that is renamed over it, keeping the
  file's permissions, so a crash or a failed write leaves the original intact instead of truncated.
  A symlinked target is written through, keeping the link, and reported as `via symlink → <target>`;
  a target that resolves outside the repository is refused unless `--yes` is passed. It was followed
  silently, wherever it pointed.
- The daemon's `daemon.log` no longer fills with dependency INFO lines (#200): lance and lancedb
  logged every dataset load, commit and query plan, and rmcp every session's handshake, about
  1.2 MiB a minute under load and 93% of the log. Without `RUST_LOG`, `lance` and `rmcp` now log
  at `warn`; basemind's own INFO lines are unchanged.
- A source file that is not UTF-8 (Latin-1 or cp1252 Java or Python, a stray byte in a comment) is
  indexed (#316). It was dropped from the code map, so it had no outline, symbols, chunks or
  `code_grep` hits, and only the scan's counters said so. Each byte of an invalid sequence now reads
  as `_` (`Caf\xe9` is the class `Caf_`), so every line, column and byte offset is the file's own;
  `code_grep` no longer counts such a file in `skipped_files`. The next scan indexes these files;
  nothing else is re-extracted.
- Outlines, `code_symbols` and implementations no longer miss common declarations (#311).
  TypeScript and TSX: abstract classes and their abstract methods, class fields (`field`, or
  `method` when one holds a function, as a React class component's `handleClick = () => …` does),
  interface methods and properties, `declare function` and `var` (`variable`, or `function` when it
  holds one). JavaScript: `var`, class fields and CommonJS exports (`exports.x = …` and
  `module.exports.x = …` define `x`, `module.exports = …` defines `module.exports`). Java: enums,
  records, annotation types and their elements, constructors, and fields (`const` when
  `static final`, as interface fields are). Python: annotated fields without a value (dataclass,
  pydantic and `TypedDict` fields; a module-level `x: int` is a `variable`) and `type` aliases. Go:
  `type A = B` aliases and interface methods. A base written as a member expression
  (`extends React.Component`), a generic type (`implements Repo<T>`, `interface A extends B<T>`) or a
  Python subscript (`Generic[T]`) is now an implementation, named by its last segment.
  `EXTRACT_EPOCH` is 7.
- The plugin hooks no longer need `jq` (#239). Without it the session-start hook told the agent to
  ignore a working basemind and left no indexed-session mark, so the `PreToolUse` guard never
  steered, and `inbox-notify` stayed silent. The hooks now read and write JSON in bash and POSIX awk
  (`hooks/json-lib.sh`) instead. They need bash and a POSIX awk: Git for Windows ships both, and a
  minimal image without bash, such as Alpine, needs it installed.
- **The free-space check counts what a scan writes, not only the index it has**
  ([#203](https://github.com/Goldziher/basemind/issues/203)). A full scan asked for 1.5x the group
  index plus 512 MiB, so a first scan, or the first after a release-minor bump, started from an empty
  index and passed with about 512 MiB free before writing several GB of index rows, blobs and vectors.
  The check now runs once the walk has listed the candidates and before anything is extracted, and adds
  3.5x (index rows), 2.5x (blobs) and, when code is embedded, 1.5x (vectors) the bytes of every
  candidate with a grammar that no file map lists yet, sized as the scan reads it: a staged or rev scan
  by its git blobs, a working-tree scan by its files. The refusal, in the CLI and from the daemon,
  states the free and needed bytes and each part of the estimate, and names the blob namespaces and
  per-view indexes an older release left in the cache, with how each is removed.
- **A blob write that finds the disk full stops the scan**
  ([#203](https://github.com/Goldziher/basemind/issues/203)). It marks the scan partial and fails it,
  as a full-disk index commit does; it was counted as one failed extraction of a scan that read
  complete.
- The git cache's disk directories stay within their budget while the daemon runs (#304). They were
  swept only when a workspace's read stack opened, so a daemon hosting one for days grew `log/`,
  `blame/` and `commit_files/` without bound. Each write now counts its bytes, and the write that
  takes a directory past `BASEMIND_GIT_CACHE_LOG_MAX_BYTES` starts a sweep that evicts the oldest
  payloads down to nine tenths of it. The sweep at open runs in the background too, so opening a
  stack no longer stats every cached file.
- `git_log` with a file `path` trusts an empty answer from a git-history index that holds HEAD
  (#297): a new, untracked or mistyped path answers at index speed instead of walking every commit
  to return nothing. A lookup that fails is now an error of its own, and only that falls back to the
  live walk. The live walk, of a file's history and a symbol's, tests at most 10 000 commits per page;
  a page that runs out first says `truncated_reason: "span_limit"` and its `next_cursor` resumes at
  the first commit left untested. A path no page matched is still refused on the last page, and a
  symbol change at the edge of a span is compared with its commit's first parent. A posting list that
  does not decode is an error, not a short history. A live-walk cursor now counts commits of the whole
  history, so one an earlier release issued resumes elsewhere: repeat the call without it.
- The shell status line no longer reads an in-repo `.basemind/` (#321). A repository carrying only the
  legacy `.basemind/basemind.toml` config showed `scanning…` for good, and a leftover legacy cache showed
  counts nothing writes any more; every repository now gets the line `basemind statusline --root`
  renders from the machine-global cache. That call is bounded to 2 seconds on a machine without
  `timeout` too (stock macOS), where a hung binary stalled the bar. The plugin version and the
  unread-message segment, which only the legacy branch rendered, now frame that line, so they show for
  global-cache repositories, the version on the `no index` hint too. The unread count is every unread
  message (`100+` past one inbox page), not one page of a single message. The legacy branch's per-type
  call counts and disk and RSS figures are gone with it.
- The plugin's MCP server starts on Windows (#238). Every generated manifest launched
  `scripts/mcp-launch.sh` directly, which Windows cannot run, so a Windows install got no `basemind`
  server. The manifests now launch the polyglot `scripts/mcp-launch.cmd`: cmd.exe finds Git Bash
  (`CLAUDE_CODE_GIT_BASH_PATH`, the Program Files and per-user installs, then `bash` on `PATH`,
  skipping the WSL launcher) and runs the launcher through it, or exits naming Git for Windows when
  none is installed; macOS and Linux run it through its shebang. The plugin's scripts and hooks are
  checked out with LF line endings on every platform, since bash cannot run a CRLF script.
- The Codex config `.mcp.json` runs the launcher of the enabled plugin, not the most recently
  modified `codex-mcp-launch.mjs` in any marketplace's cache (#246). It reads the
  `[plugins."basemind@<marketplace>"]` tables of `$CODEX_HOME/config.toml`, ignoring table-shaped
  text inside strings and comments, passes over one with `enabled = false` (bare or quoted key),
  searches only those marketplaces (only `basemind` when the config names none, and no cache when
  every table it names is disabled) and runs the highest SemVer version: a release ahead of its
  prereleases, and prereleases ordered by their identifiers (`rc.10` above `rc.2`). File age breaks
  only a tie. A stale or foreign copy no longer wins, and the chosen path is logged to stderr.
- Cursor gets one plugin, the repository root's `.cursor-plugin/` (#320). The generated
  `plugin/.cursor-plugin/` resolved its hook scripts against `${CURSOR_PLUGIN_ROOT:-.}`, so with the
  variable unset a hook ran `./hooks/run-hook.cmd` from the opened repository; it also started the
  MCP server without `--root` and wired `agent-as`, which Cursor does not run. `plugin:verify`
  (`sync_legacy_plugin_exports.py --check`) fails on a shipped hook whose root falls back to a path.
- `basemind sessions` (#162): a session that ran in a worktree since removed counts under that
  worktree's name instead of `other_directories`. A worktree is removed when its gitdir is gone and
  `git worktree list` no longer lists it; it is recognised in a directory that holds the
  repository's live worktrees and no other repository. A Codex reply that calls no tool right
  after a fork point now counts its output tokens: a copied token count is told by its cumulative
  totals within the thread it descends from, not by the call before it, so unrelated conversations
  with equal totals each count theirs. A Claude Code project directory whose slug is exactly 200
  characters is matched whole, as Claude Code keeps it, and a longer one only by its first 200
  characters followed by `-`. The transcript roots are under the home directory: `HOME` on Unix,
  else the account's home directory; on Windows `USERPROFILE` when it is set, else the account's
  profile directory. An empty home is refused rather than read as the current directory.
- Agent comms: a thread the per-repository cap of 256 active threads archives no longer hides its
  unread messages from its members without a word (#300). A member's next `agent_inbox` read over
  all its threads, a marking read or a peek, carries a notice naming the thread and how many of its
  messages the member has not read, and says to read them with `agent_inbox {thread}`; a marking
  read tells each actor once, and a replay of it tells again. A waiting read returns at once with
  such a notice. A post, join or invitation into the thread is refused with the new code
  `thread_archived_by_cap`, which says the cap archived it and to continue in a new thread by
  posting with `subject` and `to`. A thread archived by its creator, or by its last member leaving,
  is unchanged.
- Python imports other than `from m import x` now reach the files they name (#307). `import a.b`
  resolved nowhere and `import a.b.c` counted one external import per name; it is now one import of
  `a/b/c.py` bound to `a`. Aliased imports (`import a.b as n`, `from m import x as y`) were dropped
  and now bind the alias, so a `y()` call resolves into `m`, and a package `__init__.py` that
  re-exports with `from .impl import helper as helper` forwards `helper`. A `from` import of a
  submodule (`from . import views`) is an import of `views.py`. For Python this changes the
  `code_refs` `importers` and `cycles` relations, `code impact`, `code cycles` and the unreachable
  files of `code deadcode`. `RESOLVE_EPOCH` is 2: the next scan resolves every file again once.
- A command run from a directory that was removed under it (a deleted worktree) exits `1` with
  `read the current directory (pass --root to name the repository)` instead of panicking (#223). A
  scan whose worker pool the OS refuses to spawn, an embedding call whose thread pool cannot be built
  and `Broker::new` when neither the machine registry nor its fallback opens return errors too.
- A tool error names no absolute host path (#257). A data-home path is written
  `$BASEMIND_DATA_HOME/…`, a repository path repository-relative and any other path under the home
  directory `~/…`; the full text goes to the server log. A store or index that cannot be read (an
  I/O error, a corrupt or undecodable blob, an unreadable file map, a damaged index), and an index a
  rescan cannot write (a full disk, a failed write), is `index_unavailable` and says "run `basemind
  doctor`; `basemind scan` rebuilds it" in place of the library's message, on an in-process and a
  daemon-hosted rescan alike, and a held index is `busy`. A panicking tool body answers with a fixed message
  naming its failure reference in the log, not the panic payload.
- JS/TS imports resolve through barrel files built from `export { x } from`, `export { a as b }
  from` and `export * from` (#310): `code_refs` definition and references now reach the defining file
  through any chain of such re-exports, up to eight files deep. A re-export cycle is cut where it
  closes, and `export * from` does not forward `default`. The next scan re-resolves every file once,
  with no cache wipe.
- `basemind hook install --force` no longer replaces an earlier backup (#248). A foreign
  pre-commit hook is moved to the first free name of `pre-commit.bak`, `pre-commit.bak.1`,
  `pre-commit.bak.2`, ..., so a second forced install over a different hook keeps both. Before, it
  was renamed over `pre-commit.bak`, deleting the hook backed up the first time.
- `BASEMIND_GRAMMAR_OFFLINE=false` (or `no`, `off`) no longer means offline: any value but `0` or
  blank turned grammar downloads off. Likewise `BASEMIND_GIT_DISCOVER_PARENTS`, `BASEMIND_GH_INDEX`
  and `BASEMIND_GREP_BLOOM` honoured only `0` as false, and `BASEMIND_NO_AUTOSPAWN`, `BASEMIND_ALLOW_*`
  ignored `on` (#293).
- `docs_search` no longer hangs in the daemon after a scan writes to the document store a search
  opened first (#217). The writer opened a second store on the directory, and a later query read the
  full-text index cached by the first through that store's dropped runtime, waiting for ever on a
  posting list for a term no earlier query had used. The writer now adopts the search's store.
- A cancelled `docs_search` or `code_search` stops its work, as `code_grep` does: the ranking checks
  the call's cancellation before each lane, each widening fetch, while hits are hydrated and before
  the rerank, instead of running on to the end on a blocking thread nobody waits for (#217).
- `basemind cache gc` reaps what the daemon's sweep reaps (#267): the workspace caches of deleted
  worktrees, then the group directories no workspace names, then the blobs only they pinned, and
  reports `workspaces_reaped` and `group_stores_reaped`. It swept only blobs, so after
  `git worktree remove` the removed worktree's cache stayed and the next scan never retired its view
  from the shared group. A cache another process holds locked is skipped. Budget eviction stays with
  the daemon.
- `basemind memory delete` removes the note's vector rows from the store of every embedding model
  of the repository group, not only the configured one, and purges each store after: the deleted
  rows are rewritten out of the data files and every earlier table version is dropped, so a store
  written by an earlier release, whose rows carried the note's text, no longer keeps it. The
  `memory_save` description and the memory manual now say notes are plain text on local disk and
  must not hold secrets (#324).
- Offline mode did not stop the grammar download a file in a language not yet cached triggers on
  first use; only the scan's bootstrap and `basemind lang install` honoured it. The file's grammar is
  now refused with the same error (#208).
- A call site is recorded at its callee name, not where its call expression starts. In a method chain
  written over several lines (`target`, `.steal`, `.steal_into(..)`) `code_refs`, `code explain` and
  `code migration check` reported the receiver's line, up to several lines above the method name a
  text search finds; a path call (`LocalSet::new()`) and every call in Java now carry the name's
  column too. Affected Rust, JavaScript, TypeScript, TSX, Python, Go and Java, plus HTTP client calls.
  Method calls now also start at the byte a resolver records the use at, so they can be graded by
  resolution. `EXTRACT_EPOCH` is 5: the next scan re-extracts every file once.
- A Rust trait impl whose self type is not a path or generic type (`impl Kill for &mut T`,
  `impl ToSocketAddrs for (Ipv4Addr, u16)`, `impl FromStreamPriv<()> for ()`, a slice, array, pointer
  or `str`) is an implementation: `code_refs` with `relation: "implementations"` lists it, its
  `impl_type` the type as written on one line. They were missing. `EXTRACT_EPOCH` is 6.
- `git_diff` against the working tree compares the file in the form git stores for it (#295): line
  endings converted per `core.autocrlf`, `core.eol` and `.gitattributes`, `ident` collapsed, and
  clean filters such as Git LFS run. An unchanged file in a CRLF checkout showed every line as
  changed, and an LFS file diffed its pointer against the payload. A filter that fails is an error
  naming the file instead of a whole-file hunk. A structural `git_diff` takes the working tree's
  symbols from that form too, so a clean-filtered file no longer lists the payload's symbols as
  added and the pointer's as removed. A `changed` scope against a revision compares a restored file
  the same way; one whose filter fails stays in the scope and is logged as a warning.
- Unshallowing or deepening a clone changes the next `git_blame`, `git_log` and `git hotspots` answer
  without a cache clear (#298). Answers computed while the clone was shallow (a blame cut at the
  graft, a walk that ends at the boundary) were cached by commit sha and outlived `git fetch
  --unshallow`; they are no longer cached, and a daemon re-reads whether the clone is shallow on
  every call instead of once at open. A git-history index built in a shallow clone is no longer read
  once the clone's `shallow` file differs from the one it was built under (a boundary other than its
  oldest commit can be deepened too), though it still indexes HEAD, and is rebuilt with the reason
  `deepened`. `GIT_CACHE_SCHEMA` and `GIT_HISTORY_SCHEMA` move, so payloads of earlier builds are
  misses and the history index rebuilds once.
- A shallow clone's boundary commit is a root commit to both the git-history index and the live
  walk (#301): it adds every file in its tree, as `git log` shows it. The index listed no files for
  it while the live walk matched it for every path it holds, so `git_log` with a `path` answered
  differently depending on which one served it. A merge with a parent the clone lacks is compared
  against the parents it holds, in both.
- The watcher no longer discards the OS's "events were dropped" signal (FSEvents `MustScanSubDirs`,
  inotify queue overflow). A signal naming a subdirectory rescans that subtree; one naming the root,
  or none, runs the whole-tree stat-and-hash pass of `index_rescan {}`; a burst of signals is one
  pass. Before, an edit the OS dropped stayed unindexed until that file changed again.
- A serve watcher whose read stack was evicted or shed no longer scans a batch it takes after the
  drop: that scan reopened the evicted workspace (and its writer lock) to publish to a stack nobody
  served.
- A daemon-hosted workspace opened over a populated index catches up on the edits made while no
  watcher ran (the daemon's idle exit, a shed read stack, a `git checkout` or `git pull` in
  between): building the read stack runs the whole-tree stat-and-hash pass of `index_rescan {}` in
  the background, reporting `rescanning` until it commits. Before, those edits were served stale,
  unflagged, until each file changed again or someone ran `index_rescan`.
- `memory_search {key}` for a key no note has answers `{"key":…,"found":false}`. It answered
  `structuredContent: null`, which MCP types as an object, so a host that validates results
  rejected the routine "is there a note?" call.
- A cancelled `code_search` or `docs_search` holds its heavy-call permit until its body, which runs
  on the blocking pool, has ended. Before, the permit was released at the cancel while the
  embedding and rerank kept running, so a client retrying a timed-out call ran one more model pass
  per retry.
- `code_refs` callers (with `path`) and implementations flag a hit in a file edited since the scan
  `stale`, as the call sites of a name scan already did.
- The server instructions and the `basemind init` rules block send exact text in markdown, config
  and data files to `code_grep`, which searches them; `docs_search` is for documents by meaning.
- `code_symbols` paging neither repeats nor skips a row when a candidate's outline cannot be read.
- A `code_outline` cursor into a file edited since the scan is refused once the file changes again,
  instead of continuing at an offset into different symbols.
- `git_blame` of a whole file the revision lacks says `no file … at <rev>`, as the span and symbol
  forms do, instead of an internal error; its only truncation reason is `shallow_clone`.
- `resources/read` no longer waits for the in-RAM map to build; none of the resources reads it.
- A forwarded CLI command takes only a response as its call's reply, never a daemon request that
  happens to carry the same id.
- The daemon's memory no longer climbs under steady memory, document and code-search writes: every
  LanceDB store of a process caches through one session of 64 MiB (index) and 32 MiB (metadata)
  instead of Lance's default 6 GiB and 1 GiB per store, where each commit's manifest and file
  metadata stayed cached after its version was pruned.
- A root commit's file list (`git_log` `files`, a directory's history, `basemind git churn` and
  `git by-path`) names its files only, no longer every directory of its tree. The git-history index
  rebuilds once and the cached file lists are recomputed.
- The plugin launcher releases its install lock when a signal (a hook's time limit, a closed host)
  stops it mid-download, instead of making the next start wait 120 s for it.
- The session-start hook bounds its `basemind status` probe without a `timeout` binary (stock
  macOS), so a hung probe no longer holds the session.
- The status line and the PreToolUse guard keep their state in a directory only the user can write
  (`$XDG_RUNTIME_DIR/basemind`, else `${TMPDIR:-/tmp}/basemind-<uid>`, mode 0700), never through a
  link another local user planted in the shared temporary directory.
- The npm wrapper downloads over HTTPS only, a redirect included, and reports a binary killed by a
  signal as a failure.
- The npm wrapper's binary download honours `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` (#325): behind a
  proxy, `npm i -g basemind`, `pnpm add -g basemind` and the first-run install in `bin/basemind.js`
  timed out, since Node does not read these variables by default. Requests to a host `NO_PROXY` does
  not list go through an HTTP `CONNECT` tunnel to the proxy, a redirect's target included, with TLS
  to GitHub inside it; a failed install prints the proxy settings in effect, credentials removed.
- A `code_search`, `docs_search` or `code_outline` cursor issued right after the server started is
  no longer refused as stale on the next page: the background warm of the in-RAM map counted as an
  index change, though none of the three lists is read from that map.
- On Windows, a harness given by bare name (the default `claude`, `codex`, `opencode`) is found
  through `PATHEXT` (`codex.exe`) instead of never matching, or matching the extensionless POSIX
  shim npm installs beside the `.cmd`. A batch file it finds (`claude.cmd`) is refused, as
  `cmd.exe` would look for `node` in the agent's working directory first, and a Windows child gets
  `NoDefaultCurrentDirectoryInExePath=1`.
- `basemind-agents` (not yet wired into the server) closes the gaps of its third security review:
  - Claude Code is launched with `--disallowedTools` for the tools that start work outside the
    session (`RemoteTrigger`, `CronCreate`, `ScheduleWakeup`, `SendToProject`, ...), its brief
    waits for an `initialize` answer reporting the requested mode and only built-in commands, and
    its init frame may list no plugin or `.claude/`-defined skill or agent.
  - A repository whose own git config sets a filter, a diff driver, an external diff or an include
    is refused (Codex runs `git status`, and with it a clean filter, outside its sandbox), and every
    child gets `core.fsmonitor=false`. Gemini and the unverified ACP agents are refused over a
    `.env*` file. A root that holds `HOME` or a harness's or basemind's directory is refused.
  - A harness's process group dies with a launcher killed outright (a keeper process per group),
    and wall time counts while the machine sleeps.
  - Requests pending when a session fails are reported withdrawn; a failure's stderr tail moves
    to the launcher-only `Event::Diagnostics`, and `Event::Error` is one sanitised line. A Codex
    command asking for more access is a permissions request showing it, a Codex turn frame without
    a `threadId` fails the session, and an OpenCode mode switch through its config option is caught.
  - Errors no longer echo paths or tell a missing working directory from one outside the root, and
    `Debug` output leaves out the MCP server's arguments.
- On Windows, `admin eval` baselines run their shell commands with Git for Windows' path
  conversion off, so an argument such as `-L '/fn alpha/,/^}/'` reaches git as written.
- `admin eval` baselines and `benchmarks/eval/run_gate.sh` run with `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_CONFIG_GLOBAL=/dev/null` and `LC_ALL=C` (#292), so a host's `color.ui`, `core.autocrlf` or
  locale no longer changes the baseline token counts.
- `basemind scan` next to a running daemon is forwarded to it as a full rescan (ADR-0024 rule 5)
  instead of stopping with "already running against this repo" (exit 3). It prints `scan via daemon`
  with the daemon's counts, then syncs the git-history index through the daemon unless
  `--no-git-history`. A flag the daemon cannot apply (`--rebuild-git-history`, `--verify-shared-base`,
  a `--documents-*` / `--llm-*` override) exits 2 naming it; `--staged` and `--rev` exit 3 while the
  daemon holds the workspace. With no daemon the scan runs directly and starts none, and a lock held by
  any other process still exits 3 naming it.
- A daemon that stopped answering is SIGKILLed (`TerminateProcess` on Windows) only if, after the
  SIGTERM grace, its generation's lock still reads held and the pidfile still names the same process.
  One that released the lock while draining, or whose pid passed to another process, is not killed.
- Two installed basemind versions no longer kill each other's daemon. The daemon's socket (named
  pipe on Windows), lock, pidfile, log, `http.addr` and comms store moved from the comms directory
  into a protocol-generation subdirectory, `comms/g1/`, and its record in the daemon registry moved
  from `daemons/` to `daemons/g1/`. A binary only looks inside its own generation, so a 0.33 install
  and a newer one each keep their own daemon and neither can reach the other's. `BASEMIND_COMMS_DIR`
  still names the comms directory; the generation applies inside it, which leaves a long override
  three bytes less of the Unix socket path limit. The comms store starts empty in the new
  generation, and the files of the old layout, the old message store included, stay until
  `basemind comms cleanup` removes them. Limit: the workspace index, memory and git-history stores
  are not per generation, so a second daemon cannot serve a workspace the first one holds. See
  [`docs/UPGRADING.md`](docs/UPGRADING.md#running-two-basemind-versions-side-by-side).
- `basemind comms stop --all` and `basemind comms doctor` (with `--probe` too) cover the daemons of
  their own protocol generation only, and can no longer stop or force-terminate a 0.33 daemon.
  `doctor` reports how many daemons of other generations it counted. The ceiling on live daemons
  counts every generation's, from their registry records. On Windows the named pipe is named from a
  BLAKE3 hash of the account name and the generation directory's path components, so `C:\x\comms`,
  `C:\x\comms\` and `C:/x/comms` name one pipe, as they are one `daemon.lock`.
- Within a generation, bringing up the daemon no longer signals one that answers. A daemon of the
  same or a newer release is never asked to stop: it is used, or, when it is newer and speaks another
  protocol, reported with what to update. An older one is asked to stop, and a `daemon_busy` refusal
  is honoured instead of being followed by SIGTERM and SIGKILL; a store-error refusal is reported
  with the failing store and the commands that reclaim the daemon. A daemon that closed its socket
  gets the whole exit window to leave, so a second client arriving during a drain no longer signals
  it. Signals are sent only to a process still there afterwards, silent to `Stop` and `Ping`, that
  the generation's `daemon.pid` then names. `basemind comms stop` now reaches a daemon that speaks
  another protocol version, waits the normal stop timeout for its answer and honours a refusal.
  On Windows every such exchange with a daemon is now bounded: the pipe read takes no timeout, so
  a daemon that accepted a connection and never answered could hang `comms stop` and daemon
  bring-up for good. The exchange is made with an asynchronous pipe client under a timeout, and
  giving up cancels the read, so nothing stays blocked and the next probe is sent like the first.
  A pipe whose every instance is busy is a daemon at work: the open is retried for a bounded time
  and the daemon counts as present, where it read as silent and could end in `taskkill /F`. A
  `Ping` that could not be sent at all is reported as unknown, not as silence: the daemon is not
  signalled and no second one is spawned.
  "Older" and "newer" follow semantic-version precedence, pre-release included: the daemon of
  `0.34.0-rc.1` makes way for `0.34.0` and is never preferred to it.
- `basemind comms stop` and `comms stop --all` reclaim a daemon by the takeover's own steps: wait
  the exit window, ask once more (`Stop`, then `Ping`), and signal only a daemon silent to both, as
  the process the generation's `daemon.pid` names at that moment. `stop` read the pidfile once and
  signalled twelve seconds later without asking again; `stop --all` gave each daemon 800 ms to
  answer `Stop` and then killed the pid in its registry record. A daemon that is still there (it
  holds `daemon.lock`, or its socket still accepts) but that the pidfile does not identify now
  fails the command with how to find it, where `stop` printed `no live comms daemon` and exited
  0. `stop --all` stops the daemons in parallel, reports the ones it left running (`left_running`
  with `--json`) and the ones that refused (`refused`), and exits non-zero when there are any of
  either, naming each with its reason, as a single `stop` does. A daemon is signalled only while
  its generation's `daemon.lock` still reads held: one that died inside the exit window leaves a
  pid the system may have given to another process, and a free lock now means nothing is
  signalled. Nor is a process other than the one the pidfile names (macOS, Linux and Windows),
  which is what a pid given out again looks like while a new daemon already holds the lock: the
  record holds the daemon's process start and boot, so a wall-clock step does not make a live
  daemon unidentifiable; a record from an earlier release is judged by start time. On Windows a
  recorded pid whose process refuses to be opened (a SYSTEM or another user's process given the
  pid) is no longer read as a live daemon or scan, so it is not signalled or waited for. Its
  hosting marker and this generation's registry pidfile are kept, since another account or a
  sandboxed basemind sharing the data home refuses to be opened too. A `Stop` is
  refused while a relay session is still being set up, not only once it is
  registered; a relay link that has not sent its hello within 30 s is dropped, so a stalled client
  cannot hold a daemon in place. A `basemind rescan` running through the daemon makes it refuse
  `Stop` as well, where a takeover's `Stop` was accepted and cancelled the scan half way.
- `basemind serve` no longer retries forever, silently, when it cannot get a daemon, and no longer
  exits before reading the host's `initialize` when it cannot get one at start. The first
  connection goes through the same path as a reconnect. A failure no retry can cure ends the
  session: a newer daemon on another protocol or relay protocol, a socket path too long to bind, a
  view the daemon does not host, a root it refuses to index. Pending and following requests,
  `initialize` included, are answered with the reason and the process exits non-zero without
  waiting for the host to close its pipe. Anything else is retried. A request waits ten seconds,
  counted from its arrival or from the loss of the daemon it was sent to, and longer while a
  connection attempt that has not failed is still running (a daemon being started or an older one
  making way), up to ninety seconds from that attempt's start: those in flight that are safe to
  replay are re-sent if a daemon arrives, as after a restart, and those that arrived meanwhile are
  forwarded. After that `basemind serve` answers by itself while the retries go on: `initialize`
  from the binary's own server description (with `tools.listChanged`), then `ping` and
  `tools/list`, and every other request with the retryable reason. A session started under a busy
  older daemon is therefore up from the start; once that daemon leaves, the relay initializes its
  own daemon with the host's `initialize` and sends `notifications/tools/list_changed`. A
  notification from the host that waited past the ten seconds is dropped, apart from
  `notifications/initialized`. The host's pipe is read during a connection attempt and between
  attempts, and `basemind serve` exits when the host closes it; inside an attempt that can take
  the few seconds a liveness probe or a forced takeover blocks for. An older daemon that will not
  stop, the
  daemon ceiling, `BASEMIND_NO_AUTOSPAWN` with no daemon and a session the daemon declines for now
  are logged at warn and retried with a back-off growing to a minute; a daemon that is merely slow
  to come back, or that declined because it is stopping, is retried within seconds and logged at
  warn from the fourth failed attempt. An older daemon is asked to stop at most once a minute.
- A workspace whose index another basemind holds no longer ends the session. The daemon declines
  such a relay session with `workspace_locked` and the holder (the version of the daemon that
  holds it, or the command the lock records), where it answered `host_build_failed` and
  `basemind serve` exited on `initialize`. The session comes up, answers tool calls with `another
  basemind holds this workspace's index (...). This session connects by itself once that process
  lets go: ...`, and connects once the lock is free. A workspace the daemon's own scan holds is
  declined with `workspace_busy` and retried within seconds; an index written by a newer basemind
  with `index_newer`, which ends the session with the reason, since no retry changes it. Both
  were `host_build_failed`, retried once a minute. A refusal with a token this build does not
  know is retried the same way, never fatal. See
  [`docs/UPGRADING.md`](docs/UPGRADING.md#running-two-basemind-versions-side-by-side).
- With `BASEMIND_ALLOW_HTTP`, a daemon that finds the default port `51786` in use (by the daemon of
  another protocol generation, for one) binds an OS-assigned port and records it in `http.addr`
  instead of running without its HTTP front-end. An address set with `BASEMIND_HTTP_ADDR` is still
  bound as given or not at all.
- The plugin launcher no longer stops other versions' processes. On every launch it sent SIGTERM and
  then SIGKILL to each `basemind` of a different cached version, including the `serve` and daemon of a
  session still open elsewhere, and deleted their binaries; that session's host then relaunched its
  own version, which did the same in return. It now stops nothing. It installs under
  `~/.cache/basemind/versions/<version>/` and never installs into or launches from `bin/`, where
  the launchers up to 0.33 kill and delete, so one of those left installed no longer reaches a
  newer version; a version left under `bin/` is removed by the same rule as below once it was
  installed more than 14 days ago. On Git Bash, MSYS2 and Cygwin the running processes are read
  from `ps -W` and compared as Windows paths, where the POSIX `ps` options failed and nothing was
  ever pruned. It
  removes another cached version only when nothing has launched it for 14 days (each launch writes
  a `.last-used` marker; `BASEMIND_PRUNE_AFTER_DAYS` changes the number), no running process was
  started from it (matched on the whole command line, so a cache path with spaces counts, and on
  the resolved path) and no install holds its lock, and none at all when the process list cannot
  be read. A version directory that is a symlink is never removed, nor is anything behind it: on
  macOS `rm -rf` on such an entry emptied the directory it pointed at. A cache path holding a
  backslash is matched as it is. Install locks and staging directories of other versions are
  removed only after ten minutes untouched, a staging directory only once the process installing
  into it is gone.
- Storage soundness. The first scan after upgrading re-extracts every file once (see
  [`docs/UPGRADING.md`](docs/UPGRADING.md)).
  - The blob store is namespaced by blob schema (`cache/blobs/v<schema>/`): two installed versions
    no longer overwrite each other's blobs. This ships with the 0.34 release-minor bump: the sweep
    removes the un-namespaced blobs older builds wrote and the namespaces of older schemas, but
    never while a process of that version is registered under the data home (a daemon in
    `daemons/` of any generation, or a host in `hosting/<pid>`; both now record the blob schema
    they read, and a 0.33 record is read by its version; while a live one's namespace cannot be
    told, as from a hosting marker that names no build, or a daemon record cannot be read, no
    older namespace is reaped), and
    only once nothing has been written into them for `BASEMIND_PRUNE_AFTER_DAYS` days (default 14,
    the launcher's retention of an unused version; `0` leaves only the running-process check). A
    0.33 daemon still serving another repository through the upgrade keeps answering; an older
    basemind whose blobs were reaped re-extracts on its next scan. A file map records the blob
    layout it was written for, and one from before the namespaces is discarded with its index at
    the first open, without relying on that bump. A file whose blob is missing or damaged says so
    instead of "not indexed", and the `index_rescan` it names rebuilds the blob: a rescan of named paths
    (`index_rescan paths`, `basemind rescan <paths>`) validates the blob of a named file it would
    otherwise skip as unchanged. A watcher batch does not pay that read.
  - An older basemind sharing the cache with a newer one leaves the newer one's data alone: a blob
    namespace of a newer schema is not counted towards the cache budget and is never swept, a
    workspace whose view a newer version wrote is never evicted (reason `newer_version`) and does
    not stop the sweep, and a newer file map this version cannot decode is read as newer, not as
    damaged. Namespaces older than the current one that are past their hold are reaped even when
    the current one cannot be swept.
  - `cache stats` measures the whole blob store (every namespace, and un-namespaced blobs), reports
    the current namespace beside it (`blobs_current_bytes`), reports the memory vector stores
    (`memory_vectors_bytes`) and finds the last sweep again (`last_gc`). `cache clear blobs` clears
    the current namespace, older namespaces and un-namespaced blobs, and leaves a newer basemind's
    namespace in place, naming it. The cache budget counts everything under `cache/blobs/` but the
    namespaces of a newer schema, and every workspace but those holding a newer basemind's view.
  - Vector rows a scan could not write (a killed process, a LanceDB error) stay recorded in
    `pending-vectors.msgpack` beside the file map and are written by a later scan, incremental
    ones included. They were lost for good and search missed the file. A file no rows can come of
    (its blob is gone or carries no vectors of the current model, or the embed policy or chunk cap
    now excludes it) leaves that record instead of failing its batch on every scan. A full scan
    writes all of them, an incremental or a cancelled one 64.
  - A vector store that was wiped or deleted on its own (another embedding model opened it, `cache
    clear`, an unreadable `meta.json`) gets its rows back: a scan that finds another store where the
    one it wrote to was owes every indexed file its rows again and rewrites them from the cached
    blobs. It stayed empty until file contents or the embed policy changed.
  - After an unclean shutdown the next scan of a workspace is a full one that validates the
    combined blob and the document blob of each file instead of
    trusting them for existing, so one torn by a power loss is rebuilt, and rewrites the vector
    rows from the blobs. Unclean means: a scan of that workspace died without finishing, or the
    machine went down under a daemon (its marker names the boot it was started in, by the
    platform's per-boot id, and the daemon's process start, so a pid reused after a restart is not
    taken for a live daemon and a clock step does not end a live one; then every workspace
    verifies, whichever process opens it next). A daemon killed while the machine stayed up tore
    nothing and asks for no verification. A worktree seeded from a workspace that must verify its
    blobs, or whose last scan died and left its breadcrumb, must verify them too. A chunk
    sidecar's header, which states its length, is read on every pass. Not covered: a power loss
    after a scan finished outside the daemon, the payload of a chunk sidecar, and
    resolved-reference (`.rref`) blobs.
  - `memory_search` by meaning answers from the note itself and never from a stale, orphaned or
    duplicated vector row. The rows are rebuilt from the notes after an upgrade or for a new
    embedding preset, a bounded number per call: while they are behind, the response carries a
    `notice`. They are shared by every worktree of a clone, one store per embedding model, so two
    worktrees with different presets no longer wipe each other's. `memory_save` is fsynced before
    it returns. A note saved again while its vector is being rebuilt keeps the row of the later
    save, and a note whose embedding failed is embedded by a later search instead of staying
    unfindable until it is saved again, in whichever process the note was saved. A note saved from
    a worktree on another embedding preset is found by the next search, not up to an hour later.
    A vector row no longer holds the note's text.
  - The memory-notes store has its own version. A store written by a newer basemind is refused
    for memory operations ("written by a newer basemind") and never wiped; the code index of the
    workspace opens and serves as usual.
  - An open that does not hold the workspace lock writes nothing: no version stamp in the index or
    the notes store, no journal flush, no placeholder store in the view directory.
  - A rebuild interrupted after its emptied file map was written is finished by the next scan,
    incremental ones included, and the blobs it re-indexes from stay pinned against the sweep
    until it is done; a deleted file whose index rows could not be removed stays in the file map
    without keeping the other removals out of it; a repository whose files define no symbols is
    no longer rescanned on every open.
  - A damaged index database is replaced by the next writer open instead of failing every open, and
    a read-only open no longer wipes an index of an older schema: `index_status` reports
    `rescan_needed`, and a CLI query prints one line on stderr naming `basemind scan`.
  - The blob sweep can no longer run during a scan the daemon started by itself (a hosted
    rescan, the rebuild of a freshly opened workspace), and the sweep after a budget eviction
    keeps the grace period while another process holds a workspace's writer lock.
  - A workspace cache directory is renamed aside before it is deleted, and an opener that was
    waiting on its lock no longer ends up holding the lock file of the deleted directory. A lock
    is released by an explicit unlock, not only by closing its file: a child process forked by
    another thread kept it held for a moment, and the next open found the workspace locked.
  - The vector store's `meta.json` is written atomically, before any table, and removed last in a
    wipe; one written by a newer basemind is refused even when this version cannot parse it. Two
    processes opening a new store at once no longer share a temp file or remove each other's
    tables. An open that waits more than 30 s for another open of the store fails instead of
    opening beside one that may be wiping the store. The rest of that scan pass, and the tool calls
    of the next minute, take the store as unavailable at once instead of each waiting again; so do
    calls that were already waiting on the open that failed. A hybrid `code_search` runs its other
    lanes and names the vector lane in `lanes_unavailable`. `memory_search` by `query` and
    `code_search` with `lane: "semantic"` refuse the call (`isError`, not an internal error): the
    refusal says the vector store is unavailable and why, and when the store is opened again;
    `code_search` points to `lane: "keyword"` and `"hybrid"`, `memory_search` to reads by `key` or
    `prefix`. `docs_search` opens its own document store and is refused only when that store does
    not open; without the embedder it answers on its lexical lane.
- `memory_save` refuses a key longer than 1024 bytes and a note whose text and tags pass 4 MiB,
  with an invalid-params error naming the limit: a longer one could be saved locally but not
  carried in one daemon frame, by its save or by any later read. Reads of several notes or stamps
  sent to the daemon are split by the bytes of their keys as well as by count, so keys saved
  before the limit cannot make a request larger than a frame; read in the process itself, such a
  key is read as before. A note saved before the limit and too long for a daemon reply is refused
  by name, with how to fix it, when read by key, instead of breaking the connection; a search
  through the daemon leaves it out and the daemon logs it, where it failed every search whose hits
  or embedding batch included it.
- The daemon protocol changes (part of the comms protocol's step from 4 to 7): a search by meaning reads the `updated_at` of its
  hits' notes, and whole only the notes it may serve, in batches (at most 256 keys and 8 MiB of
  notes per reply, which the daemon enforces), and a rescan says whether its paths were named or
  watched. A client and a daemon of different protocol versions refuse each other at connect, as
  before. A 0.33 install never meets this daemon (it runs in another protocol generation). Within
  the generation, the newer client asks an older daemon to stop, and gets no daemon while that one
  refuses because it serves sessions: close those sessions or run `basemind comms stop`. An older
  client that finds this daemon is told to update and never takes it over.
- The agent comms store is still versioned by the release minor, so the 0.34 bump empties it once
  more.
- The `docs_search` `keywords` and `entity` filters work, before ranking (ADR-0029): they were matched
  against metadata looked up in the code file map, where no document lives.
- A same-size edit is detected as stale: the content hash decides when the size and timestamp do not.
- Symbol history no longer reports the edge of its window as `introduced`.
- A page of `git_log` with a directory `path`, answered before the git-history index is ready, walks to its
  window without listing the files of the commits before it and stores nothing in the git cache. Each page
  walked from HEAD listing every commit's files and wrote a larger log to disk.
- A `git_log` cursor is refused when the git-history index came up (or stopped answering) since it was
  issued, and the error says to restart without `cursor`: the index and the live walk order the commits a sync
  appended differently, so the offset could skip or repeat commits. Recent commits and a file's history had no
  such check.
- A page of recent commits, of a file's history or of a symbol's history walks to its page alone and stores
  nothing in the git cache, like a directory's. Each page walked from HEAD again, wrote a larger log to disk,
  and recent commits listed the files of every commit before the page; paging also stopped at 10,000 commits.
- A commit the history walk cannot read (missing from the object store or corrupt) is an error from `git_log`
  and the git-history index build. It was skipped, which ended the history there with nothing to say the list
  was short. A shallow clone's boundary is not such a commit: the walk honours `.git/shallow`.
- A commit's file list on `git_log` lists the source of a moved file as `deleted` beside its destination as
  `renamed`. Only the destination was listed, so the history of the directory a file moved out of missed the
  move, and the git-history index's history of the old path missed it where the live walk listed it.
- `git_log {path}` no longer lists a merge that took the path unchanged from one side: a merge touches a
  path, and lists it among its files, only when it differs there from every parent, as `git log --cc`
  has it. Every other commit that changes the path is listed: unlike `git log -- <path>`, no side of a
  merge is pruned. On tokio, a file's history was more than half such merges.
- A commit's file list on `git_log` no longer names the directories above a changed file (`src` as
  `modified`).
- The git-history index and the git cache rebuild their file lists once, so the three fixes above (and the
  moved file's source) reach a repository indexed by an earlier build.
- `find_files` finds the file a query names up to one typo (`buidl` finds `build.rs`, `SCEURITY` finds
  `SECURITY.md`) when no path holds the query as typed; a transposed letter matched nothing, or only
  paths holding the letters in order. It also covers every file git tracks that the scan does not index
  (images, fonts, and files the scan was told to leave out, as `git ls-files` lists them), in a search
  and in a listing; an entry a sparse checkout left out of the working tree is not listed. A query that
  holds whitespace or a `!`, `^`, `$` or `'` operator gets no typo pass. A page asked for after
  `git add` or `git rm` changed that listing reports `cursor_invalidated`.
- `basemind git by-path` refuses `--window 0` and `--limit 0`, which returned the same `next_cursor`
  forever.
- `code_refs` answered by a daemon that predates exact matching no longer sets `truncated_reason:
  "scan_cap"` when it drops that daemon's substring hits: no cap was hit. `total` is then an upper bound.
- `code_grep` with a regex that names a carriage return through an escape (`\x0D`, `\x{0D}`, `\u000D`,
  `[\x0d]`) matches a lone `\r` again; only a literal `\r` or the `\r` escape was recognised.
- `code_grep` with a regex that names the vertical tab (`\v`, `\x0B`, or a class range holding it and not
  `\r`) no longer matches a lone `\r`, which is masked as that character to keep it from ending a line: `a\vb`
  matched `a\rb`, and `[^\x00-\x0B]` missed a `\r`.
- A `code_search` or `docs_search` ranking that finishes after a rescan no longer evicts the rankings of the
  newer index generation, and neither does a lookup that started before the rescan.
- `code_symbols`, `code_grep` and `code_read` leave `truncated` out when it is false, as every other tool but
  a `memory_search` listing does.
- The parsed-outline cache behind symbol history, `git_blame {symbol}` and the outline of an edited file is
  held to 64 MiB as well as 512 entries. Each entry holds a whole file, so the count alone let 512 files of up
  to `scan.max_file_bytes` stay resident.
- `code_refs` leaves `resolved` absent when nothing was checked, instead of `false`.
- `code_grep` and `code_refs` definition no longer read through a file swapped for a link out of the
  repository, and `code_grep` no longer counts matches through a directory swapped for one: the count
  answered "does the file outside match this pattern".
- `code_refs` with `path`, and `relation: "definition"`, no longer lay indexed positions over a file edited
  since it was indexed, which reported another line under the right name.
- A structural diff's `common` no longer counts a repeated symbol twice, and `code_read` keeps a line that
  ends exactly at its size cap.
- Blocking tool work (file reads, blob reads, git walks) runs off the async reactor.
- `find_files` (previously `code` modes `find` and `files`) no longer misses document-tier files
  (markdown, config, PDFs) on a long-lived server.
- `index_status.cache_dir` reports basemind's cache directory. It reported the tree-sitter grammar
  directory.
- Fast calls are no longer returned as background tasks. A call that took 6 to 27 ms was answered with a
  pollable task handle because eligibility was decided by tool name alone; a Task is now created only for a
  call still running after 3 s.
- Unknown arguments are no longer silently dropped. A `grep` call carrying an unsupported `glob` key ran
  unscoped and reported success; it is now an error that lists the accepted arguments.
- Caller lookups no longer count call sites of longer names: callers of `open` included every call to
  `open_store`, because the name was matched as a substring.
- A bulk index build no longer leaves hundreds of MB of sealed Fjall journal on disk. A sealed journal is
  deleted only once every keyspace it covers has flushed, and lightly written keyspaces such as `meta` never
  did, so each 64 MiB journal lived forever. A scan (full or incremental) now flushes every index keyspace
  after its durability sync whenever Fjall holds a sealed journal, and logs a warning instead of failing when
  the flush errors. Scanning 5,256 source files left 4 journals (268 MB, 320 MB index) and now leaves the
  active one (24 MB, 79 MB index); the scan was not slower. An index too small to seal a journal is
  unchanged: Fjall cannot shrink its active journal (about 13 MB of the 15 MB index of this repository's
  `src` and `tests`), and flushing it would only slow the scan.
- A crash or power loss can no longer leave files permanently missing from the code index. Index writes
  reached the OS but were not fsynced, while the file map (`index.msgpack`) was, so after an unclean shutdown
  the file map could record a file as indexed and unchanged whose index rows never reached the disk, and
  every later scan skipped it. The index journal is now fsynced once per scan before the file map is
  written, both carry a scan generation, and an index stamped below its file map is discarded and rebuilt
  from the cached blobs on the next writer open, or emptied and rebuilt by the next scan into a store that
  was opened read-only; the file map is never written over an index it is ahead of. The rows a scan writes
  after that point (resolved references, keyword statistics) are fsynced once more at its end. A file whose
  index batch or keyword postings failed to commit is no longer recorded in the file map (it is reported as
  a failed extraction and indexed by the next scan), and the file map's directory is fsynced after the
  rename. Discarding an index (also on a schema change) renames it aside before deleting it, so a crash
  midway no longer leaves a directory that cannot be opened. The emptied file map is written as soon as an
  index is discarded or emptied, so a rebuild that is interrupted is never taken for a complete index. File
  maps written before this change open unchanged.
- An older basemind no longer wipes a cache a newer one wrote. Two binaries of different versions sharing
  the machine-global cache used to destroy each other's code index, git-history index, comms store and
  LanceDB store on every open, and read-only commands reported the newer index as empty ("file not
  indexed"). A store stamped with a newer schema is now left untouched and the open fails with
  `index written by a newer basemind (schema N); this binary understands M — upgrade basemind`. A store
  stamped with an older schema is still wiped and rebuilt.
- `agents` mode `inbox` with `mark_read` no longer advances a read cursor past messages the `since_hours`
  window hid, so a message older than the window is still returned by a later read instead of being consumed
  unseen. A thread holding such a message keeps its newer messages unread until the older one is read (pass
  `since_hours=0`) or acknowledged.
- `agents` mode `wait` with `thread` no longer returns on unread messages in other threads. Its response gains
  `reason` (`message`, `timeout`, `discovery` or `shutdown`), which separates a discovery wake and a closed
  broker link from a real timeout; `timed_out` is unchanged.
- The comms daemon's startup and hourly retention sweep now takes the store write lock the request handlers
  use, so it can no longer overwrite a concurrent post, archive or membership change on a thread record.
- The daemon's cache sweep now reaps workspace cache dirs that have no `workspace.json` marker and hold only
  disposable leftovers. A comms CLI verb or hook run in a directory that is never scanned leaves a dir
  holding just `agent-id` (or `telemetry.jsonl`), and the reaper kept every marker-less dir, so they
  accumulated without bound. A marker-less dir is removed only when every entry is one of `agent-id`,
  `telemetry.jsonl`, `.lock` or `.lock.meta`, nothing in it was modified for 7 days and no process holds its
  lock; one with a view, a subdirectory or any other file is kept as before. Resolving the persisted CLI
  identity now refreshes `agent-id`'s mtime (at most daily), so an identity still in use is not reaped.
  `cache gc --dry-run` reports the candidates as `markerless_workspaces_reapable` and
  `markerless_workspace_bytes_reapable`.
- Rust trait impls whose trait is written with a path or generics (`impl<T> Extend<T> for Vec<T>`,
  `impl crate::scanner::ScanObserver for RescanPaths`) are implementations, keyed by the trait's last
  path segment. Only bare trait names were matched before.
- Rust turbofish calls (`s.parse::<u32>()`, `collect::<Vec<_>>()`, `a::b::make::<T>()`) are call sites.
- Calls inside Rust macro arguments (`assert!(is_ok(x))`) are call sites. tree-sitter leaves a macro
  body unparsed, so they are recognised lexically: an identifier directly followed by `(` inside a
  macro invocation, excluding `fn`/`struct`/`enum`/`union` definitions, `let` patterns, attributes
  (`#[cfg(all(..))]`), `cfg!` predicates, `Fn`/`FnMut`/`FnOnce` bounds, `matches!` patterns, nested
  macro names and `macro_rules!` bodies. Like every call they match by name only.
- Rust trait methods without a body and `extern` block functions (`function_signature_item`) are
  symbols of kind `function`.
- Rust `fn` items nested in a function body are symbols; other function locals stay out of the outline.
- Items declared inside item-position Rust macro bodies (tokio's `cfg_time! { pub(crate) fn … }`,
  `thread_local!`, `lazy_static!`) are symbols, imports and implementations. Such a body is re-parsed
  on its own once per file and kept only when it parses without errors (at most 256 KiB, 3 macro
  layers and 256 bodies per file); its calls then come from the calls query over that parse instead of
  the lexical match, so attributes, enum tuple variants and patterns there are not calls and turbofish
  calls are. `EXTRACT_EPOCH` is 2, so unchanged files are re-extracted on the next scan, including the
  calls of a blob that held them (also with `eager_l2 = false`) and code-search chunks the new outline
  cuts differently.
- `impl !Send for X {}` is not an implementation of `Send`, and `impl Tr for a::B<T>` is an `impl` symbol
  named `B`.
- A tool that pins a symbol by name without `line` (`code` `expand` and `callers`, `git` `blame_symbol`
  and `symbol_history`) picks the item-level definition when the other same-named ones are `fn` items
  nested in function bodies, and `symbol_history` follows a definition among those of the same
  nesting, so a nested `fn` of the same name added in a revision no longer shifts it.
- `symbol_history` with a structural `hash_mode` fingerprints a symbol declared inside a macro body on
  its own instead of hashing the whole macro body.
- A parse that hit its timeout no longer leaves the pooled parser to resume it on the next file.
- The git-history index runs on fixed memory, journal and open-file budgets (8 MiB memtables, a
  16 MiB cache, a 64 MiB journal, 64 cached files) instead of fjall's defaults of 64 MiB memtables,
  a 512 MiB journal and up to 900 cached files per repository the daemon holds. The index rebuilds
  once.
- A hosted workspace's idle clocks start when its last session ends, not when it connected, so a
  long session's read stack is no longer shed right after it closes.
- A forwarded CLI command no longer counts as a second `serve` session of the agent's identity, which
  made agent replies warn of a shared identity.
- A comms `Hello` from a registered root no longer enumerates the repository's worktrees and
  branches and rewrites the machine registry on every prompt hook. `workspace workspaces`,
  `worktrees` and `branches` reconcile the rows they list instead: a repository whose refs,
  `packed-refs` or `worktrees/` changed since it was enumerated is enumerated again, and rows whose
  path is gone are dropped. A root that resolves to the filesystem root is no longer registered.
- A daemon serving the HTTP front-end is no longer stopped for idleness. It disappeared ten minutes
  after its last request, and no HTTP client could start another.
- `basemind comms doctor --probe` reports a daemon whose agent store is not open as degraded.
- `BASEMIND_COMMS_DIR` naming an existing directory no longer has its mode changed to 0700; only the
  generation directory inside it is made owner-only.
- The daemon's exit path (shedding hosted stacks, the agent store's last flush) is bounded, and the
  waits on a draining predecessor cover the whole teardown.
- A hosted workspace's file watcher no longer wakes five times a second while nothing changes.
- A document over `[documents] max_chunks_per_document` is no longer embedded in full and then left
  unsearchable: whichever chunker cut it, it is not embedded, and its chunks are indexed without
  vectors, so `docs_search` finds it on the lexical lane. The scan summary counts such documents as
  `docs_over_chunk_cap`.
- A document blob another workspace rewrote under other `[documents]` settings or another model is no
  longer turned into this workspace's rows (which lost their vectors for good and were rewritten on
  every scan): the file is extracted again.
- An incremental scan (a watcher batch) that touched no document no longer lists every document of
  the store and runs index upkeep; upkeep runs only after rows were written or deleted. A full scan
  that re-queued owed code rows sweeps dead document rows again.
- `code_search` with `lane: "semantic"` refused with a fix naming `mode`, an argument it rejects; it
  names `lane`.
- A `code_search` ranking under the default `[code_search] embed = false` is kept for its pages and
  repeats, like any other.
- The cross-encoder reranker is released by the idle engine release like the embedder, and one that
  failed to load is not retried by every `docs_search` and `code_search` for 60 s.
- A `docs_search` reranker answer naming an unknown hit leaves the fused order as it was.
- `[documents.reranker] top_k` above 50 is refused instead of silently capped.
- An empty `memory_search` query is an invalid argument, and an embedding model that will not load
  makes `memory_search` and `code_search` unavailable, not an internal error.
- `docs_search` no longer wipes a document store of another model or version it cannot read: it
  refuses it, and the scan that holds the store rebuilds it.
- A git-history sync that appends commits no longer flushes and major-compacts all eight keyspaces:
  it syncs the journal, flushes only once 8 MiB of rows sit unflushed, and merges only a small
  keyspace holding more than one table. A one-commit append on a 2.7k-commit history went from
  about 530 ms to 23 ms (debug build); a full rebuild still compacts every keyspace.
- `code_search` and `docs_search` report an incomplete index like the other verb tools (#258): while
  the index warms, builds, rescans, rebuilds after a partial scan or is empty, the answer carries the
  same `notice` object (`state`, `message`, `retry`). On `docs_search` it takes the place of the
  document store's own `notice` text, so an answer has one.
- Closing stdin during the `basemind init` capability prompt (#328) accepts the default for that
  capability and every later one. It advertised the capability being asked about and dropped the
  rest.
- `git_diff` keeps a hunk line that is not valid UTF-8 (a file in Latin-1 or another legacy
  encoding), each invalid byte sequence read as U+FFFD, and sets `lossy_text: true` (#296). Such
  lines were dropped from the hunk text while its line counts still included them.
- docs.rs builds the crate (#274). 0.31.0, 0.31.1 and 0.32.0 failed there: the build script of
  tree-sitter-language-pack downloads its parser sources, and docs.rs has no network.
  `[package.metadata.docs.rs]` sets `TSLP_OFFLINE`, and CI builds the docs the same way, offline.
- Prose chunked from a text file now overlaps when its lines are longer than `[documents] overlap`:
  with no line start in a chunk's last `overlap` characters, the next chunk starts at the first word
  start there (#314). Before, an unwrapped paragraph or a one-line file got no overlap at all. The
  extraction digest is `doc/5`: the next scan extracts and embeds every document once.
- The daemon's watcher scans are bounded across workspaces (#281). A hosted workspace's watcher
  batch now takes one of the four scan slots a client's rescan takes, and the batches that arrive
  while it waits for one merge into the same pass, so twenty worktrees switching branches at once
  run four passes at a time instead of twenty, and each waiting workspace scans its batches once. The boot, catch-up, embedding and
  index-rebuild passes take a slot too. The wait holds no thread, and read-stack builds and
  term-index warms run on threads of their own, so an agent's read is not queued behind them. Up to
  one read-stack build per four cores, at most three, runs at once instead of one machine-wide,
  with one more thread kept for term-index warms.
  `index_status` gains `queued_scan` while a pass waits: the batches merged into it (`batches`), the
  paths it will scan (`paths`) and whether it rescans the whole tree (`whole_tree`).

## [0.33.0] - 2026-11-14

This minor bumps the persisted blob, index, git-history and LanceDB memory schema versions, so existing
`.basemind/` data is rebuilt on the next scan.

### Added

- **Shared worktree indexes, opt-in with `BASEMIND_INDEX_LAYERED=1`** (ADR-0016, issue #87). Worktrees of one
  repository serve code, graph, git and document queries from an immutable base index of the committed tree
  plus a small per-worktree overlay for uncommitted edits, instead of each worktree holding a full index.
  Semantic search runs through verified shared LanceDB snapshots, the daemon refreshes overlays for connected
  sessions, and a verified standalone working view can be retired (`BASEMIND_INDEX_RECLAIM_BACKUP=1` also
  reclaims its backup). `BASEMIND_INDEX_MAX_GROUP_BYTES` caps a shared group's storage and
  `BASEMIND_INDEX_CACHE_BYTES` sizes the index cache. Without the variable, indexing is unchanged.

### Changed

- `xberg` 1.4.0, with the bundled ONNX Runtime now `ort` 2.0.0-rc.15; `crawlberg` 1.13.0, `oxc_*` 0.158.0,
  `rmcp` 3.10, `jsonschema` 0.61, `tree-sitter-language-pack` 1.25.0, `zstd` 0.14.4, `arc-swap` 1.9.5; website
  `astro` 7.4.7.
- Startup instructions direct agents to Basemind first for capabilities enabled during initialization.

### Removed

- **Prebuilt macOS Intel binaries.** Releases no longer include `basemind-x86_64-apple-darwin.tar.gz`. The
  npm and PyPI installers, the plugin launcher and the Homebrew formula now refuse Intel Macs with a clear
  message instead of fetching a missing asset, and the `ort-dynamic` Cargo feature, which only that build
  used, is gone. Apple Silicon, Linux (x86_64, arm64) and Windows (x86_64) are unchanged.

### Fixed

- The MCP `agents` message mode accepts the `message_ref` field returned by history and inbox.
- Index plugin resources so agents can discover their skills and instructions.
- BM25 keyword postings shared by identical files are retained, and legacy keyword postings are rebuilt on
  scan.
- The `intelligence` feature builds on its own again, with embeddings enabled.
- macOS builds with the `full` feature link clang's compiler-rt, which the bundled ONNX Runtime needs for
  `___isPlatformVersionAtLeast`; without it the release link failed.

### Security

- Use patched bundled SQLite and rustix via temporary, commit-pinned Xberg and RMUX backports.
  While these pins are in place the release workflow publishes platform binaries and package
  wrappers and skips crates.io. Registry dependencies can resume when the backports are published.

## [0.32.0] - 2026-11-06

### Changed

- Python 3.11 is now the minimum supported version; Python 3.10 reached end of life.
- Dependency upgrades: `crawlberg` 1.12.4, `oxc_*` 0.157.0, `rmcp` 3.9, `toml` 1.2, `tree-sitter-language-pack` 1.24.0, website `astro` 7.4.5 / `@astrojs/starlight` 0.42.9, plugin wrapper `@opencode/plugin` 2.2.5, PyPI wrapper `certifi` floor 2026.10.23. `arrow` stays at 58 (the LanceDB 0.41 type identity), `bincode` at 2.0.1 (3.0.0 is a `compile_error!` placeholder), and transitive `liter-llm` is pinned at 2.3.0 because `xberg` 1.3.10 does not compile against the in-range 2.6.x.

## [0.31.1] - 2026-11-01

A patch release with no schema change, so `.basemind/` is kept.

### Changed

- **Tasks and tools run through mise.** `mise.toml` and `mise.lock` pin poly, Python, uv, Node and pnpm at
  the xberg-io/toolchains v1.12.3 baseline, and every Taskfile task is now `mise run <task>` under the same
  name; `release:sync-version` takes the version as an argument (`mise run release:sync-version X.Y.Z`).
  CI installs tools with `setup-mise`, gains the `check-toolchain` gate, and builds the macOS release
  binaries on macos-26 with `MACOSX_DEPLOYMENT_TARGET=14.0`.
- **Plugin bundles are regenerated with ai-rulez 5.9.8.** Both `.ai-rulez/` trees are converted to OKF, and
  the bundles change only in provenance headers and index lists; the plugin version moves with the crate so
  cached installs pick them up. ai-rulez is no longer pinned in CI.
- Dependencies: `lancedb` 0.41, `fjall` 3.1.13, `rmcp` 3.8, `crawlberg` 1.12.3, `oxc_*` 0.156, plus
  compatible lockfile updates. `liter-llm` is held at 2.3.0: `xberg` 1.3.10 (the latest) does not build
  against 2.4's `RerankRequest` / `EmbeddingRequest` / `LlmConfig` fields. `arrow` stays at 58 because
  `lancedb` 0.41 still requires `^58` (arrow 60 forks the array types and breaks `Scannable`), and
  `bincode` stays on 2.x because 3.0.0 is a `compile_error!` placeholder for the unmaintained crate.

## [0.31.0] - 2026-10-23

This minor bumps the persisted blob, index and git-history schema versions, so `.basemind/` is wiped and
rebuilt on the next scan.

### Changed

- **`grep_bloom` rows are keyed by content hash.** The `code grep` trigram prefilter (ADR-0012) stored one row
  per path; it now stores one row per distinct file content, shared by every path holding the same bytes, and
  the live `(size, mtime_ns)` stamp moves to the file map. Older path-keyed rows read as "candidate" (never a
  false skip) and are dropped by the next full scan's sweep, which also backfills rows for unchanged files, so
  the layout change needs no wipe of its own (`ROW_VERSION` 2).
- Index reads go through a backend-neutral `IndexView` trait (`src/index/view.rs`) instead of calling the
  Fjall keyspaces directly; the named reads (`references_to`, `definition_of`, `importers_probing`, ...) are
  built on its generic prefix/range layer. No behaviour change; this is the read seam the layered index
  (ADR-0016) overlays on.
- **ADR-0016 specifies phases A2–A8 of the shared base index and worktree overlays**, with its defaults sized
  from measurements on a large reference monorepo, and `docs/layered-index-design.md` points at the ADR where
  the two diverge.
- The pinned `ai-rulez` moves to **5.5.3** everywhere: the validation, publish and plugin-artifact workflows and
  both lock files. Strict validation demotes the AR602/AR701/AR703/AR801 codes that fire on the intentionally
  mirrored skill trees and launchers.
- Dependencies: `rmcp` 3.7, `gix` 0.90 (its `max-performance` is the old `max-performance-safe`), `jsonschema`
  0.60, `crawlberg` 1.12, `oxc_*` 0.155, `lru` 0.19, `arc-swap` 1.9.3, plus compatible lockfile updates. `arrow`
  stays at 58 (lancedb 0.40 is the latest and requires `^58`), `bincode` at 2.x (3.0.0 is a placeholder release),
  `rustix` at 1.1.4 (rmux-client). The OpenCode plugin depends on `@opencode/plugin` ^2.1.2; the Python wrapper
  requires `certifi>=2026.10.19` (distrusted emSign roots). The Hermes plugin stays on `mcp` 1.30.0, the latest
  1.x: Hermes Agent pins `mcp==1.26.0` and 2.x renames `McpError`.
- CI actions move to their current majors (`checkout` v7, `setup-node` / `setup-python` / `upload-artifact` v7,
  `upload-pages-artifact` / `deploy-pages` v5, `pnpm/action-setup` v6, `setup-uv` v10.3.1).

### Fixed

- **`grep_bloom` no longer grows without bound under the watcher.** An incremental scan drops the rows of
  content no path holds any more (an edit, delete or rename), not only a full scan's sweep.
- **A file written in the same clock tick as the scan can no longer be skipped by `code grep`.** A stamp taken
  within 2 s of the clock is recorded as unknown, so the bloom is not trusted for that file until a later scan
  sees it settled.
- The release workflow's plugin jobs pinned ai-rulez 5.3.0 against 5.5.3 lock files, so `lock --check` failed
  and v0.30.0's agent-runtime archives, marketplaces and npm agent plugins never published. The version guard
  also required a `marketplace.json` version key that ai-rulez no longer emits, and the Hermes check read a
  path that does not exist in the job, so it never ran.
- The Hermes plugin's source config pins `mcp==1.30.0`, matching the generated package.
- Security: `rustls-webpki` 0.103.16, `webpki-roots` / `webpki-root-certs` 1.0.10 (RUSTSEC-2026-0382,
  RUSTSEC-2026-0383: distrusted emSign intermediates).
- The document-tier tests use `*.svg` fixtures, which 0.30.0's default `[documents] exclude` drops; they now
  clear the exclude list, so the full-feature test suite is green again.
- Linux scans no longer crawl at about five seconds per file when the process runs in a cgroup with no
  enforced memory limit: a CI runner's service or a desktop session's scope. The memory ceiling compared
  that shared cgroup's working set, which includes other processes and active page cache, with a quarter of
  RAM, so the drive governor waited out every admit and narrowed to one file per chunk. Without an
  enforced limit the reading now uses the process's own RSS, as it already did with no cgroup. This is
  also what made the Ubuntu CI test legs time out.
- The npm agent plugin (`@goldziher/basemind-plugin`) never had a trusted publisher, so its OIDC token
  exchange returned 404 and the 0.30.0 and 0.31.0 release runs failed on it; the publisher is configured and
  the workflow's bootstrap error now points at the setup steps in `plugin/README.md`.
- Gemini CLI and OpenCode agents get real model ids (`flash` / `pro`, `anthropic/claude-sonnet-5-5` /
  `anthropic/claude-opus-5-5`) instead of silently inheriting the session model, which they did because
  neither harness accepts the Claude `sonnet` / `opus` aliases.

## [0.30.0] - 2026-10-18

This minor bumps the persisted blob, index and git-history schema versions, so `.basemind/` is wiped and
rebuilt on the next scan.

### Changed

- Dependencies: `xberg` 1.3.10, `crawlberg` 1.11.1, `zstd` 0.14.3, `jsonschema` 0.59, plus compatible
  lockfile updates. `arrow` stays at 58 (lancedb 0.40 requires `^58`, and a second arrow major forks the
  schema/array types), `bincode` at 2.x (3.0.0 is a placeholder release).
- The pinned `ai-rulez` moves from 5.3.0 to **5.5.2** (the lock files and the validation workflow), which
  adds the `adr-workflow`, `config-placement` and `skill-authoring` skills and the
  `customer-data-boundary`, `adr-immutability` and `generated-files` rules. The Hermes plugin's source
  config pins `mcp==1.30.0`.
- **Smaller code and git-history indexes** ([#82](https://github.com/Goldziher/basemind/issues/82)).
  Every index keyspace now uses LZ4 for data and index blocks at all levels; fjall's default left L0 and L1,
  where a fresh bulk build mostly lives, uncompressed. A database opened empty also gets a 256 MiB journal cap
  (instead of 64 MiB) so a cold scan rotates far fewer small tables. The index and git-history schema versions
  bump, so existing indexes rebuild once on next open.
- **`[documents] exclude` now defaults to SVG assets, lockfiles and test data** ([#83](https://github.com/Goldziher/basemind/issues/83)).
  The default drops `*.svg`, `pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`, `Cargo.lock`, `poetry.lock`, `*.lock`,
  `**/test_data/**`, `**/testdata/**`, `**/fixtures/**` and `**/__snapshots__/**`. SVG markup is not prose — the linear
  chunker turned icon/vector files into meaningless vector rows, so they no longer reach the document tier. Setting the key
  replaces the list; `exclude = []` restores the old behaviour.
- Structured data (json, yaml, xml, svg, csv/tsv) is always chunked by the linear chunker instead of xberg's quadratic
  markdown splitter, whatever its size. Markdown and prose keep the splitter under the `*_chunk_max_bytes` cutovers.
- `docs_degraded` counts only size-triggered linear chunking and the text/plain fallback; structured data, which is
  linear by design, no longer inflates it.
- A read-only reader whose repo group's memory store is held by another process (the daemon) keeps its code index
  instead of dropping to blob-only reads; memory operations on that handle are refused and go through the daemon.
- The `✓ doc` progress line shows how long each document took.
- **The auto memory ceiling is 25% of machine RAM, floored at 2048 MiB** ([#85](https://github.com/Goldziher/basemind/issues/85)).
  `max_footprint_mb = "auto"` budgeted 50% of RAM, which is 48 GB on a 96 GB workstation and never engaged on a
  scan that peaked at 6-8 GB. An enforced cgroup limit still gets 75%, now with the same 2048 MiB floor (was 512).
  Set `max_footprint_mb` explicitly to restore a different budget. The `memory` log line's
  `ceiling_enforced` reports whether the OS enforces the limit (a cgroup), not whether the gate runs; the gate
  parks document extraction and embedding workers over the ceiling whenever it is non-zero.
- `basemind scan` and `rescan` print one `peak memory` line with the peak footprint of each scan phase
  (`candidates`, `extract`, `purge`, `flush`, `resolve`, `lanes`), and `ScanReport` carries them as `phase_peaks`.
  Memtable flush concurrency is already bounded by the fixed fjall worker-thread count.
- **Parse-error warnings collapse to one line per grammar** ([#86](https://github.com/Goldziher/basemind/issues/86)).
  A scan of a tree with many `scss` or `sql` files no longer prints one `warn ... (N parse errors, partial map)`
  line per file; it prints `<grammar>: N files, M parse errors` once at the end. `-v` keeps the per-file lines.

### Fixed

- **An index wiped by a schema revision bump now rebuilds instead of answering empty.** After the partition
  revision bump the fjall index was recreated empty while `index.msgpack` survived, and a daemon-held workspace
  scheduled no rescan, so `references` / `callers` / `implementations` returned zero results silently. Opening
  such a workspace (wiped or empty index over a populated file map) now drops the file map so the scan
  re-extracts every file, the daemon queues a full scan and marks `status.json` degraded, and reads and
  `admin status` carry an `index_rebuilding` notice until it completes.
- **A full disk or poisoned index no longer leaves a scan reporting complete** ([#79](https://github.com/Goldziher/basemind/issues/79)).
  Index commits that failed with `StorageFull` or `Poisoned` were logged as warnings while the scan ran on and
  `status.json` read a full file count over half-written `refs_by_*` partitions. Both are now fatal: the scan
  stops, returns an error and records itself as partial. A scan and its resolve pass first check free space
  against 1.5x the current index size plus 512 MiB of headroom (`BASEMIND_MIN_FREE_MB` sets the headroom, `0`
  keeps only the proportional part) and refuse when the volume is short. `status.json` gains a
  `resolve_complete` marker that is set only after the resolve pass commits, and `admin status` (`degraded`)
  and `basemind doctor` report a missing one. The comms daemon reopens a poisoned code-index handle after the
  scan that hit it, and rescans a workspace whose last scan was degraded when it next opens.
- **A cancelled scan's partial file count is flagged** ([#84](https://github.com/Goldziher/basemind/issues/84)).
  A scan cancelled by daemon shutdown, idle reap or ownership loss flushed the files it had completed, and
  `admin status` reported that count as the workspace's. `status.json` now records `partial`, the reason and
  the progress (`progress_done` of `progress_total`); `admin status` surfaces `partial` and `degraded`, and
  `basemind doctor` warns. The next full scan clears it.
- **Cache budget eviction no longer deletes user memory and proposals** ([#80](https://github.com/Goldziher/basemind/issues/80)).
  `memory_by_key`, `memory_archive` and `proposals` moved out of each view's `index.fjall` into a per-repo-group
  store at `cache/repos/<group>/memory.fjall`, shared by every worktree of a clone (non-git roots use their own
  workspace key) and never listed by GC. On open, rows left in a view are merged into it, last writer wins by
  timestamp, and removed from the view. Worktree seeding no longer forks memory.
- **The document phase no longer burns its budget on low-value files** ([#83](https://github.com/Goldziher/basemind/issues/83)).
  The phase budget is checked before a document is announced as linearly chunked, and the per-file "deferred" warnings are
  replaced by one summary count at the end of the phase (per-file detail stays at debug level).
- Blob GC removes stray `*.tmp` files (staging files left by a dead writer) from the global `blobs/` directory
  once they are older than the six-hour grace window. Age is the only test, so a file still being written is kept.
- **Incremental rescans re-stitch cross-file references exactly, and only for the files they affect** ([#81](https://github.com/Goldziher/basemind/issues/81)).
  The watcher's re-stitch read every resolver-language file's blob to find the importers of a changed file, and
  followed only one hop, so a file behind a Python `__init__.py` re-export (or a deleted target, a new file
  shadowing a resolved module, a `tsconfig.json` edit) left its importers with stale `refs_by_def` /
  `refs_by_path` edges until the next full scan. The stitch now records every path the resolver consulted per
  importer (two new keyspaces, `import_targets` and `importers_by_probe`), and a rescan re-stitches the changed
  paths plus those importers: O(affected), equal to a full pass. An existing index runs one wholesale resolve
  pass on its first rescan to build the probe rows; no wipe. The watcher now forwards `tsconfig*.json` and
  `package.json` edits so a config change reaches the importers that read it.

## [0.29.3] - 2026-10-13

### Added

- **`git` mode `show` / `basemind git show <sha>`** returns one commit with its full, untruncated message and
  changed files, the way back to a body that `search` cut.
- **Document-guard counters on every scan surface** ([#72](https://github.com/Goldziher/basemind/issues/72)).
  `docs_degraded` and `doc_timeouts` now travel through the daemon `Rescanned` reply, the MCP `rescan`
  result and `basemind rescan` when it is forwarded to the daemon, not just the in-process scan summary.
  The fields are additive and default to 0, so mixed old and new daemon/client pairs keep working
  (no `PROTO_VER` bump).
- **Agent Plugins distribution** ([#69](https://github.com/Goldziher/basemind/issues/69)). The repository
  root now carries a conformant [Agent Plugins](https://agent-plugins.org) 1.0.0 package (`plugin.json`,
  `mcp.json`, `skills/`), generated by ai-rulez (`generate --plugin`) and checked by
  `validate --strict`. Its MCP server runs `basemind serve` from `PATH`.

### Changed

- Dependencies: `xberg` 1.3.9, `tree-sitter-language-pack` 1.22.1, `oxc_*` 0.154, plus compatible lockfile updates. `ort` stays at 2.0.0-rc.13, `arrow` at 58 and `bincode` at 2.x.
- **Linear-chunker cutovers raised, measured on a release build with xberg 1.3.8.**
  `[documents] markdown_chunk_max_bytes` goes 256 KiB to 1 MiB and `plain_text_chunk_max_bytes` 128 KiB to
  4 MiB. The splitter is linear for ordinary text now; only a single markdown block dense with inline
  markup stays quadratic, and the linear fallback still bounds it. Documents already routed through the
  linear chunker keep their index entry until the file changes.
- **Document extraction bounds** ([#72](https://github.com/Goldziher/basemind/issues/72)). The default
  `[documents] extraction_timeout_secs` drops from 600 to 120, so a few documents timing out together no
  longer cost ten minutes each, and a new `phase_budget_secs` (default 1800) bounds the whole document
  phase of a scan. Once it is spent the remaining documents are deferred, not failed, and the next scan
  picks them up (`docs_deferred` in the scan summary). Raise `extraction_timeout_secs` if your corpus has
  legitimately slow documents (large scanned PDFs).
- **Timed-out documents are no longer retried on every scan.** A document that overruns its budget is
  recorded in the index by content hash together with the extraction limits (timeout, chunk cutovers,
  `max_pages` and the extraction settings). While both are unchanged the file is skipped and counted as
  `docs_tombstoned`; editing it, or changing a limit, retries it. Existing indexes stay readable (the
  record is an additive field).
- **Abandoned extraction threads are capped.** A timed-out extraction cannot be cancelled and keeps a
  core busy until it finishes. `[documents] max_abandoned_extractions` (default 4) caps how many may still
  run; at the cap further documents are deferred instead of spawning more. The live count is logged and
  shown as `abandoned_extractions` in the scan summary. Running extraction in a killable subprocess was
  evaluated and rejected, see ADR-0015.

### Fixed

- **Document scans no longer accumulate tens of thousands of LanceDB versions** ([#76](https://github.com/Goldziher/basemind/issues/76)).
  The `documents`, `doc_links` and `code_chunks` tables took one delete and one append commit per file, so a
  14,664-document scan produced ~28k versions per table and spent about two hours in commit latency. A scan now
  writes one delete plus one append per batch (256 files or 4,096 rows), skips the delete on an empty table,
  purges stale documents with bulk `IN (...)` deletes, and ends with a compaction and a prune of versions older
  than 60 seconds. The incremental and watch paths prune versions older than five minutes every 32 commits and
  compact every 128, so the table directories stay bounded. Existing bloated indexes shrink on their next scan.
- **`git` responses are bounded when commit messages are huge** ([#74](https://github.com/Goldziher/basemind/issues/74)).
  `git` mode `search` returned every hit's commit body whole; squash-merge bodies reach 285 KB, so one
  `limit` 100 call cost 2.6 MB. Each hit's body is now cut at `body_chars` (default 1000, `0` omits it)
  with a marker, `body_truncated` and `body_bytes`, and the commit-list modes (`search`, `recent`,
  `touching`, `by_path`) hold their output to `max_tokens` (default 20000), setting `budgeted` and
  resuming through `next_cursor`. Both are available on `basemind git` as `--body-chars` and
  `--max-tokens`.
- **Cache budget no longer evicts the workspace you just built** ([#75](https://github.com/Goldziher/basemind/issues/75)).
  A 28 GB workspace over the 20 GiB `BASEMIND_CACHE_BUDGET_MB` default was evicted about ten minutes after
  its scan, because the 24 h "hot" floor only reordered candidates. Budget eviction now skips a workspace
  that is locked by a live writer, the most recently used one across all worktrees, or built within a grace
  window (`BASEMIND_CACHE_GRACE_MINUTES`, default 360, `0` disables the window). The cache is left over
  budget, a warning is logged, `cache_stats.last_gc` reports `over_budget` with the new
  `protected_workspaces` list, and `basemind doctor` gains a `cache` row that warns about it. Raise
  `BASEMIND_CACHE_BUDGET_MB` to clear the warning.
- **Code-only scans no longer peak above 12 GB in the resolve phase** ([#77](https://github.com/Goldziher/basemind/issues/77)).
  The vendored stack-graphs stitcher polled its cancellation flag once per phase, and a phase extended
  every path of the previous one without limit, so the per-file step and path budgets were only compared
  between phases and a pathological Python or Java file could allocate gigabytes before they were ever
  consulted. A phase now performs at most 256 path extensions before the budgets are checked (unfinished
  work stays queued, so the paths found are unchanged), the stitcher has its own budget of 4,096 phases,
  and the resolve pass waits at the `max_footprint_mb` gate between chunks as the extraction phase does.
  A file that exhausts a budget still falls back to tree-sitter locals binding. `RUST_LOG=basemind=debug`
  logs each file's `steps`/`phases`/`paths` counters.
- **Short memory spikes are now visible in the scan log** ([#77](https://github.com/Goldziher/basemind/issues/77)).
  The footprint is sampled every 200 ms instead of once per 5 s line; each `memory` line adds
  `window_peak_mb` and `process_peak_rss_mb`, and a gibibyte jump over anything already logged writes a
  `spike=true` line immediately.
- **Text documents rejected by xberg are indexed as plain text instead of failing** ([#78](https://github.com/Goldziher/basemind/issues/78)).
  A monorepo scan lost 489 documents to extractor verdicts on readable text: Helm `templates/*.yaml`
  (Go templates, `Failed to parse YAML`), CSVs past the table-cell limit and SVG/YAML files with an
  oversized entity. When xberg fails with a parse or security-limit error on a text-like format, the file
  is re-read as `text/plain` through the linear chunker under the same extraction budget. These count as
  `docs_degraded` and, separately, as the new `docs_text_fallback` (scan summary, `rescan`, daemon
  `Rescanned` reply and MCP `rescan`; additive, defaults to 0). Binary formats, corrupt PDFs, IO errors and
  timeouts stay failures.

## [0.29.2] - 2026-10-12

### Added

- **Agent Plugins distribution** ([#69](https://github.com/Goldziher/basemind/issues/69)). The repository
  root now carries a conformant [Agent Plugins](https://agent-plugins.org) 1.0.0 package (`plugin.json`,
  `mcp.json`, `skills/`), generated by ai-rulez (`generate --plugin`) and checked by
  `validate --strict`. Its MCP server runs `basemind serve` from `PATH`.

### Fixed

- A daemon from an older build that ignores `Stop` no longer wedges new sessions with "a previous basemind comms daemon ... did not stop": the takeover now escalates to SIGTERM, then SIGKILL, by pid.

## [0.29.1] - 2026-10-11

Patch release: 0.29.0 shipped everywhere except crates.io, which rejected it. No index or blob format
change.

### Fixed

- **`basemind` 0.29.x can be published to crates.io.** The vendored forks `basemind-lsp-positions`,
  `basemind-tree-sitter-graph` and `basemind-tree-sitter-stack-graphs` had moved to tree-sitter 0.27
  without a version bump, so crates.io still served their 0.26 builds and `basemind` could not resolve a
  single `tree-sitter`. They are now 0.3.5, 0.12.1 and 0.10.1.
- Windows build: an unused constant and unused test imports failed the `-D warnings` build; unix-only
  test fixtures no longer compile on Windows.

## [0.29.0] - 2026-10-09

0.29.0 reshapes the CLI (one home for each command, a documented exit-code contract, honest output and
daemon forwarding), makes the code map code-only with a separate document tier, adds a trigram prefilter
for `code grep`, MCP resources, and a more robust comms protocol (v4). Read **Breaking changes** and
[Upgrading](website/src/content/docs/start/upgrading.mdx) before installing.

### Breaking changes

- **First scan after upgrading rebuilds the index.** `RELEASE_MINOR` goes 28 to 29, so every view's
  `index.msgpack` and Fjall index are reset on open and every file is re-extracted (blobs are
  overwritten in place at their content-hash path; the git cache refills lazily). The LanceDB document
  tables and the blob store are not wiped; orphaned blobs are reclaimed by the post-scan sweep. Stop
  the old daemon first with `basemind comms stop` so the new binary starts its own.

- **CLI renames and removals, no aliases kept** (the MCP `admin` tool is unchanged):

  | Removed | Use instead |
  | --- | --- |
  | `admin status` | `status` |
  | `admin rescan` | `rescan` (also takes `--no-git-history`, `--rebuild-git-history`; runs the root guard) |
  | `admin cache-stats` / `admin gc` / `admin cache-clear` | `cache stats` / `cache gc` / `cache clear` |
  | `admin delta` / `admin checkpoint` / `admin waste` | `delta` / `checkpoint` / `detect-waste` |
  | `graph map --include-churn` | `graph map --no-churn` (churn is on by default) |
  | `serve --git-cache-mem`, `--no-git-cache-disk`, `--no-watch`, `--documents-*` | none; `serve` is a stdio relay to the daemon and ignored them |

  `admin` keeps `repo`, `telemetry`, `compress`, `tokens` and `eval`.

- **Exit-code contract:** `0` ok, `1` error, `2` usage, `3` writer lock held by another process, `4`
  required daemon unreachable, `130` interrupted. `scan` and `rescan` used to print the lock notice and
  exit `0`; they now exit `3`. An invalid `rescan` path (outside the repo, or neither on disk nor
  indexed) and invalid tool parameters exit `2`.

- **`cache clear` needs `--yes`** (or a terminal confirmation) for every component except `git-cache`,
  refuses with exit `3` while a writer holds the workspace lock, and refuses `blobs` while the daemon
  runs. `blobs` is the machine-global store shared by every workspace.

- **`cache gc` deletes; `--dry-run` reports.** The old `admin gc` reported while `cache gc` was a no-op.

- **Output changes:** the human-mode timing footer moved from stdout to stderr (`--json` keeps
  `elapsed_us` / `startup_us`); human output no longer truncates strings to 200 characters (only table
  cells are shortened, and stderr says so).

- **`--format` accepts only `json` or `toon`** on `code semantic` and `memory documents`; the other
  closed-set flags are now value enums (see Added).

- **`agents` / `workspace` `--json` reshaped** to the MCP response types and pretty-printed. `agents
  list` gains `description`, `skills`, `first_seen`, `last_seen`; threads use `last_activity_micros`;
  message rows gain `age_secs` and `body_sha`; empty optionals are omitted.

- **`hook install` is safer, so old hooks should be refreshed.** It asks git for the hooks directory
  (honours `core.hooksPath` and linked worktrees), refuses to overwrite a pre-commit hook it did not
  write unless `--force` (the old one is kept as `pre-commit.bak`), and the generated hook is
  fail-open (exit 0 when `basemind` is missing or the scan fails). Re-run `hook install`.

- **Code map is code-only.** Markdown, reStructuredText, AsciiDoc, vimdoc, CSV, JSON, YAML, TOML, INI,
  XML, `.properties`, `.env`, diffs, `.gitignore` / `.gitattributes` and Fluent files go to the document
  tier (searchable with `memory documents`), so `symbols`, `outline` and `grep` no longer return README
  headings or config keys. `[documents] include` / `exclude` and the MIME allowlist gate them. Builds
  without the `documents` feature keep the old behaviour.

- **`code outline` / `symbols` no longer index names bound inside function, method or lambda bodies**
  (locals, nested functions); module- and class-level definitions are unchanged. Value-like symbols
  store a shorter signature (`MAX_RETRIES = 3` becomes `= 3`; initialisers cut at 80 characters).

- **MCP logging is retired** (SEP-2577): no `logging` capability, no `logging/setLevel`, and `rescan`
  no longer emits the `rescan_complete` log notification. Counts are in the tool result; progress
  notifications are unchanged.

- **MCP `memory` tool schema dropped ~26 `documents.*` / `llm.*` override fields** (`llm_api_key`,
  `llm_model`, `reranker_*`, `embedding_preset`, ...). They remain CLI flags, env vars and
  `basemind.toml` keys; `memory` mode `documents` keeps `format`, `max_tokens`, `mime_type`, `scope`,
  `entity_category` and `keywords_contains`. No API key travels as a tool argument any more.

- **`agents` `wait` is capped at 40 s** (was 300).

- **Comms protocol version 4:** requests carry ids. Stop the old daemon (`basemind comms stop`) after
  upgrading so clients and daemon speak the same version.

### Added

- `basemind doctor` (root, config, index, grammars, pre-commit hook and daemon checked with filesystem
  and process probes only; `--json`; exits 1 when a check fails), top-level `status`, `completions
  <shell>` (bash, zsh, fish, elvish, powershell) and `man`.
- `hook install --force`, `cache gc --dry-run`, `delta --new <FILE>`, `detect-waste --log <FILE>`.
- Value-enum flags (parse-time errors listing the choices, shell completion): `graph` `--direction` /
  `--edges` / `--algorithm` / `--granularity` / `--format`, `code semantic` `--lane` / `--format`, `git
  search --field`, `git symbol-history --hash-mode`, `memory proposals --kind`, `admin telemetry
  --window`, `admin compress --level`.
- CLI pagination and size controls passed straight to the MCP params: `--cursor` on `code`
  symbols / grep / files / find / references / callers / implementations, `git` recent / search /
  touching / by-path / blame / blame-symbol / symbol-history and `memory` list / proposals;
  `--max-tokens` on `code` outline / symbols / grep / files / find / references / callers /
  implementations / semantic and `memory documents`; `--rerank-top-k` on `code semantic`; `--format
  json|toon` on the `code` list modes and `memory documents`.
- CLI forwards to a running daemon: `memory`, references / callers / graph reads and the grep prefilter
  no longer fail with `memory_by_key index not available` or degrade to a truncating in-RAM
  projection. With no daemon and another process holding the index lock, the error names the remedy
  and exits `3`.
- Tool subcommand paths (including `git` paths and `graph --focus`) normalize to the repo-relative
  index key: absolute, `./`-prefixed and relative spellings are the same file. `rescan` also
  validates paths and routes through the daemon instead of racing it.
- CLI/MCP parity test (`tests/cli_parity/`): a capability table maps every MCP tool and mode to a CLI
  command (or declares it `cli_only` / `mcp_only` with a reason), compares per-mode parameters
  against reasoned exceptions, and checks that representative reads return equal `--json` payloads.
- MCP resources `basemind://status`, `basemind://repo/map` and the templates
  `basemind://outline/{path}` and `basemind://memory/{key}` (with `{path}` completion). Bodies equal
  the tool results; paths are validated against traversal; listed on demand; no subscribe support.
- Human-readable `title` on each of the nine tools.
- `agents post` takes an optional `idempotency_key` (generated when omitted); the daemon stores it per
  (agent, thread) for an hour and returns the original message id on a repeat. The stdio relay
  replays keyed posts and read-only requests once on a replacement backend instead of failing with
  `backend_restarted`.
- Cancellation: `code grep`, `code semantic`, `graph map`, `graph calls`, `memory documents` and
  `agents wait` honour `notifications/cancelled` (answering `-32800 request_cancelled`), and
  cancelling an offloaded task reaches the running tool. Mutating modes (`rescan`, `put`, ...) always
  run to completion.
- `web crawl` reports MCP progress per page; task `statusMessage` mirrors rescan and crawl progress.
- `code grep` trigram bloom prefilter: one bloom filter per indexed file (new `grep_bloom` keyspace,
  about 12.8 % of indexed code bytes) lets grep skip files that cannot match a required literal.
  Results, totals, hit order and cursors are identical to the full sweep, including for files edited
  since the last scan. No index wipe; blooms are built on the next scan. `BASEMIND_GREP_BLOOM=0`
  forces the full sweep. See ADR-0012.
- `EXTRACT_EPOCH`: extractor-output revision on every file entry and blob. The next scan re-extracts
  only stale files, so a patch release can fix extraction without a minor bump. The scan summary shows
  `tier_migrated` and `refreshed`. Epoch 1 refreshes signatures produced before the header-only fix.
- Comms: `BASEMIND_COMMS_REQUEST_TIMEOUT_SECS`, `BASEMIND_COMMS_HANDSHAKE_TIMEOUT_SECS` (default 30 s),
  `BASEMIND_RELAY_REQUEST_TIMEOUT_SECS`; the detached daemon logs to a size-rotated
  `<comms_dir>/daemon.log`, shown by `comms doctor`.
- `BASEMIND_BLOB_GC_GRACE_SECS` overrides the 6 hour age below which the blob sweep keeps a blob.

### Changed

- `code find` and `code files` cover every indexed path (code map plus document tier), labelling
  documents by grammar name or extension. `find` drops candidates scoring under 85 % of the best match.
  On a 90-task eval: recall 0.59 to 0.83, hit@1 0.58 to 0.82, precision 0.28 to 0.62.
- Outline eval precision rises from 0.67 to 1.00 and the mean outline response shrinks from 1384 to
  747 tokens with the narrower symbol scope.
- Token-savings estimates are recalibrated from measured eval ratios (outline 1.2x, symbols 25x,
  references and implementations 1.2x, docs 2.5x; callers, dependents, find and files claim no saving).
- Upgrading is self-cleaning: the blob sweep is tier-aware, and `cache gc` performs it (serialised
  machine-wide by `cache/gc.lock`; blobs younger than 6 hours are kept). A scan that migrated paths,
  re-extracted blobs or reset a view prints `cleanup: reclaimed N orphaned blob(s)`; the daemon does the
  same a minute later. The MCP `admin` mode `gc` stays a non-destructive report. See
  `docs/UPGRADING.md`.
- Performance: `symbols` and `dependents` answer from a resident term index; `references`,
  `callers` and `implementations` use a resident name dictionary (needles matching over 5 % of names
  fall back to the full walk); `code grep` reuses a per-thread read buffer. Results are unchanged.
- Comms daemon runtime has 8 workers (was 4) and liveness probes run off the async workers.
- The production `rmcp` dependency drops its default features. Dependencies upgraded: `lancedb` 0.37 to
  0.40 (with its `remote` feature; `lance` 10 to 13), `gix` 0.89, `oxc_*` 0.153, `crawlberg` 1.10,
  `xberg` 1.3.8 and `tree-sitter-language-pack` 1.21.3 (both now chunk in linear time, so the
  quadratic stalls behind the Fixed entries below are fixed at the source; the cutovers stay as a
  backstop), `zstd` 0.14.2, plus lockfile refreshes. `arrow-*` stays on 58 to match `lancedb` 0.40.
- Eval generators follow the new symbol scope; `gen_find.py` takes `--indexed FILE`.

### Fixed

- CLI `--format toon` crashed the renderer; human mode now renders the TOON response.
- `code references`, `callers` and `implementations` from a daemon-backed session are complete. They
  used to answer from an in-RAM projection capped at `[resources] max_map_cache_mb` and returned a
  truncated lower bound behind a `projections_capped` notice. The scan is now forwarded to the daemon
  (`IndexRead`); the projection is a last-resort fallback only.
- Ctrl-C during `scan` / `rescan` stops at a file boundary (completed batches stay committed, the stale
  purge is skipped), prints the summary and exits `130`; a second Ctrl-C exits immediately.
- Comms unresponsiveness: replies are correlated by request id, so an abandoned request (such as the
  delivery-notice probe timing out) can no longer leave a stale reply misread as `unexpected response
  shape`; requests and handshakes time out with a retryable `comms: broker unresponsive`, the relay
  answers `backend_timeout`, and a slow connect for one identity no longer blocks the others.
- Comms daemon stalls under slow disk: blocking `CommsStore` calls and registry writes run on the
  blocking pool (mutations stay serialized through one write gate, so `seq` order holds), and the hourly
  retention sweep moved off the runtime.
- The inbox read bounds its unread scan (`unread` is a lower bound past roughly 500 rows) and the
  delivery probe uses its own connection, at most once every 2 s.
- `agents wait` stops as soon as the host cancels or the connection closes, and no longer returns
  instantly on messages a previous `wait` already returned.
- A comms or shells daemon starting as the previous one exits no longer mistakes a just-released lock
  for a live peer and exits silently (the lock waits up to 300 ms).
- Symbol signatures in indentation-based languages end at the grammar's body field instead of embedding
  the whole body; existing indexes refresh through `EXTRACT_EPOCH`.
- macOS: the heap no longer shows up as GBs of "GPU" memory; mimalloc's default Mach VM tag 100
  (`VM_MEMORY_IOACCELERATOR`) is retagged to 254 at startup.
- A panic inside an embedding job no longer aborts the process; it surfaces as an `embedding worker
  panicked` error.
- Flaky `daemon_lock` and `store_layout` tests (a sibling test's fork/exec holding a released flock; a
  test mutating the process-global `BASEMIND_DATA_HOME`).
- Stale docs: `serve` flags, `memory mine` flags, `--json` help text and the admin command tables.

## [0.28.1] - 2026-10-06

### Added

- `basemind admin eval`: a repo-agnostic retrieval-quality and token-savings evaluation. It runs a
  JSONL task file (modes `symbols`, `outline`, `references`, `callers`, `grep`, `find`,
  `dependents`, `git_search`, `docs`) through the same in-process tool code the MCP server uses,
  scores answers against gold (set P/R/F1; hit@1/@5, MRR and nDCG for ranked tasks), records
  per-call latency and response tokens, and reports per-mode p50/p95 latency and quality as NDJSON,
  JSON and markdown. Tasks may carry a `baseline` (grep command and/or whole-file reads); its
  output is counted with the same tokenizer and savings are only credited when recall reaches
  `--min-recall`, then compared with the fixed multipliers in `src/mcp/savings.rs` (modes deviating
  more than 25% are flagged). `--baseline <report.json>` exits non-zero on a regression beyond
  `--tolerance` / `--cost-tolerance`.
- `benchmarks/eval/`: stdlib-only Python gold generators (`gen_symbols`, `gen_outline`,
  `gen_references`, `gen_dependents`, `gen_grep`, `gen_find`, `gen_git_search`, `gen_docs`), deterministic
  for a fixed HEAD sha and seed, with a workflow README.

### Changed

- **`[resources] onnx_provider` (new) defaults to `cpu`.** basemind previously left ONNX Runtime on
  its platform default, which on macOS is CoreML. CoreML compiles a new graph per input shape and
  measured 7.6 GB resident after embedding one 2 KB SVG, against 0.8 GB on the CPU provider (which
  is also ~3x faster end to end here); a scan of a repository with images and office documents grew
  to 33 GB. The setting covers the embedder, reranker, layout detection and NER. Set
  `onnx_provider = "auto"` to restore the old platform default, or `"coreml"` / `"cuda"` /
  `"tensorrt"` to pin one. Existing configs keep parsing; only the resource behaviour changes.
- The document tier now serialises over the memory ceiling. `FootprintGate::admit` (unchanged, still
  advisory for the drive governor and large-file parses) admitted every waiter after five seconds, so
  N workers over `max_footprint_mb` meant N simultaneous extractions. Documents use the new
  `admit_exclusive`: over the ceiling exactly one document runs at a time, and an otherwise idle
  process is admitted immediately instead of stalling every item for the full wait.
- `[documents] max_archive_bytes` (default 64 MiB) and `max_archive_entries` (default 1000) bound
  archive extraction when `extract_archives = true` (xberg's own defaults were 500 MB and 10,000).

### Fixed

- Stack-graph (L2) resolution no longer drives scan memory: each file is abandoned past 600,000
  steps, 50,000 partial paths or 3 s, and at most two builds run at once. A wall-clock budget alone let
  parallel workers stitching 100-200 KB files take a large-monorepo scan from ~2 GB to 9-13 GB.
- A scan could deadlock with `max_concurrent_documents` set: a scan worker holding a document slot
  waited on the embedding pool with `ThreadPool::install`, which keeps running queued jobs from the
  worker's own pool, so it started a second document that blocked on the slot the first still held
  (one scan thread and one slot hangs forever; more threads hang once every slot is held by a
  blocked frame). The embed hand-off now blocks on a channel and never runs foreign work.
- Documents whose own estimated working set exceeds `max_footprint_mb` are skipped (counted as too
  large, with a warning naming the estimate and the limit) rather than extracted: a 7 MB-of-XML
  `.docx` measured 12 GB with CoreML, and a 30,000 x 30,000 pixel image header is priced from its
  dimensions, not its byte size.
- **A large text document could stall a scan for tens of minutes.** xberg's markdown splitter (before 1.3.8) re-collects
  and re-sorts every remaining element for each chunk it emits, so chunking time grows with the square of
  the document (measured: 0.9 s at 256 KB, 3.3 s at 512 KB, 11.6 s at 1 MB, 40 s at 2 MB of log text, and 17 s at 256 KB of
  densely marked-up markdown, in a debug build). 0.29.0 routes csv, json, yaml, xml, txt and similar files to the document tier, so
  multi-megabyte data files reached it. Text-like documents over the new `[documents]
  markdown_chunk_max_bytes` (default 256 KiB) / `plain_text_chunk_max_bytes` (default 128 KiB) cutovers
  are now chunked by a linear fixed-size chunker; smaller documents chunk exactly as before. Degraded
  documents are logged and counted as `docs_degraded` in the scan summary.
- **`[documents] extraction_timeout_secs` is now enforced.** It was passed to xberg but could not
  interrupt CPU-bound work. Extraction now runs under a watchdog: an overrunning document is abandoned,
  logged, skipped, retried on the next scan, and counted as `doc_timeouts` (also in `failed`).

## [0.28.0] - 2026-10-04

> **Minor release — persisted schema bumped (`RELEASE_MINOR` 27 → 28).** The first `basemind scan`
> after upgrading wipes and rebuilds the on-disk cache (blobs, the Fjall index, vector rows) from
> source. No manual step is needed; a full re-scan is automatic.

### Added

- Project-level `.config/` convention (<https://github.com/pi0/config-dir>): configuration is now
  also discovered at `.config/basemind.toml` or `.config/basemind/config.toml` when no root
  `basemind.toml` (or legacy `.basemind/basemind.toml`) exists. The root file still wins when both
  are present. Any of these locations now counts as the durable "this is a basemind-managed root"
  marker for upward root discovery and the workspace-root guard, so a project that keeps its config
  under `.config/` no longer needs `--root`.
- `basemind init --config-dir`: scaffold the config under the `.config/` convention. Accepts
  `.config` (writes `.config/basemind.toml`) or `.config/basemind` (writes
  `.config/basemind/config.toml`); any other value is rejected, since there is no read-time
  config-path flag and a config written elsewhere would never be auto-discovered.
- `[languages.<grammar>]` per-grammar overrides keyed by tree-sitter-language-pack grammar name:
  `enabled = false` stops parsing that grammar's files as code (they fall through to the document
  tier; fixes `.txt` detected as vimdoc and `.conf` as nginx), `extensions` / `filenames` map extra
  suffixes and exact file names onto a grammar ahead of built-in detection, and `preload` makes
  `basemind lang install` fetch it. An unknown grammar name is a config error with near-match
  suggestions.
- `[scan] floor_allow`: remove entries from the always-on exclude floor by directory, file name,
  glob or floor pattern. `.git` and `.basemind` can never be allowed; an entry naming nothing in the
  floor is ignored with a warning.
- `[documents] include` / `exclude` (which non-code files become documents), `max_file_bytes`
  (per-document cap, default 50 MiB, independent of `[scan] max_file_bytes`), and `embed_include`
  alongside the existing `embed_exclude`; `[code_search] embed_include` with the same semantics
  (allow-list, `embed_exclude` wins, left-out files stay keyword-searchable).
- Cached chunks and documents are fingerprinted against the settings that shape them, so changing a
  chunk-size, page-cap, keyword, NER, summarization or `[llm] model` setting re-processes only the
  affected files. The first scan after an embedding-scope change (`embed`, `embed_include`,
  `embed_exclude`, the `enabled` switches) deletes the vector rows of files that are no longer
  eligible and rebuilds the rows of newly eligible ones from cached blobs without re-embedding.
- The shared daemon loads each workspace's config once, applies operator-raisable resource ceilings
  (`BASEMIND_DAEMON_MAX_SCAN_THREADS`, `_EMBED_THREADS`, `_EMBED_BATCH`, `_CONCURRENT_DOCUMENTS`,
  `_FOOTPRINT_MB`, `_MAP_CACHE_MB`, `_CANDIDATES`, `_FILE_BYTES`, `_DOCUMENT_BYTES`,
  `_DOCUMENT_PAGES`, `_EXTRACTION_SECS`, `_CHUNKS_PER_DOCUMENT`, `_CRAWL_PAGES`, `_CRAWL_DEPTH`,
  `_CRAWL_BODY_BYTES`, and the `BASEMIND_DAEMON_MIN_DEBOUNCE_MS` floor), and reloads a workspace's
  `basemind.toml` on the next request after it changes. A file that no longer parses keeps the last
  good config and logs a warning. `admin status` reports `config_stamp` to spot an edit.
- Operator environment grants for settings a repository's own `basemind.toml` cannot set itself:
  `BASEMIND_ALLOW_REPO_LLM`, `BASEMIND_ALLOW_FOLLOW_SYMLINKS`, `BASEMIND_ALLOW_EXTRA_ROOTS` and, in
  the daemon, `BASEMIND_DAEMON_ALLOW_EXTRACT_ARCHIVES`. `BASEMIND_ALLOW_EXTRA_ROOTS` takes `1` /
  `true` / `yes` for every workspace, or a `:`-separated list of absolute workspace roots that
  grants only those (`;` on Windows); relative entries are ignored with a warning.
- A new linked worktree's `working` view is seeded from a sibling checkout with a copy-on-write
  clone, so its first scan only touches files that differ. A sibling whose workspace lock is held, or
  one too large to copy without reflinks, is skipped; a crashed clone's `.seed-tmp` debris is swept
  on open. `BASEMIND_NO_SEED=1` opts out.
- A linked worktree whose HEAD descends from the indexed git-history head now appends to the shared
  index instead of skipping it.
- Reserved config keys that nothing reads log a warning when set to a non-default value.

### Changed

- Bare names in every glob list (`[scan]`, `[documents]`, `[code_search]` `include` / `exclude` /
  `embed_*`) are gitignore-like: `generated` matches that path segment at any depth and everything
  beneath it, and `docs/api` is anchored at the root. A leading `./` or `/` and a trailing `/` are
  ignored. A negated (`!`) pattern, an invalid glob, or an empty `[scan] include` is a config error.
  For `extra_roots` files, globs match the path relative to the extra root.
- Repo-supplied `[llm] base_url`, foreign `api_key` env references, `[crawl] allow_private_network`,
  `[scan] follow_symlinks` and `[scan] extra_roots` now need an operator grant in the environment
  (`BASEMIND_ALLOW_REPO_LLM`, `BASEMIND_ALLOW_PRIVATE_HOSTS`, `BASEMIND_ALLOW_FOLLOW_SYMLINKS`,
  `BASEMIND_ALLOW_EXTRA_ROOTS`). An `api_key` env reference is still honoured for the chosen
  provider's standard variable and `BASEMIND_LLM_API_KEY`.
- `BASEMIND_ALLOW_PRIVATE_HOSTS` accepts `1`, `true` or `yes` in both the config gate and the URL
  guard (the URL guard previously accepted only `1`).
- The daemon treats `[resources]`, `[scan] max_candidates` / `max_file_bytes`, `[documents]` limits,
  `[crawl]` limits and `[watch] debounce_ms` as ceilings or floors, and clamps the per-call `web`
  `crawl` `max_pages` / `max_depth` overrides to the same caps.
- mimalloc is the global allocator. ONNX Runtime memory is bounded in every process (no
  memory-pattern planning, no retained CPU arena; intra-op threads capped), and the daemon releases
  resident embedding engines once the last concurrent embedding pass ends.
- The daemon caps its runtime and pool threads, serializes read-stack builds, defers the startup GC,
  bounds fjall worker threads and journal size (flushing an oversized journal on open), and runs
  git-history index sync on a dedicated two-thread pool.
- Blob chunk state is read from the envelope header alone instead of decoding the blob.
- Changing a reserved key (`[documents.language] preferred_languages`, `[documents.ocr]`) no longer
  re-extracts every document.
- `basemind.toml` scaffold comments written by `basemind init` describe the new keys, the credential
  floor and the operator grants.

### Fixed

- Linked worktrees no longer wipe and rebuild the shared git-history index when they switch
  branches.
- The file watcher rebuilds its include/exclude filter when `basemind.toml` changes, and detects an
  equal-length rewrite of the config inside one timestamp tick.
- Incremental scans (watcher, `rescan paths`) evict files that became ineligible, skip symlinks, and
  cap the bytes read.
- The embed policy is recorded only after a complete purge, and keyword postings are rebuilt when
  needed; empty-scope lance deletes are skipped and large ones are batched at 2000.
- Extraction returns an error instead of panicking when its runtime cannot be built, and the
  git-history pool falls back to the caller's thread instead of panicking under thread exhaustion.
- Re-flushing an external-root (`extra_roots`) document replaces its existing vector rows instead of
  leaving stale duplicates: the delete now uses the document's own per-root scope, matching the rows
  it inserts.
- The last concurrent embedding pass releases resident engines under a lock, so a pass that starts
  concurrently cannot have the engines it is about to use freed underneath it.

### Security

- Credential and key material is excluded from the index by default, so no secret becomes
  searchable by every agent that can query it: `.env` and `.env.*` (including `.env.example`),
  `.aws/`, `.ssh/`, `.gnupg/`, `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, `id_rsa` /
  `id_dsa` / `id_ecdsa` / `id_ed25519`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`.
  `[scan] floor_allow` opts a class back in, but a credential entry needs the operator's
  `BASEMIND_ALLOW_REPO_CREDENTIALS` grant — a cloned repository's own `basemind.toml` cannot
  un-exclude the operator's untracked secrets (build-artifact entries still need no grant).
- `follow_symlinks` is an operator decision: the repository's own `basemind.toml` cannot turn it on.
  Working-tree reads, including the watcher and `rescan paths`, refuse symlinked files and paths that
  resolve outside the workspace, and a `basemind.toml` that is a symlink leaving the workspace is not
  followed.
- Repository-authored `basemind.toml` can no longer redirect an LLM endpoint or read an arbitrary
  environment variable as an API key, reach private hosts, follow symlinks, index directories outside
  the repository, or expand archives in the daemon, without an operator grant (see Changed).
  `extra_roots` additionally refuses credential directories (`.ssh`, `.aws`, `.gnupg`, `/etc`),
  filesystem roots and roots inside the repository, grant or not.
- Daemon resource ceilings bound what a hostile repository config can cost: scan and embed threads,
  batch size, footprint, candidates, file and document sizes, pages, extraction time, chunks and
  crawl pages, depth and body size.

### Removed

- The TUI and agent stack was removed upstream and is not part of this release: the
  `basemind-tui`, `basemind-agent`, `basemind-agent-ipc` and `basemind-ui` crates, the
  `basemind ui` and `basemind agent` subcommands, the `agent-tui` / `desktop-ui` features, and the
  `[agent]` config table (now rejected as an unknown key; delete it from `basemind.toml`). None of it
  was ever part of a release archive: the `agent-tui` / `desktop-ui` features were omitted from
  `full` and the release workflow built only the `basemind` binary. `basemind agents` (multi-agent
  comms) is unrelated and unchanged.
  ADR-0006 is marked withdrawn.

## [0.27.4] - 2026-09-28

> **Patch release — `init` defaults to local/gitignored config, a benchmark harness, and a
> read-only-store flush fix.** No schema or MCP-surface change: `RELEASE_MINOR` stays 27, so
> the on-disk blob and index formats are untouched.

### Added

- `basemind init` now defaults every writable target to its `.local`/gitignored form: an
  ai-rulez config routes to `.ai-rulez/local/rules/basemind-usage.md` instead of the
  committed rules tree, and a new `.gitignore`-coverage check prompts (or, with `--yes`,
  auto-appends) a pattern for any `.local` target `.gitignore` doesn't already cover.
- A new `.claude/settings.local.json` step in `init`, adding the basemind MCP permission
  entries via a surgical JSON merge (`serde_json`'s `preserve_order` feature) that only
  touches the keys basemind owns — an existing `skillOverrides` or other hand-ordered
  content in that file is left untouched.
- `basemind admin tokens --stdin`: counts stdin with the real o200k tokenizer
  (`src/mcp/tokens.rs`) and prints just the integer. Gated behind the `tokenizer` feature
  (already implied by `documents`), not the heavier `documents`/`code-search` builds.
- `benchmarks/`: a harness (`run.sh` + `tasks.example.yaml`) that runs a fixed task list
  twice — once via a `basemind` command, once via the equivalent `grep`/`cat`/`find`
  baseline — and reports real token counts (via `admin tokens --stdin`) and wall-clock time
  for each side, plus totals. See `benchmarks/README.md`.

### Fixed

- **`admin rescan` / `scan_and_refresh` could fail with ENOENT on
  `views/working/index.msgpack.tmp`** on a workspace's first scan, whenever the `Store` was
  opened via `open_read_only` (which — unlike `open_with_holder` — never creates `view_dir`
  on disk). The MCP `admin rescan` tool and its CLI shim both open their `Store` this way, so
  this reproduced on any workspace's first-ever scan through either surface. Fixed with a new
  `Store::ensure_dirs()`, called inside `scan_and_refresh` right after acquiring the write
  lock, so a store opened read-only still gets its rebuildable directories materialized
  before the first `flush()`.
- `Store::open_with_holder` now acquires its lock before creating rebuildable cache
  subdirectories, closing a lock-ordering gap that could race a concurrent opener.

## [0.27.3] - 2026-09-27

> **Patch release — the first publish that completes on every platform.** Supersedes the
> unpublished v0.27.1 and v0.27.2 drafts: no crates.io, npm or PyPI version was cut from either.

### Fixed

- The release helpers no longer pass `gh api` an endpoint with a leading slash. Git Bash on the
  Windows runner rewrites an argument beginning with `/` into a filesystem path, so
  `gh api "/repos/…"` became `C:/Program Files/Git/repos/…` and the call failed with
  *invalid API endpoint*. The Windows build job hit this on its pre-upload resolve and skipped the
  upload, and `finalize_release` runs on every leg via `always()`. The endpoints now omit the
  leading slash, as the CLI's own error recommends; the absolute `uploads.github.com` URL is
  unaffected.

## [0.27.2] - 2026-09-27

> **Never published as a GitHub release.** The Linux builds were fixed below, but the Windows
> upload and the promotion still failed, so no registry version was cut. Superseded by
> [0.27.3].

### Fixed

- The `shells` feature no longer fails to compile on Linux. `rustix` was refreshed to 1.1.5, which
  renamed its experimental `runtime` module to a rotating mangled name (`runtime_448b8ad740e2a26f`)
  and reduced the old `runtime` path to a private alias. `rmux-client` 0.10.0 imports
  `rustix::runtime::{…}` from its Linux attach-resize path, so only the Linux builds broke
  (E0603); macOS and Windows were unaffected. `rustix` is now held at 1.1.4, the last release whose
  `runtime` module is public and the version `rmux-client` itself declares.

## [0.27.1] - 2026-09-27

> **Never published as a GitHub release.** The source below was tagged and pushed, but the Linux
> archives failed to build and the release record was never promoted. It is superseded by
> [0.27.3]; no registry version was cut from it.
>
> **Patch release — memory-safety follow-ups to [#62], a stricter plugin launcher, and a
> release-workflow fix.** No schema or MCP-surface change: `RELEASE_MINOR` stays 27, so the
> on-disk blob and index formats are untouched.

### Changed

- Dependency refresh: `crawlberg` 1.7.1 → 1.8.0, `gix` 0.87.1 → 0.88.0, `jsonschema` 0.56 → 0.58,
  and `lru` 0.18.4 → 0.18.5. `lancedb` stays held at 0.37: 0.38 and 0.39 still gate the
  `Error::Http` variant behind their `remote` feature while `job.rs` references it unconditionally,
  and enabling `remote` would pull an HTTP client stack in for no benefit here.

### Fixed

- **Comms daemon memory growth ([#62])**, continued:
  - The per-connection comms client map is now bounded with an LRU. It is keyed by the
    caller-supplied `as_agent`, so it previously grew one broker socket and one notification queue
    for every identity a session ever named.
  - The pending-notification queue is now capped, dropping the oldest entries. Clients that only
    issue ordinary requests never drain it, so it grew for the life of the link.
- The git-cache RAM categories are now charged by bytes as well as entry count, each held to
  32 MiB. A single `CommitInfo` or `BlameResult` has no natural size, so 1024 entries could reach
  gigabytes.
- The file watcher's debounce queues are bounded, and overflow coalesces into a path set instead of
  queueing without limit. A rescan slower than the debounce interval could otherwise let batches
  accumulate unbounded.
- `gix` 0.88 no longer accepts the `Infallible` turbofish, which `src/git/commit.rs` still passed.
- The plugin launcher never falls back below 0.26.0, verifies each candidate is installable, and
  honours `BASEMIND_BIN`. With v0.27.0 incomplete, the old fallback exec'd 0.25.2, which predates
  the [#62] memory bounds and OOM-killed the machine.
- The release workflow now addresses assets by release id rather than by tag. A hand-promoted
  v0.27.0 draft left two records on one tag and split the archives across them; uploads replace in
  place, and every stage refuses an ambiguous tag.

## [0.27.0] - 2026-09-18

> **Minor release — a toolchain dependency refresh and a lint-driven hardening pass.** The agent
> tool surface and the config schema are unchanged, but `RELEASE_MINOR` advances 26 → 27, so the
> first scan after upgrading rebuilds the content-addressed blob cache automatically.

### Changed

- **tree-sitter 0.27** (was 0.26), with tree-sitter-language-pack 1.20.0 to match. Two breaking API
  moves were adapted:
  - `QueryMatch.captures` became the method `QueryMatch::captures()`. The L1/L2 extractors
    (`src/extract/l1.rs`, `src/extract/l2.rs`, `src/extract/locals.rs`) and the intel resolver
    (`src/intel/stackgraph.rs`) were migrated.
  - `Node::kind()` now returns a tree-borrowing `&str`. The vendored tree-sitter-graph keeps
    `SyntaxNodeRef`'s kind `'static` by interning node-kind names into a small global table
    (`crates/tree-sitter-graph/src/graph.rs`).
- **rmcp 3.4** (was 3.2). The handshake types that collided with the wire protocol's
  `serverInfo`/`clientInfo` fields were renamed: `ServerInfo` → `ServerConfig`
  (`src/mcp/server_handler.rs`) and `ClientInfo` → `ClientConfig` (`tests/tasks_smoke.rs`). The
  server still advertises MCP logging; that capability line now carries `#[allow(deprecated)]` until
  the SEP-2577 deprecation removes logging in a future rmcp.
- **jsonschema 0.56** (was 0.52), **crawlberg 1.7.1** (was 1.5.0), **lru 0.18.4** (was 0.18.3),
  **zstd 0.14.0** (was 0.13.3), and **liter-llm 2.0.2** in the agent crate (was 1.19.1). `lancedb`
  stays pinned at 0.37: 0.38 and 0.39 still fail to compile without an HTTP-client stack basemind
  has no use for.
- The provenance ledger in the layered config merge (`src/config/layered.rs`) is no longer inlined
  per field. The merge now records a field's `ConfigSource` through one `record_provenance`
  helper — which also skips the ledger entirely on the MCP override path that throws it away — and
  the merge's cyclomatic complexity drops from 43 to under the 40 ceiling. The two override tests
  pin the behaviour.

### Fixed

- The npm and pip installers' network paths were lint-harded: `npm-package/install.js` no longer
  returns from a Promise executor or reads the deprecated `RegExp` last-match globals
  (`parseInt` → `Number`), and its thrown errors carry `{ cause }`; `pip-package/basemind/
  downloader.py` moves the type-only import under `TYPE_CHECKING`.
- Typos in tests (a misspelled `repositories` in a test name and a stray fixture path) and a
  dangling `return;` in `scripts/codex-mcp-launch.mjs`. The opencode plugin's console fallback
  explains why it exists instead of carrying a bare suppression directive (`no-console` was already
  ignored for JS in `poly.toml`, so the rule it guarded never fires).
- `poly.toml` gained per-file ignores for the vendored fork crates and for `CHANGELOG.md` (length is
  intentional), and the shipped shell scripts were re-rendered by the shell formatter —
  whitespace-only, no behaviour change.

## [0.26.0] - 2026-09-01

> **Minor release — cache rebuild, breaking changes, and a security fix.** Read *Breaking changes*
> and *Security* before upgrading. Almost all of this release is issue [#62]: a comms daemon reached
> 43.8 GiB RSS in about two minutes on an initial scan of a large monorepo and was OOM-killed,
> taking a VS Code renderer with it; under a 2 GiB `MemoryMax` the same scan died in about 50
> seconds.

### Breaking changes

1. **A workspace root must be a project.** It has to be a git repository, or a directory containing
   `basemind.toml`. Anything else is refused, and a filesystem or volume root is refused with no
   override at all. *If you hit this:* run `basemind init` in the directory (writes `basemind.toml`,
   the durable marker), or set `BASEMIND_ALLOW_ANY_ROOT=1` for that invocation. A root that already
   had a working-view index before the upgrade keeps working untouched.
2. **`[scan] extra_roots` is ignored unless `BASEMIND_ALLOW_EXTRA_ROOTS=1` is set** in the
   environment that launches basemind. *If you hit this:* export the variable where your MCP host or
   shell starts basemind — it deliberately cannot be granted from the scanned repository's own
   config.
3. **`[resources] max_footprint_mb = 0` now means *auto*, not *disabled*.** Every config generated
   before this release carries that value, so upgrading gains a memory ceiling rather than keeping
   none. *If you want the old behaviour:* write `max_footprint_mb = "off"`.
4. **Caches are wiped and rebuilt on the next scan.** `INDEX_PARTITION_REVISION` advances 4 → 5, so
   the Fjall secondary index is recreated on next open — fjall persists a keyspace's
   `max_memtable_size` at *create* time and ignores the create options for a keyspace that already
   exists, so the new per-keyspace sizing only takes effect on a recreated index. `RELEASE_MINOR`
   advances 25 → 26 with the release cut, which also invalidates the content-addressed blob cache
   and the git cache. *No action required:* rebuild is automatic, but the first scan after upgrading
   is a full one.
5. **Git history is no longer indexed for a workspace root that is merely a subdirectory of an
   enclosing repository.** *If you relied on it:* set `BASEMIND_GIT_DISCOVER_PARENTS=1`.

### Security

- **The daemon's streamable-HTTP MCP endpoint is no longer unauthenticated, and no longer starts by
  default.** Versions 0.23.0 through 0.25.2 bound `127.0.0.1:51786` unconditionally and gated
  requests only with a `Host` / `:authority` loopback check. That check defeats DNS rebinding and
  nothing else: any local process could open a TCP connection, send `Host: 127.0.0.1`, and reach the
  full shared tool router — which in the release build (`--features full`) includes the `shell`
  tool, enabled by default. Every process on the machine that could reach the port could therefore
  run commands as the daemon's user. Two changes close it:
  - The listener is opt-in. It binds only when `BASEMIND_ALLOW_HTTP` is truthy in the *daemon's*
    environment; unset, no socket exists. An environment variable rather than a config key because
    the daemon's environment belongs to whoever started it, whereas `basemind.toml` is authored by
    whoever wrote the repository.
  - When it does bind, every request on both routes (`/mcp` and `/ui`) must present a bearer token —
    32 bytes from the OS CSPRNG, drawn per daemon process, checked before routing and before any
    workspace is named or opened, compared without an early exit. The token is published as the
    second line of `<comms_dir>/http.addr`, created mode `0600` at `open` time so it is never
    briefly world-readable. `Authorization: Bearer <token>` and `?token=<token>` are both accepted
    (a browser navigating to `/ui` cannot set a header). `basemind daemon ensure` reports the token: on
    stderr for the human form, so the URL stays alone on stdout and `URL=$(basemind daemon ensure)`
    keeps working, and in the object for `--json`.
- **The Claude and Cursor plugin manifests no longer point at that port.** They now launch
  `scripts/mcp-launch.sh serve --root <project dir>` over stdio. A hard-coded loopback port is not
  addressable to a *process*: whichever process bound it first is what the client talked to, so a
  local squatter could silently intercept every tool call, and no credential on the real daemon can
  fix a client that never reaches it. The stdio relay is path-addressed and `0600`. The root is now
  passed explicitly instead of being inherited from the host's working directory — a failed
  expansion becomes a loud refusal from the root guard, where a wrong cwd was a silent scan of the
  wrong tree. `tests/plugin_manifest_smoke.rs` pins both properties, including that no manifest
  hard-codes `51786` anywhere.

### Added

- `[scan] max_candidates` (default `500_000`, `0` disables) aborts a scan from inside the walk once
  more than that many candidate files have been kept — before extraction, before any index write,
  and before the candidate list itself can grow to hundreds of megabytes. The error names the
  heaviest contributing directories by their first two path segments, so a vendored or generated
  tree that slipped past `.gitignore` is a one-line fix rather than an out-of-memory kill. ([#62])
- `BASEMIND_ALLOW_ANY_ROOT=1` accepts any directory as a workspace root, for the cases the new
  allow-list refuses. It does not override the refusal of a filesystem or volume root, and it
  applies only to the invocation that sets it: the index a hatched scan produces does not make the
  root permanently acceptable, so unsetting the variable refuses it again. ([#62])
- `[scan] max_candidates` also bounds the walk itself, not only what it keeps: a walk that visits a
  generous multiple of that many filesystem entries while accepting almost none of them now aborts
  with its own error, because a root broad enough to enumerate tens of millions of entries burns
  exactly the time and syscalls the ceiling exists to prevent. ([#62])
- basemind now reads the memory limit it is running under, not just its own usage: cgroup v2
  (`memory.max`, taking the minimum over the whole hierarchy, because systemd puts `MemoryMax` on
  a slice above the scope a process actually lives in), cgroup v1 (with its no-limit sentinel
  recognised as *no limit* rather than as an 8-exabyte ceiling), `/proc/meminfo` on a Linux box
  with no memory cgroup, `hw.memsize` on macOS, and `GlobalMemoryStatusEx` on Windows. Usage on
  Linux is a working set — reclaimable page cache is subtracted, as kubelet does — so having read
  a large repository no longer reads as being near the limit. A cgroup that sets no limit — the
  default for a plain `docker run` — falls back to total RAM rather than reporting no ceiling, and
  a cgroup limit larger than the machine is not treated as the binding constraint. Issue #62 was
  reported from a process running under a 2 GiB `MemoryMax` that had no way to know the limit
  existed. ([#62])
- `[resources] max_footprint_mb` accepts `"off"` and `"auto"` alongside an integer. ([#62])
- `[resources] max_map_cache_mb` (default `256`, `0` = unbounded) bounds the MCP read stack's
  decoded-outline cache, per workspace. Charged in **bytes**, not entries: a decoded outline spans
  three orders of magnitude (a ~1.4 KB median against a measured 697 KB maximum), so an entry cap
  would bound the count and not the footprint. A miss costs one blob read and never changes an
  answer. The code-graph memo is charged at half the same budget, and a read-only session's
  projected call / implementation indexes are charged against it too. ([#62])
- `[code_search] max_chunks_per_file` (default `2000`) caps how many chunks one source file may
  contribute to the search index, mirroring the document tier's `max_chunks_per_document`. A
  generated parser table or a checked-in bundle otherwise fans one file out into a chunk count
  bounded only by `[scan] max_file_bytes`, and every chunk costs a BM25 posting entry carrying its
  full term list, a LanceDB row, and — with `embed` on — an ONNX embed. An over-cap file is still
  chunked and cached (the `.chunk.msgpack` sidecar is written, so outline and grep are unaffected);
  what it loses is the per-chunk fan-out, with a warning naming the file. ([#62])
- `BASEMIND_NO_AUTOSPAWN=1` makes basemind connect to a daemon that is already running but never
  start one, so an operator can guarantee the daemon exists only inside a resource-controlled unit.
  A working systemd user unit — with install steps, the `environment.d` wiring, and the commands
  that verify the ceiling is real rather than merely configured — ships at
  `docs/systemd/basemind-comms.service`. Two deliberate exceptions: `basemind comms start` ignores
  the variable (an operator who typed a start command has stated the intent it withholds from
  implicit callers), and an already-live daemon is still used — connect-only means *do not start
  one*, not *do not use one*. ([#62])
- A scan leaves post-mortem evidence a `SIGKILL` cannot erase.
  `<workspace_cache_dir>/scan-inflight.json` is written before the pass starts and removed on
  `Drop`, so a surviving record proves the process
  never ran its destructors — the signature of an OOM kill, which is exactly the case the existing
  `last-fatal.json` structurally cannot report, because the code that writes it never runs. The
  record carries the pid, the build, the phase, the candidate count, and the root (a root of `/` is
  itself the finding). `basemind comms doctor` lists every such record whose pid is dead —
  filesystem-only, no RPC, so it stays usable on a wedged machine — and `--clear-fatal` now
  acknowledges stale scan records alongside the fatal-store record. A periodic `rss_mb` /
  `ceiling_mb` log line makes the growth curve reconstructable after the fact. ([#62])
- `BASEMIND_GIT_DELTA_CACHE_BYTES` overrides the per-`Repository` delta-base (pack) cache bound;
  `0` disables the cache. gix's default when `core.deltaBaseCacheLimit` is unset is 96 MiB **per
  thread-local `Repository`**, i.e. per rayon worker during a git-history build — roughly 3 GiB of
  decoded delta bases on a 32-core host that nothing in basemind accounted for. basemind now sets
  the key to 8 MiB, as a CLI-precedence override so a repository-local `core.deltaBaseCacheLimit`
  cannot raise the bound we set for our own protection. Undersizing it costs re-decoding, never a
  wrong answer. ([#62])
- `BASEMIND_ALLOW_HTTP` gates the daemon's streamable-HTTP MCP front-end — see *Security*. ([#62])

### Changed

- **Breaking:** a workspace root must now be a git repository or contain `basemind.toml`. basemind
  opens a root read-write and indexes everything beneath it, and root discovery falls back to the
  starting directory, so an MCP host launched at `/` could previously hand the daemon the whole
  filesystem — which is how a scan reached 43.8 GiB and was OOM-killed. `basemind init` and
  `BASEMIND_ALLOW_ANY_ROOT` are the escape hatches, and a root that already had an index on disk
  before the upgrade is grandfathered so upgrades do not break a working setup. The guard resolves
  the root — collapsing `..`, following symlinks — before deciding, and hands the resolved path to
  whatever opens the store, so a path such as `/..` can no longer be checked as a subdirectory and
  opened as the filesystem root. It now also covers `basemind admin rescan` (and the `admin` MCP
  tool), which walks the whole working tree in-process, and the daemon's git-history sync. There are
  three refusals, and the error text names every way out of the one you hit: *not a project* (a
  plain directory — overridable), *filesystem root* (`/`, `C:\`, a UNC share root — never
  overridable, by any marker, hatch or pre-existing index), and *unresolvable* (relative, missing or
  unreadable, so the guard cannot decide about it at all). It is a misconfiguration guard, not an
  authorization boundary: it does not defend against a caller who controls the process's arguments
  or environment. ([#62])

- The grandfather clause reads an explicit record rather than inferring consent from a side effect.
  The guard writes `root-admission.json` into the root's workspace cache dir, stamped
  `pre_existing_index` (a standing admission that survives with no env var — the upgrade path) or
  `env_hatch` (never a standing admission). Without it, the mere existence of a working-view index
  was proof of admission, which made a single `BASEMIND_ALLOW_ANY_ROOT=1 basemind scan` a
  *permanent* whitelist for that root: the scan the hatch authorized left behind the very file the
  next hatch-free run would accept as evidence. The first verdict is the standing one and is never
  overwritten, and the write is best-effort — a failed write only degrades the guard to its
  index-means-admitted behaviour and never fails a scan. ([#62])

- The HTTP transport's `?root=` now runs the same root discovery the CLI does, so naming a
  subdirectory of a repository attaches to that repository instead of being refused — the two
  front-ends previously disagreed about what a root is. ([#62])

- Excluded directories are now pruned during the walk instead of being filtered per file after the
  walker has already descended into them, so a non-gitignored `node_modules` is no longer traversed
  and stat'd in full. The indexed set is unchanged. This also fixes the Linux watcher, which
  previously registered inotify watches inside trees it was excluding. ([#62])

- **Breaking:** `[scan] extra_roots` now requires the operator to set `BASEMIND_ALLOW_EXTRA_ROOTS=1`
  in the environment. That key names directories *outside* the repository but is read from the
  scanned repository's own `basemind.toml`, so cloning and indexing a repo was enough to make
  basemind walk whatever it named — `~/.ssh`, `~/.aws` — and surface the contents through the
  `code grep` / `search` tools. The process environment is the one input a cloned repository cannot
  write, so it is what the grant hangs on. Extra roots also now count toward `[scan] max_candidates`
  (they were appended after the ceiling was evaluated), are refused when they resolve to a
  filesystem or volume root, follow symlinks only when `[scan] follow_symlinks` is on (it was
  hard-coded on), and stop when a scan is cancelled. ([#62])

- The scanner bounds what it stages into the index by **bytes** rather than by file count. BM25
  keyword postings — one entry per (term, chunk), staged on essentially every scan — previously
  counted toward no limit at all and could not force a commit; they now participate in the byte
  budget.

  This is the single change that most explains issue #62's number. The old bound counted *files*,
  and postings were staged without touching that counter, so the one structure that grows with the
  product of files and distinct terms was the one structure nothing bounded. The other fixes in this
  release matter, but they are smaller than their prominence suggests: reverting the chunked drive
  loop to its previous whole-corpus form, measured at both arms on a 4 000-file corpus, moves peak
  RSS by ~38 MiB — about 10 KiB per file — because a file's postings are already cleared inside the
  worker before its result is returned. Worth having at monorepo scale, but not the 43.8 GiB.
  Directory pruning is a walk-cost win rather than a memory one. Stated explicitly so nobody
  re-derives the priorities from scratch next time. ([#62])

- **Breaking:** `[resources] max_footprint_mb = 0` now means **auto**, not disabled. An existing
  config that carries the old default — as every config generated before this release does —
  therefore gains a memory ceiling on upgrade rather than keeping none. Auto budgets 75% of an
  enforced cgroup limit or 50% of total machine RAM, whichever the platform reports, floored at
  512 MiB. Write `max_footprint_mb = "off"` to restore the previous behaviour, or a positive
  integer to state a ceiling explicitly. The gate remains best-effort — it parks workers and then
  admits anyway after five seconds rather than failing a scan — so the change shapes peak memory
  and cannot make a scan that used to succeed start failing. A long-lived daemon shipping with its
  only memory bound disabled by default is what made 43.8 GiB reachable. ([#62])

- The footprint gate works on Linux and Windows, not only macOS. Its sampler was
  `sysres::phys_footprint`, a Darwin-only metric returning `None` everywhere else, so on the
  machine that filed #62 the ceiling could not have throttled anything even if it had been
  configured. ([#62])

- The MCP read stack no longer keeps every file's decoded outline resident for the process lifetime.
  The old `MapCache` was the only `O(total symbols)` structure with permanent lifetime — plus a
  second full copy of every import, plus a deep clone of the whole map on every watcher batch, times
  up to 16 hot workspaces in one daemon. It splits into a permanent `O(files)` symbol-free index
  view and a byte-charged read-through LRU governed by `[resources] max_map_cache_mb`, keyed by
  content hash so one cache is safely shared across snapshots and identical files share an entry.
  Building the view now decodes zero outline blobs — it projects the index and probes blob existence
  in parallel, one stat per file instead of a full read and decode. A read-only session whose
  projections are truncated by the budget reports it through the existing lifecycle notice with
  `retry: false`, because rerunning cannot widen a budget. ([#62])

- Per-keyspace Fjall memtable sizing replaces the 64 MiB default on all 17 keyspaces: 16 MiB for the
  BM25 posting list (the densest writer by far), 8 MiB for the secondary indexes the scanner
  rewrites once per file, less for the keyspaces a scan barely touches — all within fjall's
  recommended 8–64 MiB band. This is what `INDEX_PARTITION_REVISION` 4 → 5 exists to force: fjall
  persists `max_memtable_size` at keyspace *create* time and ignores the create options on reopen,
  so an index built by an earlier build would have kept the 64 MiB default forever. ([#62])

- The staged-byte counter now models fjall's real per-entry cost (`64 + heap(key) + heap(value)`,
  where a payload of 20 bytes or fewer inlines and anything larger carries an 8-byte refcount
  header). Charging `key.len() + value.len()` undercounted what a `WriteBatch` actually holds by
  2–3×, so both the per-worker batch budget and the process-wide staging ceiling meant something
  other than what they said. It remains a deliberate lower bound: allocator size-class rounding and
  `Vec` growth slack are not modelled. ([#62])

- The resolve pass no longer materialises the whole corpus. It previously built every file's
  resolved-reference edge set at once — one edge per resolved identifier use — held them through
  fact staging, then transformed rather than dropped them. The per-file edge set now cannot escape
  the file that produced it (there is no field to retain it in, so keeping the corpus set is a
  compile error), and the serial index writers gained the same byte bounds the parallel workers
  already had, closing the last staging path with no byte bound at all. ([#62])

- `basemind scan` / `rescan` stream each file's line as the scan absorbs it instead of accumulating
  one result per file for the whole repository just to print it afterwards. `ScanReport` now carries
  the summary statistics only; a caller that wants the old whole-corpus vector asks for it
  explicitly. ([#62])

- **Breaking:** git history is no longer indexed for a workspace root that is a subdirectory of the
  repository git discovery lands on. Those commits are addressed by paths that do not exist under
  the root, and walking them costs the whole ancestor object database — a 4.4k-file subdirectory of
  a monorepo was driving gix over 275 MiB of packs and 101 655 objects. Discovery itself is
  unchanged (ascent is what lets `basemind scan src/` lift to the repository root); only the
  history integration is gated. `git init` in the directory, or
  `BASEMIND_GIT_DISCOVER_PARENTS=1`, restores the old behaviour. ([#62])

- The Claude and Cursor plugin manifests launch `scripts/mcp-launch.sh serve --root <project dir>`
  over stdio instead of dialling `http://127.0.0.1:51786/mcp?root=…` — see *Security*. ([#62])

- Dependency refresh: `xberg` repinned to the current 1.1.0 revision (internal extraction fixes and
  its own dependency updates), `crawlberg` 1.4.2 → 1.5.0, `rmcp` 3.1 → 3.2,
  `tree-sitter-language-pack` 1.15.12 → 1.16.1, `liter-llm` 1.18.4 → 1.19.1, `smallvec` 1.15 → 1.16,
  `getrandom` 0.3 → 0.4. Two incompatible upgrades were deliberately not taken: `tree-sitter` stays
  at 0.26 because `tree-sitter-language-pack` 1.16.1 still requires `^0.26` and `tree-sitter`
  declares `links = "tree-sitter"`, so the graph admits exactly one version; and `lancedb` stays at
  0.37 because 0.38.0 does not build without its `remote` feature — it gates the `Error::Http`
  variant behind that feature while referencing it unconditionally from `job.rs`, and enabling
  `remote` would pull an HTTP client stack in for no benefit here.

### Fixed

- A scan of a repository holding *exactly* `[scan] max_candidates` candidate files could abort
  spuriously: the ceiling was checked against every walk entry, so once the candidate list reached
  the cap the next directory node or non-indexed file tripped it. Whether it fired depended on
  readdir order. ([#62])
- The "largest contributors" list in a `max_candidates` abort routinely named the wrong directory.
  The tally stopped at the breach and `ignore::Walk` is depth-first in readdir order, so a repo
  with `aaa/` (30 files) and `zzz/` (5000) was told to exclude `aaa`. The walk now surveys a
  bounded number of further entries before erroring, buckets by the first *two* path segments so
  monorepos get an actionable name, and groups root-level files instead of listing them one by
  one. ([#62])
- A draining daemon can now interrupt a running file walk. Previously cancellation was only checked
  once the walk had finished, so a runaway scan could not be stopped. ([#62])
- `[resources] max_concurrent_documents` was parsed and never consumed — the setting did nothing. A
  counting semaphore now enforces it. It is distinct from the footprint gate, which reacts to memory
  *already* allocated: a document extraction's spike (decoded page buffers, OCR bitmaps, an
  embedding batch) lands faster than the sampler can see it, so this bounds how many spikes may
  overlap before the first one starts. ([#62])
- Staging an entry that failed returned early without running the commit check, so bytes the failed
  upsert had already staged drifted out of the ledger permanently. ([#62])
- Commit cadence no longer collapses at high core counts. The process-wide staging ceiling had no
  hysteresis, so cadence scaled as `ceiling / workers` — a commit per file on a many-core host. A
  1 MiB floor on that branch bounds the worst case at about 128 MiB of resident staged index at 64
  workers, and no core count now commits more often than the 8-worker cadence. ([#62])

### Known limitations

- **Two concurrent full-tree scans of one workspace share one `scan-inflight.json`.** The second
  scan's record overwrites the first's, and the first scan's completion then removes the file while
  the second is still running — a window in which a genuine hard death would leave no breadcrumb.
  This is not reachable today (the daemon serialises a workspace's scans behind its store lock, and
  a `serve` boot's deferred and inline passes are sequential), and a per-pid filename would trade
  the window for records that accumulate and never get cleaned, so the single-file design stands.
  Recorded because the evidence is only trustworthy if its gaps are known. ([#62])
- **A memory ceiling on the daemon can only be enforced by whoever *starts* it.** basemind spawns the
  daemon detached with `setsid(2)`, which changes the session and the process group and **not** the
  cgroup. On Linux a child stays in the spawning process's cgroup, so a daemon auto-spawned from an
  interactive shell lands in that shell's scope — outside whatever `MemoryMax` an operator
  configured on a basemind unit — and a process cannot move its own children into a cgroup it does
  not control. There is no in-process fix, and none is claimed. The answer is
  `BASEMIND_NO_AUTOSPAWN=1` everywhere plus a unit that starts the daemon itself; see
  `docs/systemd/basemind-comms.service`, which includes the two commands that check whether the
  ceiling is real rather than merely configured. ([#62])
- **`[resources] max_footprint_mb` is advisory, not structural.** The gate parks workers at admit
  points while the process is over the ceiling and then admits anyway after five seconds, trading an
  overshoot for guaranteed forward progress. It shapes the peak; it cannot enforce an invariant the
  allocator will not, and it cannot make a scan that used to succeed start failing. The structural
  bounds this release adds are the ones that abort or truncate: `[scan] max_candidates`,
  `[code_search] max_chunks_per_file`, `[resources] max_map_cache_mb`, and the byte budget on staged
  index entries.
- **`[scan] max_candidates` does not bound bytes read or index size**, and does not apply to the
  `Staged` / `Rev` scan sources, which enumerate from git rather than from a walk.
- **Directory pruning is a walk-cost win, not a memory win.** Pruned files were already rejected by
  the path filter and never entered the candidate list. What pruning removes is the descent and the
  per-file `stat` — real, and the reason the Linux watcher stopped registering inotify watches
  inside excluded trees — but no claim is made that it reduced peak memory.
- **`SCANNER_STACK_SIZE` (256 MiB, `src/scanner_file.rs`) is not a leak and was deliberately left
  alone.** It is the per-worker stack reservation for the scanner's thread pool: lazily-committed
  virtual address space that never counts toward RSS or a cgroup limit. Shrinking it to "save
  memory" would save nothing and would reintroduce stack-overflow aborts on deeply nested syntax
  trees.

[#62]: https://github.com/Goldziher/basemind/issues/62

## [0.25.2] - 2026-08-31

### Added

- `basemind comms doctor --probe` asks each comms daemon whether it can actually serve and reports
  the verdict per row (`serving` / `LIVE BUT FAILING` / `LIVE BUT NOT RESPONDING`). The default
  report stays RPC-free, so it remains safe to run on a wedged machine, and now states which
  question it answered: process liveness from the pidfile registry, not health.
- A daemon that hits an unrecoverable store error records it to `<comms_dir>/last-fatal.json`
  before exiting. `comms doctor` reports that record — the failing method, pid, build, and age —
  so the evidence outlives the process; `--clear-fatal` acknowledges it.

### Changed

- Dependency refresh: `xberg` repinned to the current 1.1.0 revision, `crawlberg` 1.4.0 → 1.4.2,
  `fjall` 3.1.9 → 3.1.10, `jsonschema` 0.51 → 0.52, `lru` 0.18.2 → 0.18.3, `mach2` 0.6 → 0.7,
  `tree-sitter-language-pack` 1.15.8 → 1.15.12, and `liter-llm` 1.18.1 → 1.18.4.

### Fixed

- A comms daemon whose store became permanently unusable held the singleton forever: it kept the
  flock and the socket, so its pid stayed live and no replacement could take over, while every
  request failed identically. It now releases the lock and exits non-zero on the first such error,
  so the next `comms start` gets a clean daemon ([#61]).
- `basemind comms stop` hung indefinitely against a daemon wedged before its accept loop, because
  the connect never returned — the documented recovery path blocked on the very daemon it was
  meant to reclaim. Both stop paths now time-box the RPC and escalate to the pid. A daemon that
  answers with a deliberate refusal (`daemon_busy`, protecting the clients attached to it) is
  reported and left running, never terminated.
- `comms status` reported `threads: 0` when the store could not be read, which is indistinguishable
  from a healthy empty broker. It now surfaces the store error instead.

[#61]: https://github.com/Goldziher/basemind/issues/61

## [0.25.1] - 2026-08-26

### Changed

- Dependency refresh: `xberg` repinned to the current 1.1.0 revision (EPUB chapter/metadata loss,
  PDF reading-order and CCITT image decoding, and XML/MathML hardening fixes on the document
  extraction path), `crawlberg` 1.3.3 → 1.4.0, `gix` 0.87.0 → 0.87.1, `jsonschema` 0.50 → 0.51, and
  `tree-sitter-language-pack` 1.15.7 → 1.15.8.

### Fixed

- The Linux file watcher no longer aborts when a directory cannot be read (permission denied).
  inotify watches are now registered directory-by-directory, pruned by the same gitignore +
  exclude-glob filters a full scan uses, with registration failures downgraded to warnings.
  Directories created after startup are re-armed on create events, and the per-directory scheme is
  gated to Linux (macOS/Windows keep the single recursive call to avoid an FSEvents startup
  regression).

## [0.25.0] - 2026-08-23

### Added

- MCP stdio sessions now reconnect through a persistent protocol-aware relay when the shared daemon
  restarts, replaying initialization and returning retryable errors for interrupted requests.
- Agent comms now auto-register presence, deliver mailbox notices alongside ordinary MCP responses,
  notify scoped agents about discoverable threads, accept unambiguous short message references, and
  expose lifecycle status plus dry-run/apply retention cleanup.
- Document extraction uses xberg 1.1 chunk content, offsets, language metadata, bounded extraction
  time/page limits, and basemind's shared batch embedding path. Binary builds pin the unreleased
  xberg source to an exact public Git revision for reproducibility.

### Changed

- **Cache schema advances to 25.** Existing machine-global index, blob, and comms cache state is
  rebuilt on the next scan after upgrading.
- Cache GC repairs corrupt or old rebuildable views instead of aborting the global sweep, enforces
  the disk budget against rebuildable workspace data, and records degraded GC health.
- Comms retention now bounds messages, archived threads, inactive agents, claims, and total threads;
  stale-agent cleanup removes routing state atomically while preserving authored history.
- Heavy MCP work is admitted through a daemon-wide concurrency limit and returns a retryable
  `server_busy` response under sustained load.

### Fixed

- Inbox cursors paginate independently per thread without repeats or undercounted unread totals.
- Message reads and acknowledgements enforce thread membership.
- Cancelled inbox waits release their subscriptions immediately, and default MCP identities no
  longer collide across concurrent sessions in one workspace.
- Standalone scan and MCP processes no longer hang during shutdown when Fjall's background worker
  teardown encounters the upstream database-drop deadlock.

## [0.24.0] - 2026-08-07

### Added

- **Interactive UI over HTTP + the `ui` tool (ADR-0006).** A new `ui` MCP tool (and `query ui` CLI, in
  full parity) renders the code-graph and returns a URL to view and drive it: a live
  `http://<addr>/ui?root=…` page served by the basemind daemon's HTTP front-end when one is reachable,
  otherwise a `file://` URL to the self-contained offline export (which the tool always writes). The
  served page is the same zero-dependency interactive canvas as `display`/`graph_export html`, so a
  browser — or an agent driving one — can navigate, search, pan/zoom, and screenshot it. `open: false`
  returns the URL without launching a viewer (agents/tests). The daemon's streamable-HTTP front-end
  gained a `GET /ui` route alongside `POST /mcp` (both behind the `comms` feature). Both routes are
  loopback-only: on a loopback bind the front-end rejects any request whose `Host`/`:authority` is not
  a loopback name (DNS-rebinding protection), skipped only on an explicit non-loopback bind.
- **Desktop UI launch path (ADR-0006, first slice).** A `basemind ui` subcommand launches the desktop
  UI front-end (`basemind-ui`) by re-exec'ing the sibling binary built alongside `basemind` —
  mirroring how `basemind agent` launches `basemind-tui`. Both launchers are behind the `desktop-ui`
  and `agent-tui` features and ship in no release archive yet (see Changed); build them locally to
  use them. This slice wires the launch path; today the binary opens the offline, self-contained
  interactive HTML code graph (the same ADR-0005 payload the `display` tool produces). The resident Tauri window that
  drives the agent client/event/command seam lands behind a `desktop` feature in a later slice, so
  default builds stay free of the per-platform webview dependency.
- **Inline rationale extraction (ADR-0009).** L1 extraction now classifies source comments carrying
  `WHY:` / `RATIONALE:` / `NOTE:` / `TODO:` / `FIXME:` / `HACK:` / `XXX:` / `SAFETY:` markers (and
  bare comments citing `ADR`/`RFC` decision records) into `FileMapL1.rationale`, which the read-side
  graph build consumes to emit `Annotates` / `Cites` edges. Blob-compatible (`#[serde(default)]`); no
  schema bump.
- **Single-daemon guarantees + leak safeguards.** The comms daemon now takes an exclusive
  single-owner lock (`daemon.lock`) *before* binding its socket, so a redundant daemon on the same
  comms dir converges (exits 0) instead of racing — uniform across Unix and Windows. Every live daemon
  registers a pidfile under `<data_home>/daemons/`, and a new machine-wide ceiling
  (`BASEMIND_MAX_DAEMONS`, default 8) **refuses** to spawn past it rather than letting daemons pile up.
  New `basemind comms doctor` lists every live daemon (pid / comms dir / version / uptime, pruning dead
  entries) and flags a pile-up; `basemind comms stop --all` reclaims them all in one call.

### Changed

- **Cache schema advances to 24.** Existing machine-global index, blob, and comms cache state is
  rebuilt on the next scan after upgrading.

- **Release dependencies are current.** `tree-sitter-language-pack` is updated to 1.14.3,
  `crawlberg` to 1.1.4, and `liter-llm` to 1.16.0. The optional LSP bincode integration is pinned to
  2.0.1 because the published 3.0.0 crate is an intentionally non-compiling placeholder.

- **Agent shell sessions now default to headless presentation.** Background MCP/CLI work no longer
  opens terminal tabs unless `[shells].visual` explicitly selects `current` or `window`.

- **The two interactive front-ends are held back from releases.** The agent TUI (`basemind-tui`,
  ratatui) and the desktop UI (`basemind-ui`, tauri) are not ready to ship, so the release archive now
  contains the `basemind` binary alone, and the launcher subcommands that would re-exec them sit
  behind two new root-crate features, `agent-tui` and `desktop-ui`, which `full` deliberately omits.
  A released `basemind` therefore never advertises a command whose sibling binary is absent from the
  archive. Neither crate is affected otherwise: both remain workspace members, both still build under
  `cargo build --workspace`, the `basemind-tui` replay/PTY suite still runs in CI, and a local
  `cargo install --path . --features full,agent-tui,desktop-ui` still gets both launchers. Nothing was
  published from these crates before — both carry `publish = false` — so no crates.io surface changes.

- **Daemons self-reap sooner to stop the process/memory leak class.** The idle-reap default drops
  from 30 min to 10 min, and a new **bootstrap reaper** self-terminates a daemon that no client ever
  connects to within ~2 min (env `BASEMIND_COMMS_BOOTSTRAP_SECS`) — catching the spawn-and-abandon
  shape (a test or script that ran `daemon ensure` then exited) that previously left a daemon, and its
  tens of threads, resident. Behavioural only; no schema/`RELEASE_MINOR` bump. Test binaries pin a fast
  reap so a parallel `cargo test` no longer accumulates one lingering daemon per binary.

- **BREAKING: `basemind-agent`'s LLM-facing tool names follow the domain consolidation.** The agent
  registered its code-map tools under the pre-consolidation flat names, so the model saw one
  vocabulary and the MCP surface another. They are now `domain_mode` in bare snake_case:
  `outline` → `code_outline`, `search_symbols` → `code_symbols`, `find_references` →
  `code_references`, `find_callers` → `code_callers`, `workspace_grep` → `code_grep`, `call_graph` →
  `graph_calls`, `recent_changes` → `git_recent`, `blame_symbol` → `git_blame_symbol`, `diff_file` →
  `git_diff`. `shell_exec` is unchanged — it is the agent's own tool, not an MCP domain. The
  underscore rather than the MCP surface's colon is a provider constraint, not a style choice:
  Anthropic, OpenAI and Google all validate tool names against `^[a-zA-Z0-9_-]{1,128}$`, so
  `code:outline` is not a legal name to register. Only saved conversations that replay a raw tool
  name are affected. The token-savings estimator now rewrites the agent spelling to the MCP
  telemetry key at the boundary, keyed off the real mode vocabulary in `src/mcp/mode.rs`, so each
  baseline carries a single spelling — previously it carried both, and dropping either would have
  silently zeroed the agent TUI's "tokens saved" readout instead of failing a test.

- **`call_graph` is re-expressed over the shared `codegraph`.** The tool no longer maintains its own
  call scan; it projects the resolved `Calls` edges of the shared, memoized graph the other graph
  tools already read. One user-visible consequence: a call whose callee does not resolve to an
  in-repo function-like definition (an external/library call) no longer surfaces as an empty-`sites`
  callee node — `call_graph` now reports only resolved, in-repo call relationships. The `callers`
  direction is unchanged. Read-side only; no blob/index schema change.

- **BREAKING: the 85-tool MCP surface collapses into nine domain tools, each dispatched by a
  required `mode`.** Hosts defer MCP tools and surface them only through keyword search, so every
  extra tool name competed for the same query, and GH #50 showed one layer's schema defect took the
  whole registry down with it — the comms layer's schema broke code navigation, despite the two
  sharing nothing. Nine dense tools (`code`, `graph`, `git`, `memory`, `admin`, `web`, `agents`,
  `workspace`, `shell`) retrieve better than eighty-five thin ones, and each remaining description
  now carries the vocabulary an agent actually searches for. `mode` is required and never defaults —
  a default would let an agent omit it and silently get the wrong operation, which looks like an
  empty result rather than an error. The CLI collapses onto the same nine group names as real clap
  subcommands (`basemind code outline`, not a `--mode` flag), so every operation keeps its own
  `--help` and argument validation; `tests/cli_parity.rs` now asserts a strict `(domain, mode)`
  bijection between the two surfaces. All 85 operations survive with no deprecated shims — a name
  that still answered to the old spelling would keep competing in deferred-tool search, which is the
  cost this removes. Landed over four commits (`88cd9da` web, `6af6268` admin/memory/workspace,
  `36919a4` git/graph, `02bf4a5` code/agents/shell); design and trade-offs in
  [ADR-0011](docs/adr/0011-mcp-tool-surface-redesign.md). Two costs taken deliberately: no
  `output_schema` on any of the nine tools (each domain's modes return different response shapes,
  and SEP-2106 allows exactly one schema per tool), and per-tool annotations coarsen to the union of
  a domain's modes, resolving toward the side effect (e.g. `shell` advertises `destructive_hint`
  because `kill` is one of its modes, and `graph` advertises `read_only_hint: false` because
  `display`/`open` launch a viewer, even though seven of its nine modes are pure reads).

  Anyone with a saved prompt, script, or pinned tool name against the old surface needs the
  replacement below — `<old tool>` is now `<domain>` mode `<mode>`, and every CLI invocation moves
  from its old subcommand path to the new one. `basemind registry` is also renamed `basemind
  workspace` (its subcommands are unchanged), and `basemind comms` now holds only the broker
  daemon's own lifecycle (`daemon` / `start` / `stop` / `status` / `doctor`) — the agent-facing verbs
  it used to carry (`register`, `agents`, `thread-*`, `join`, `leave`, `members`, `post`, `history`,
  `read`, `inbox`, `wait`, plus a new `ack`) moved to `basemind agents`.

  | Old MCP tool | New `domain` `mode` | Old CLI | New CLI |
  |---|---|---|---|
  | `outline` | `code` `outline` | `query outline <path> [--l2]` | `code outline <path> [--l2]` |
  | `search_symbols` | `code` `symbols` | `query search <needle>` (alias `query symbol`) | `code symbols <needle>` |
  | `workspace_grep` | `code` `grep` | `query grep <pattern>` | `code grep <pattern>` |
  | `list_files` | `code` `files` | `query list-files` | `code files` |
  | `find_files` | `code` `find` | `query find-files <query>` | `code find <query>` |
  | `goto_definition` | `code` `definition` | `query goto-definition <path> <line>` | `code definition <path> <line>` |
  | `find_references` | `code` `references` | `query references <name>` | `code references <name>` |
  | `find_callers` | `code` `callers` | `query callers <path> <name>` | `code callers <path> <name>` |
  | `find_implementations` | `code` `implementations` | `query implementations <trait>` | `code implementations <trait>` |
  | `dependents` | `code` `dependents` | `query dependents <module>` | `code dependents <module>` |
  | `expand` | `code` `expand` | `query expand <path> <name>` | `code expand <path> <name>` |
  | `search_code` | `code` `semantic` | `query search-code <query>` | `code semantic <query>` |
  | `get_chunk` | `code` `chunk` | `query get-chunk <path>` | `code chunk <path>` |
  | `call_graph` | `graph` `calls` | `query call-graph <name>` | `graph calls <name>` |
  | `neighbors` | `graph` `neighbors` | `query neighbors <name>` | `graph neighbors <name>` |
  | `path` | `graph` `path` | `query path <from> <to>` | `graph path <from> <to>` |
  | `subgraph` | `graph` `subgraph` | `query subgraph <name>` | `graph subgraph <name>` |
  | `communities` | `graph` `communities` | `query communities` | `graph communities` |
  | `architecture_map` | `graph` `map` | `query architecture-map` | `graph map` |
  | `graph_export` | `graph` `export` | `query graph-export` | `graph export` |
  | `display` | `graph` `display` | `query display` | `graph display` |
  | `ui` | `graph` `open` | `query ui` | `graph open` |
  | `working_tree_status` | `git` `status` | `git working-tree-status` | `git status` |
  | `recent_changes` | `git` `recent` | `git recent-changes` | `git recent` |
  | `commits_touching` | `git` `touching` | `git commits-touching <path>` | `git touching <path>` |
  | `find_commits_by_path` | `git` `by_path` | `git find-commits-by-path <pattern>` | `git by-path <pattern>` |
  | `hot_files` | `git` `churn` | `git hot-files` | `git churn` |
  | `diff_file` | `git` `diff` | `git diff-file <path> <old> <new>` | `git diff <path> <old> <new>` |
  | `diff_outline` | `git` `diff_outline` | `git diff-outline <path>` | `git diff-outline <path>` |
  | `blame_file` | `git` `blame` | `git blame-file <path>` | `git blame <path>` |
  | `blame_symbol` | `git` `blame_symbol` | `git blame-symbol <path> <name>` | `git blame-symbol <path> <name>` |
  | `symbol_history` | `git` `symbol_history` | `git symbol-history <path> <name>` | `git symbol-history <path> <name>` |
  | `search_git_history` | `git` `search` | `git search <pattern>` | `git search <pattern>` |
  | `memory_put` | `memory` `put` | `memory put <key> <value>` | `memory put <key> <value>` |
  | `memory_get` | `memory` `get` | `memory get <key>` | `memory get <key>` |
  | `memory_list` | `memory` `list` | `memory list` | `memory list` |
  | `memory_search` | `memory` `search` | `memory search <query>` | `memory search <query>` |
  | `memory_delete` | `memory` `delete` | `memory delete <key>` | `memory delete <key>` |
  | `memory_audit` | `memory` `audit` | `governance audit` | `memory audit` |
  | `search_documents` | `memory` `documents` | `memory search-documents <query>` | `memory documents <query>` |
  | `proposals_mine` | `memory` `mine` | `governance mine` | `memory mine` |
  | `proposals_list` | `memory` `proposals` | `governance proposals` | `memory proposals` |
  | `proposal_accept` | `memory` `accept` | `governance accept <id>` | `memory accept <id>` |
  | `proposal_reject` | `memory` `reject` | `governance reject <id>` | `memory reject <id>` |
  | `status` | `admin` `status` | `query status` | `admin status` |
  | `repo_info` | `admin` `repo` | `query repo-info` | `admin repo` |
  | `rescan` | `admin` `rescan` | `basemind rescan [paths]` | `admin rescan [paths]` |
  | `cache_stats` | `admin` `cache_stats` | `cache stats` | `admin cache-stats` |
  | `cache_gc` | `admin` `gc` | `cache gc` | `admin gc` |
  | `cache_clear` | `admin` `cache_clear` | `cache clear --component <c>` | `admin cache-clear --component <c>` |
  | `telemetry_summary` | `admin` `telemetry` | `basemind telemetry` | `admin telemetry` |
  | `compress` | `admin` `compress` | `basemind compress-output` | `admin compress` |
  | `delta` | `admin` `delta` | `basemind delta` | `admin delta` |
  | `checkpoint` | `admin` `checkpoint` | `basemind checkpoint` | `admin checkpoint` |
  | `detect_waste` | `admin` `waste` | `basemind detect-waste` | `admin waste` |
  | `web_scrape` | `web` `scrape` | `web scrape <url>` | `web scrape <url>` |
  | `web_crawl` | `web` `crawl` | `web crawl <seed-url>` | `web crawl <seed-url>` |
  | `web_map` | `web` `map` | `web map <url>` | `web map <url>` |
  | `agent_register` | `agents` `register` | `comms register --name <handle>` | `agents register --name <handle>` |
  | `agent_list` | `agents` `list` | `comms agents [--thread]` | `agents list [--thread]` |
  | `thread_start` | `agents` `thread_start` | `comms thread-start` | `agents thread-start` |
  | `thread_list` | `agents` `thread_list` | `comms threads [--subject-contains]` | `agents thread-list [--subject-contains]` |
  | `thread_join` | `agents` `join` | `comms join <thread>` | `agents join <thread>` |
  | `thread_leave` | `agents` `leave` | `comms leave <thread>` | `agents leave <thread>` |
  | `thread_members` | `agents` `members` | `comms members <thread>` | `agents members <thread>` |
  | `thread_add_member` | `agents` `add_member` | `comms add-member <thread> <id>` | `agents add-member <thread> <id>` |
  | `thread_remove_member` | `agents` `remove_member` | `comms remove-member <thread> <id>` | `agents remove-member <thread> <id>` |
  | `thread_archive` | `agents` `archive` | `comms archive <thread>` | `agents archive <thread>` |
  | `thread_post` | `agents` `post` | `comms post <thread> <subject>` | `agents post <thread> <subject>` |
  | `thread_history` | `agents` `history` | `comms history <thread>` | `agents history <thread>` |
  | `message_get` | `agents` `message` | `comms read <id>` | `agents message <id>` |
  | `inbox_read` | `agents` `inbox` | `comms inbox [--mark-read]` | `agents inbox [--mark-read]` |
  | `inbox_ack` | `agents` `ack` | *(new — no prior tool)* | `agents ack` |
  | `inbox_wait` | `agents` `wait` | `comms wait [--thread --timeout-secs]` | `agents wait [--thread --timeout-secs]` |
  | `workspaces` | `workspace` `workspaces` | `registry workspaces` | `workspace workspaces` |
  | `worktrees` | `workspace` `worktrees` | `registry worktrees` | `workspace worktrees` |
  | `branches` | `workspace` `branches` | `registry branches` | `workspace branches` |
  | `worktree_claim` | `workspace` `claim` | `registry claim` | `workspace claim` |
  | `worktree_release` | `workspace` `release` | `registry release` | `workspace release` |
  | `shell_spawn` | `shell` `spawn` | `shells spawn <command>` | `shell spawn <command>` |
  | `shell_send` | `shell` `send` | `shells send <session-id> <text>` | `shell send <session-id> <text>` |
  | `shell_capture` | `shell` `capture` | `shells capture <session-id>` | `shell capture <session-id>` |
  | `shell_kill` | `shell` `kill` | `shells kill <session-id>` | `shell kill <session-id>` |
  | `shell_list` | `shell` `list` | `shells list` | `shell list` |
  | `shell_broadcast` | `shell` `broadcast` | `shells broadcast <text> --session <id>…` | `shell broadcast <text> --session <id>…` |

### Fixed

- **Recycling the shared daemon no longer disconnects other agents.** `basemind comms stop` now
  refuses with `daemon_busy` while another live relay is connected, instead of entering a bounded
  drain that severed every stdio MCP transport after ten seconds and left hosts permanently showing
  `Transport closed`. Once the other sessions exit, the same stop request succeeds normally.
- **EndNote XML is rejected until xberg ships its quick-xml fix.** The document scanner now blocks
  `.enw` / `application/x-endnote+xml` before xberg 1.0.14 can route untrusted input through
  biblib 0.4.3's vulnerable quick-xml 0.37 attribute parser (RUSTSEC-2026-0194). The narrow guard is
  tracked for removal in [xberg #1398](https://github.com/xberg-io/xberg/issues/1398).
- **Resolved imported calls could remain inferred in code graphs.** The reference index now prefers
  a terminal cross-file definition over the local import binding, and code-graph proof matches the
  resolver's identifier byte within the outline symbol span. Python callers and goto-definition now
  agree on the exported target.
- **Cache budget enforcement could remain over its configured limit.** Eviction now prefers cold
  workspaces but falls back to hot ones when required, reclaims newly orphaned blobs in the same
  pass, and remeasures the actual cache footprint before selecting another workspace.
- **Starved or failed daemon GC cycles were visible only in logs.** `cache_stats.last_gc` now records
  the latest attempt status, timestamp, actionable diagnosis, consecutive degraded-cycle count,
  and budget outcome while preserving counters from the last successful sweep.
- **Content-addressed blobs no longer consume raw msgpack size on disk (issue #49).** New `.fm`,
  `.doc`, `.rref`, and `.chunk` blobs use zstd-1 envelopes. Filemap L1/L2 frames remain independent
  so outline reads do not decompress calls, and chunk embedding metadata stays plain for cheap cache
  validation. Existing uncompressed blobs remain readable and are rewritten when next produced.
- **Agent shell capture could return an empty screen for a successful command.** The snapshot-only
  path dropped the first row for fresh CLI clients and let rmux's dead-pane row push short output
  out of `lines=N` captures. Capture now reads retained history, removes terminal synthetic and
  outer blank rows while preserving interior spacing, and returns a bounded recent tail, so a
  completed one-line command remains observable until `kill`.
- **Shared shell sessions could start in another workspace.** Relative or omitted `cwd` values were
  interpreted from the machine-global rmux daemon's original process directory. Spawn now anchors
  every command to the calling workspace root, and accepts `cwd = "."` as that root.
- **A transient pane inspection could make `shell list` fail globally.** The daemon now preserves
  the known session list and conservatively reports liveness when rmux cannot provide one pane-state
  snapshot, rather than making every session undiscoverable. Its idle reaper uses strict liveness
  separately, so persistent inspection failure cannot pin the daemon forever.
- **A resident daemon running different code at the same version was invisible.** After a local
  rebuild or `cargo install --path .`, the version check compares only `MAJOR.MINOR.PATCH`, so the
  daemon left over from the previous build passes every test and is reused — answering with stale
  code while the newly installed binary sits unused. `basemind comms status` now reports a `build=`
  identity (a hash of the running executable, so two builds differ exactly when their bytes do) and
  warns explicitly when the daemon's build differs from the binary asking. The build id is
  diagnostic only: it deliberately does not trigger takeover, because two same-version binaries at
  different paths would then reclaim the socket from each other indefinitely.
- **`admin rescan` reported a refreshed document in no bucket at all.** A rescan that re-indexed a
  changed PDF/Office/HTML file returned `scanned: 3, skipped_unchanged: 2, updated: 0` — the
  document lane's work was counted by the scanner but dropped from the response, which read as
  "nothing happened" for documents users. The response now carries `docs_indexed`. Indexed data was
  never stale; only the accounting was.
- **`agents post` accepted an empty `body` and stored a message with nothing to read.** An empty
  string is now refused with an error naming the mode and pointing at the subject-only form; omitting
  `body` entirely stays legal.
- **The machine registry never retired a row.** Workspaces were recorded on registration and nothing
  ever removed them, so every throwaway checkout and test tempdir accumulated forever and buried the
  live repos `workspace workspaces` exists to surface. The daemon now prunes rows whose path is gone,
  once at startup and on its hourly maintenance pass.

## [0.23.1] — 2026-07-31

### Fixed

- **`code semantic` silently returned nothing on a reader session.** Both non-vector retrieval lanes
  — BM25 keyword and exact symbol-name — read fjall, whose directory lock is exclusive, so any
  session that is not the daemon opens the store without an index and both lanes returned an empty
  vector. That empty was indistinguishable from "searched and found nothing": the call succeeded, in
  microseconds, with `hits: []`. A reader session now forwards the two lanes to the daemon (rankings
  only — chunk bodies still come from content-addressed blobs the caller can already read), and a
  daemon-hosted session reaches the same index straight through the host seam rather than dialling
  the daemon it runs inside. When a lane genuinely cannot run, the response carries
  `degraded_lanes` + `degraded_reason` and partial results are labelled as partial; when no lane can
  run at all the call is an error that says so, never a bare `[]`. Keyword-lane hits also carry
  their `matched_lanes` / `keyword_rank` provenance, which only the fused path used to set.
- **The `multi-agent-room` skill now ships to every plugin tree.** `scripts/sync-plugin-skills.sh`
  carried a hand-maintained list of nine skills while `skills/` held ten, so `multi-agent-room` was
  copied into the Hermes bundle (generated separately by ai-rulez) but never into the Codex, Cursor
  or opencode plugins — while the `agent-comms` rule those plugins *do* ship tells the agent to
  consult it. The skill and command lists are now derived from the canonical `skills/` and
  `commands/` trees, so adding either ships it everywhere instead of only where someone remembered.
- **Documentation named tools and commands that no longer exist.** The consolidation swept the
  README, `llms.txt`, `docs/`, `skills/` and the plugin manifests but missed the `website/` docs
  site, the `pre-tool-guard` hook (which suggested `search_symbols` / `find_references` /
  `list_files` on every Grep and Glob), and `basemind git blame-file` in the README quickstart.
  Every `basemind <domain> <mode>` command now appearing in the docs or plugin trees is verified to
  resolve against the built binary.
- **Runaway daemon spawning under `cargo test` (fork bomb).** `spawn_detached_daemon` re-exec'd
  `std::env::current_exe()` with `comms daemon` appended. Under a test harness that executable is
  libtest, which reads the appended words as a **test-name filter** and re-runs the whole suite — and
  every test in that re-run reaching the spawn forked another generation. A single run reached 5610
  orphaned daemons and 8256 processes, at which point `fork` itself began failing. The machine-wide
  ceiling could not contain it because `init_isolated_cache` minted a fresh `$BASEMIND_DATA_HOME` in
  every process, so each generation counted an empty registry. Now the spawn resolves a real
  `basemind` binary or refuses: a harness is redirected to the sibling `target/<profile>/basemind`,
  and `BASEMIND_DAEMON_BINARY` is the explicit override. The isolated test cache is stamped with a
  marker and re-adopted by child processes, so one registry — and one ceiling — covers the whole
  process tree, while an unmarked `$BASEMIND_DATA_HOME` (a developer's real cache) is never adopted.
- **Blank statusline in global-cache mode.** With the index living in the machine-global cache there
  is no in-repo `.basemind/` for the shell statusline to read, so it fell back to a permanent "no
  index" hint even on a freshly-scanned repo. `basemind statusline` gains a `--root <path>` mode that
  renders the rich per-repo line (file count · scan age · today's calls · tokens saved) from a cheap
  new `status.json` sidecar plus the `telemetry.jsonl` tail — never opening the Fjall index, so it is
  safe to refresh every few seconds. The sidecar is written after every working-view scan/rescan and
  is additive/best-effort (a missing or unrecognized-schema sidecar degrades to the "no index" hint).
  No blob/index schema change.

## [0.23.0] — 2026-07-31

Minor release: **`.basemind/` is wiped and rebuilt on the next `basemind scan`** (`RELEASE_MINOR`
22 → 23). Rebuild is automatic from source; no action needed.

### Added

- **Streamable-HTTP MCP transport (rmcp 3.0, SEP-2567), hosted by the comms daemon.** The single
  machine-global daemon now serves the rmcp router over a stateless loopback-HTTP front-end
  (`127.0.0.1:51786` by default, `BASEMIND_HTTP_ADDR` to override; the bound address is published to
  `<comms_dir>/http.addr`) in addition to its Unix-socket relay. HTTP-native clients dial
  `http://127.0.0.1:<port>/mcp?root=<repo>` directly — no per-session server process. `basemind
  daemon ensure` brings the daemon + transport up and prints the URL.
- **Structured tool output (SEP-2106).** Every tool advertises an `outputSchema` and returns
  `structuredContent` alongside the text mirror, so compliant clients get typed results.
- **`tools/list` / `prompts/list` cache hints (SEP-2549)** — the tool surface is static per process,
  so the daemon advertises a TTL + public cache scope and clients stop re-listing every session.
- **Long-running tools as pollable tasks (SEP-2663).** Slow tools (`rescan`, and the feature-gated
  `search_documents` / `web_scrape` / `web_crawl` / `web_map`) run as cancellable tasks when the
  client declares the Tasks capability; other clients keep the synchronous path.

### Changed

- **rmcp 2.2 → 3.0**, plus `arrow`, `lancedb`, `xberg`, `crawlberg`, and `jsonschema` dependency
  bumps.
- **`basemind serve` is now a thin stdio↔daemon relay, not a server in its own right.** It ensures
  the daemon (the sole rmcp host and Fjall writer) and byte-pumps stdin/stdout to the daemon's relay
  socket. Every stdio MCP client keeps working with no config change; the daemon outlives any single
  client. The Claude Code and Cursor plugins switch to the HTTP transport (`type: http`) and dial the
  daemon directly.

### Fixed

- **The MCP transport "drops and never returns" hang.** `serve` previously fell back to an
  in-process stdio server whose lifetime was bound to the stdin pipe, so a slow tool call that timed
  out and closed stdin killed the whole server — and it did not come back until the client respawned
  it. The daemon-hosted model decouples server lifetime from any single connection: a dropped client
  ends only the throwaway relay (or a single stateless HTTP request), and the warm daemon serves the
  next connection immediately.
- **Document re-embed loop follow-ups (issue #44).** Three residual thrash paths on the document
  tier are closed: (1) a hosted rescan now honors the daemon drain token, so `comms stop` / SIGTERM
  interrupts a mid-scan instead of letting it run to completion; (2) a reused document blob has its
  mtime refreshed on every scan, so the blob GC's grace window keeps protecting an actively-scanned
  doc through a rename's transient entry-less gap instead of reaping it and forcing a full re-embed;
  (3) a doc whose embedding deterministically yields no vectors (an unembeddable body, a missing ONNX
  runtime) is now recorded as *attempted* and no longer re-extracted + re-embedded on every scan — a
  content-hash or embedding-preset change re-opens the attempt.

## [0.22.8] — 2026-07-27

### Changed

- **`basemind init` no longer writes a committed `CLAUDE.md` / `AGENTS.md` unasked.** The rules
  block is a shared, version-controlled surface, so onboarding now defaults to the personal,
  gitignored sibling instead: `--rules-target auto` (the default) routes to ai-rulez when it owns
  governance, otherwise to `CLAUDE.local.md` (or `AGENTS.local.md` when the repo uses AGENTS) —
  never a committed file. The `/bm-init` slash command asks the user where the block should go, and
  the interactive CLI prompts for it; `--rules-target` gains `claude-local` / `agents-local`
  values, and `claude` / `agents` remain as the explicit opt-in to the committed files.

### Fixed

- **Comms daemon readiness flake (`spawned comms daemon did not become ready within the timeout`).**
  The Unix comms front-end peeked a freshly-accepted connection's first byte _inline in the accept
  loop_ (`peek_first_byte(&stream).await` before the spawn), so a single client that connected but
  was slow or silent to send its first byte parked the entire accept loop on `readable()`. Every
  other client then sat unaccepted in the kernel backlog — including `ensure_daemon`'s readiness
  probe, whose short timeout reported a perfectly healthy, still-running daemon as unreachable, which
  in turn spawned a replacement that hit `AddrInUse` and exited (`SpawnTimeout`). The peek + relay/
  legacy routing now runs *inside* the per-connection spawned task, so the accept loop only
  `accept()`s and `spawn`s and never awaits client I/O — a slow/silent client harms only its own
  task. This aligns the Unix path with the Windows named-pipe front-end, which already reads the
  first byte in-task. No blob/index schema change.

## [0.22.7] — 2026-07-25

### Changed

- **`basemind serve` is now a thin relay to the singleton daemon; the daemon hosts the MCP router.**
  Every client's `serve` used to open its own read-only store, build its own in-RAM code map, and
  lazily load its own ONNX + LanceDB (~300–370 MB RSS **per process**). On a `comms` build serving
  the working view, `serve` now dials the daemon, runs a tiny handshake, and pumps stdio bytes to it
  — the daemon hosts the rmcp router over **one shared read stack per workspace** (`SharedReadStack`),
  so the in-RAM map, ONNX model, LanceDB, and git caches are resident **once per workspace** instead
  of once per client. `ServerState` was split into a per-connection identity (`agent_id`,
  `comms_clients`, `log_level`) plus that shared stack, so distinct clients keep distinct memory-owner
  identities while sharing the heavy read state. Fallback is total: any handshake problem — no daemon,
  an older daemon (which rejects the `RELAY_MAGIC` preamble as an over-long frame), a proto/view
  mismatch, or `BASEMIND_MCP_LEAN` / a non-working view / `BASEMIND_SERVE_INPROCESS` — serves
  in-process exactly as before, so a client is never bricked. No blob/index schema change.
- **Daemon-hosted git-history now runs in-process** instead of the daemon dialing itself. A hosted
  connection's history reads and its startup sync route through a new in-process seam
  (`HistoryHost`, implemented by the daemon `Broker`) that shares the Broker's single open
  `git-history.fjall/` handle and per-repo build lock — so `recent_changes` / `commits_touching` /
  `blame_*` / `search_git_history` no longer pay a unix-socket round trip per call on the hosted
  path, while the single-holder + single-flight-build invariants are unchanged. A thin
  `daemon_writer` serve (the fallback) still forwards over the socket.
- **The `serve` relay now works on Windows named pipes.** Named pipes have no non-destructive peek
  (the Unix path uses `MSG_PEEK`), so the accept loop consumes the connection's first byte and
  replays it — to the relay handshake via a small `PrefixReader`, or to a legacy comms link via a
  seeded frame buffer — routing rmcp relay connections apart from comms links without a fallback to
  in-process serve.

### Fixed

- **Daemon-hosted precise `resolved_refs` (and `search_code`) no longer degrade.** Hosted connections
  read the daemon's read-write fjall index directly in-process via a host seam, so cross-file
  `find_callers` / `goto_definition` see the `refs_by_def` edges an index-less thin serve could not —
  without the daemon dialing itself over its own socket.

### Removed

- Deleted the orphaned `monitors/monitors.json` + `scripts/comms-monitor.sh` poll-loop scripts
  (referenced by no manifest; the daemon's `SubscribeInbox` push channel supersedes them).

## [0.22.6] — 2026-07-25

### Fixed

- **The older-build daemon takeover now actually fires** (#44). `singleton::roundtrip` decoded a
  bare `CommsResponse`, but the daemon frames every reply as a `CommsOut::Response` envelope, so the
  decode always failed and `daemon_status` always returned `None`. `ensure_daemon` never entered its
  version gate and reused whatever daemon held the socket — so a live pre-0.22.5 runaway daemon
  survived the client upgrade instead of being reaped (it had to be killed by hand). The probe now
  decodes the `CommsOut` envelope; the 0.22.5 cache/GC remediation actually reaches machines with a
  lingering older daemon. Regression present since the takeover was introduced.

### Changed

- Refreshed the dependency lockfile (`auto_enums`, `derive_utils`). `cargo upgrade --incompatible`
  is a no-op: every incompatible upgrade is a deliberately pinned crate (`arrow-*` `=58`, `oxc_*`,
  `tree-sitter-python`).

## [0.22.5] — 2026-07-25

Hardening release after a starved-GC incident: a pre-0.22.4 daemon stuck in the #44 re-embed loop
held the blob-GC read lock indefinitely, which silently stopped **all** daemon maintenance and let
the machine-global cache grow to 116 GB with no signal anywhere. Updating also remediates lingering
pre-fix daemons: a 0.22.5 client stops any older-build daemon on first contact (existing
older-build takeover) instead of leaving it to run away.

### Added

- **Cache size budget with cold-workspace eviction.** `BASEMIND_CACHE_BUDGET_MB` (default 20 GiB;
  `0` = unlimited) bounds the machine-global cache. When workspaces + blobs exceed it, the GC
  sweep evicts the coldest workspace dirs (idle ≥ 24 h, never ones another process holds locked)
  and reclaims the blobs they pinned — the backstop the reference-counting GC lacked, since
  live-but-idle workspaces pin their blobs forever.
- **Persisted GC observability.** Every destructive sweep records its outcome (timestamp, blobs
  removed, bytes freed, workspaces reaped/evicted) to `gc-state.json` in the cache root, and
  `cache_stats` surfaces it as `last_gc` — a GC that stops running is now visible instead of a
  silent disk leak.

### Fixed

- **Blob GC runs at daemon startup and on its own task** (#44). It previously ran only as the
  last step of the hourly prune loop: a daemon restarting more often than hourly never swept once,
  and a single starved GC await stopped message pruning, thread archiving, and workspace eviction
  for the life of the process.
- **The GC's lock wait is bounded** (#44). Waiting for the blob-GC write lock now times out after
  5 minutes (logged, retried next cycle) instead of parking forever behind a runaway rescan's read
  guard — which, the lock being FIFO, also queued every subsequent rescan behind the stuck writer
  and wedged the daemon machine-wide.
- **A draining daemon refuses new rescans** (`rescan_draining`) instead of running doomed partial
  passes, and a rescan that queued behind a drain-cancelled pass fails fast instead of re-walking
  the tree — cancelled passes never advance the coalescing generation, so each one previously
  re-walked the whole monorepo.

### Changed

- Upgraded `gix` to 0.86 and `xberg` to 1.0.0-rc.37; `arrow-array`/`arrow-schema` are pinned to
  `=58` (required by LanceDB 0.31).

## [0.22.4] — 2026-07-23

### Fixed

- **The daemon no longer loops full document re-embed passes on churny trees** (#44 — ~700 % CPU
  for hours, RSS to 5.4 GB on a multi-session pnpm monorepo). Root cause: `Store::write_doc` went
  through the schema-only skip in `write_blob`, so when the serve boot's Deferred pass had written
  a doc blob vectorless (`embedding_dim: 0`), the follow-up Inline pass re-extracted and re-embedded
  the document — and then silently threw the embedded result away. The blob stayed vectorless
  forever, and every entry-less encounter of the same content (NoCache rename remove+create pairs
  from #43, `git worktree` churn, fresh worktree-rooted workspaces) re-ran ONNX embedding from
  scratch. Doc blobs now overwrite on upgrade (the same fix the code-chunk tier already had), and
  `DocEntry` records embed state so already-poisoned caches heal with exactly one persisted
  re-embed pass on the next scan — no wipe needed. The same skip also meant `search_documents`
  vectors never landed in LanceDB, so document vector search now actually works for repo docs.
- **`basemind comms stop` and SIGTERM now interrupt a mid-scan daemon** (#44). The drain trips a
  cooperative per-file cancellation token threaded through the scanner, and the runtime teardown is
  bounded at 15 s instead of waiting forever on an in-flight rescan — previously only SIGKILL could
  end a scanning daemon, which fed the respawn loop. A cancelled pass commits completed files
  (including their LanceDB rows), never runs the stale purge, and surfaces as `rescan_cancelled`
  rather than a completed rescan.
- **Queued identical full rescans coalesce.** N sessions forwarding full rescans of the same
  workspace serialized on the store lock and each re-walked the whole tree; a request that waited
  behind an identical-or-stronger full scan is now served that scan's stats.
- Unreferenced blobs younger than 6 h survive the daemon's hourly GC sweep, protecting the
  Deferred→Inline and rename windows where content-addressed doc blobs are legitimately entry-less.

### Changed

- Dependencies: tree-sitter-language-pack 1.13.2 → 1.13.3, xberg rc.30 → rc.33, lockfile refresh
  (arrow stays on 58.x — lancedb 0.31 pins arrow 58).

### Added

- **`basemind agent` — a terminal coding agent over the code map.** A model-agnostic
  (bring-your-own-key) ratatui TUI that runs a turn-loop against basemind's in-process code-map and
  git tools. Launch it with `basemind agent [prompt]` (forwards `--continue` / `--resume` / the
  positional prompt); the `basemind-tui` binary now ships alongside `basemind` in every release
  archive, and `basemind agent` re-execs it.
- **Multi-agent room.** Agents sharing a repo can join a room over the comms broker: a roster of
  peers in the status area, `/post [subject:] <text>` to broadcast, `/roster` and `/leave` commands,
  incoming peer messages plus join/leave notices in the transcript, an unread cue, and model-facing
  `room:*` tools. The live broker room is built behind the `room` feature; its incoming stream
  reconnects with backoff, refreshes the roster on a cadence, and dedups by message id.

## [0.22.3] — 2026-07-21

### Fixed

- **The file watcher no longer leaks memory or storms the CPU on large, churny trees.** On
  macOS/Windows the `notify-debouncer-full` debouncer defaulted to its `FileIdMap` cache, which
  recursively walks and `stat`s the entire watched subtree — following symlinks, at watch time and
  again on every directory create/rename — into an unbounded map. On a pnpm monorepo (a
  `node_modules` symlink farm plus in-repo git worktrees) that walk amplified without bound: RSS
  climbed 138 MB → 6.85 GB in ~3 minutes at 45–95 % CPU (#43, re-file of #41; CPU counterpart #33).
  Both the tree watcher (`basemind watch` / `serve`) and the internal view watcher now use
  `NoCache` — the same cache Linux already defaulted to — so the debouncer never walks the tree.
  basemind's own gitignore-aware `IndexFilter` still selects what a change rescans, and renames
  degrade to remove-old + create-new, which the incremental scanner already handles.

## [0.22.2] — 2026-07-20

### Fixed

- **Codex downstream workspaces now start the latest complete release reliably.** Codex resolved the
  old relative launcher inside its installed plugin cache, indexing the plugin instead of the user's
  workspace. Its plugin now locates a bundled bootstrap, resolves the latest published GitHub tag,
  and uses the serialized, checksummed launcher; simultaneous first starts cannot corrupt a shared
  `npx` cache.
- **MCP tools stay available across a release window.** The plugin launcher (`mcp-launch.sh`)
  hard-pinned to the plugin's exact version and aborted if that release's platform asset 404'd — so
  in the window between the plugin bumping to a new version and that release finishing its
  per-platform binary uploads, the session had no MCP server at all. On a missing pinned asset the
  launcher now re-execs pinned to the newest *published* GitHub release: GitHub exposes only
  finalized, non-draft releases at `/releases/latest`, and the publish workflow promotes a release
  only after every platform asset + checksums exist, so the fallback target is always complete and
  safe to run — even from an empty cache. A same-schema-minor cached binary remains the offline last
  resort. The launcher also now discovers the plugin manifest under any harness mirror
  (`.claude-plugin` / `.codex-plugin` / `.cursor-plugin`), so one launcher serves every coding agent.
- **The publish workflow heals partial releases and never publishes out of order.** A rerun that
  found any single release asset previously assumed the release was complete and skipped the build
  matrix, so a release missing a platform binary could never recover. It now requires the full
  archive + checksums set before treating a release as published, so a rerun rebuilds only the
  missing platforms. Publish concurrency keys on the resolved tag (a tag push and a manual dispatch
  for the same tag no longer race) and never cancels a publish mid-flight, and the crates.io publish
  waits for the release to finalize like the npm/PyPI publishes.
- **Version updates no longer leak basemind processes.** On update, the prior version's `serve` and
  comms daemon kept running beside the new generation, so the machine never converged on a single
  daemon. The launcher now terminates every basemind process (serve and comms/write/shell daemons —
  all the same binary) belonging to a different cached version before exec'ing the resolved binary,
  and prunes leftover install lock/staging dirs. Unix best-effort; never blocks or fails the launch.
  No API or on-disk-format change (blob/index schema unchanged).

## [0.22.1] — 2026-07-20

### Fixed

- **basemind is publishable to crates.io again.** The 0.22.0 stack-graphs vendoring added four
  in-tree forks as versionless path dependencies with `publish = false`, which made `cargo publish`
  reject basemind ("all dependencies must have a version requirement specified"). The forks are
  renamed to owned, published crates — `basemind-stack-graphs`, `basemind-lsp-positions`,
  `basemind-tree-sitter-graph`, `basemind-tree-sitter-stack-graphs` — and referenced by version, so
  `cargo install basemind` works again. The publish workflow now releases the sub-crates in
  dependency order before basemind. No API or on-disk-format change (blob/index schema unchanged).

## [0.22.0] — 2026-07-20

> **Index rebuild on upgrade.** This release bumps the index/blob/comms schema (21 → 22), so every
> existing `.basemind/` (legacy, per-repo) and the new global cache wipe and rebuild from source on
> the next `basemind scan`. The cache location itself also moves: state no longer lives under a
> repo's `.basemind/` — it lives in a single global, content-addressed store under the XDG data
> directory (see below). This is the intended migration path; nothing you need to do beyond letting
> the next scan run.

### Added

- **Machine-wide daemon — concurrent sessions now read AND write.** A single background daemon per
  machine is the sole fjall writer for the index. Previously, one `serve` process per repo held the
  repo's fjall write-lock, so a second session on the same repo silently fell back to a read-only,
  increasingly stale view. `serve` now opens its store read-only (blobs only) and forwards writes
  (scan / rescan) to the daemon over a Unix domain socket (named pipe on Windows); the daemon starts
  on demand and is shared by every session on the machine. N sessions on one repo now all read and
  write without downgrade.
- **Global XDG cache + `BASEMIND_DATA_HOME`.** Index data (content-addressed blobs, per-workspace
  views, LanceDB) moves out of the repo and into `~/.local/share/basemind/` (overridable with
  `BASEMIND_DATA_HOME`), keyed by workspace. The blob store is now a single global, content-addressed
  cache that dedupes identical files across **every** repo and worktree on the machine, not just
  within one repo. `.basemind/` is no longer created in repos — the gitignore dance from `init` is
  gone.
- **Registry + worktree coordination tools.** The daemon keeps a cheap, always-on registry of
  repos, worktrees, and branches built from git plumbing, exposed as new MCP tools: `workspaces`
  (known repos on the machine), `worktrees` (linked worktrees for a repo), `branches` (branches per
  worktree), and `worktree_claim` / `worktree_release` — an advisory claim so multiple agent sessions
  don't collide working the same worktree at once.
- **`find_files`** — fuzzy, fzf/fd-style filename/path search across indexed files, ranked by match
  score. Complements the existing substring `list_files`.
- **`basemind statusline`** — a CLI subcommand that queries the daemon for the workspaces currently
  active and prints a compact status line; prints nothing (idle) when no daemon is running.
- **Thread comms replace room comms.** Agent coordination now uses threads instead of rooms: each
  thread is addressed by at least two of subject / path-glob / members, discovered by scope (member,
  cwd path-match, or subject filter — never global), with idle threads auto-archiving. See
  `thread_start` / `thread_post` / `thread_list` / `inbox_read` / `agent_list` in the MCP tool table.
- **Precise, scope- and import-aware name resolution for Python and Java** (feature
  `code-intel-stack`, included in `full` and the release binary). Until now only JavaScript/TypeScript
  resolved precisely (via oxc); every other language fell back to heuristic name matching, so
  `find_references` / `find_callers` / `goto_definition` could not tell a shadowed local from an
  import. basemind now runs GitHub stack-graphs-style `.tsg` name-binding rules — executed by an
  in-tree, tree-sitter-0.26 fork of the `tree-sitter-graph` / `tree-sitter-stack-graphs` engines
  (maintained under `crates/`) — to build a per-file scope/definition graph and resolve references to
  their true definitions. Delivers **intra-file** precision (shadowing, per-function/method parameter
  scope, Python comprehension bindings, Java field-vs-local) and **cross-file** resolution for Python:
  dotted and relative imports resolve to their target module, and each call site of an imported name
  resolves *through* the import to the exported definition (reporting `resolved: true`), rather than
  matching by name. No new MCP tools or query surface — existing navigation just gets precise for
  these languages. Known limitations this iteration: Java cross-file resolution of *member* calls on
  an imported type (`Foo.greet()`) is not yet resolved (the imported type itself is), and a variable
  reused after a same-name Python comprehension may bind to the comprehension's own variable.
- **`[code_intel] precise_resolution` config toggle** (default `true`). Set it to `false` in
  `basemind.toml` to skip the precise engines (oxc + stack-graphs) and fall back to fast tree-sitter
  `locals` scope binding for every language. Applies to files (re)scanned after the change.
- **`[resources]` governance config.** A new table bounds basemind's memory and CPU footprint for
  fitting a large workspace on a modest host: `embed_threads` (caps the ONNX embedding pool;
  supersedes the deprecated `[documents].embed_max_threads` alias), `embed_batch_size` (default 32),
  `scan_threads` (caps the scanner rayon pool), `document_models` (`full` | `code_only` | `none` —
  narrows the document tier for a code-centric workspace), and `max_footprint_mb`, enforced by a
  best-effort backpressure gate that samples the process `phys_footprint` (mach `TASK_VM_INFO`, which
  counts compressed and swapped-out pages) at the document-extraction and embedding admit points and
  parks over-ceiling workers on a bounded backoff — never failing a scan. The schema change is
  additive (no `RELEASE_MINOR`/blob bump).
- **`inbox_wait`** — a blocking comms tool that returns the moment a peer posts to a joined thread (or
  on timeout), replacing the `inbox_read`/`thread_list` polling loop. Backed by the broker's existing
  push fan-out via a membership-routed `SubscribeInbox` sink over an ephemeral connection, so a long
  wait can't head-of-line-block other comms calls.
- **Per-call latency in every tool response.** Latency-relevant tools now return `elapsed_us` (tool
  body only — index/store lookups, git walks, ranking, response construction; excludes transport and
  serialization) and the CLI additionally reports `startup_us`, so query cost is observable and
  distinct from process startup. Microseconds because the warm code-map path is routinely
  sub-millisecond.

### Changed

- **Cache moved out of the repo.** Everything the scanner and index used to write under a repo's
  `.basemind/` now lives under the global XDG cache, keyed by workspace — a repo checkout no longer
  gains any basemind-owned files or directories.
- **Blobs are global and deduped across repos.** The content-addressed blob store used to be
  per-repo; it is now one machine-wide store, so identical file content scanned from different repos
  or worktrees is extracted and stored once.
- **`find_callers` reports a complete result; precision is now additive.** The name scan (the same
  sound scan `find_references` runs) is the floor and defines `total`/`hits`; import resolution can
  only refine it, surfacing precision as a per-hit `resolved` flag plus a `resolved_total` lower
  bound. Previously resolution ran first and returned early, so a partial resolution could report a
  confident, complete-looking, wrong subset (see Fixed).
- **1000-line module cap is now enforced.** `tests/max_lines.rs` walks `src/**/*.rs` and fails
  `cargo test` on any file over the cap — the documented `rust-max-lines` poly check never actually
  existed (poly's custom rules are ast-grep matches and cannot count lines). Several files that had
  drifted past the cap were split along real seams into sibling modules, behaviour-preserving by
  construction (the `keys.rs` split keeps `symbol_kind_byte`'s persisted `u8` ordinals byte-identical).
- **Dependency upgrades.** xberg `1.0.0-rc.30`, crawlberg `1.0.7`, tree-sitter-language-pack `1.13.1`,
  and the latest compatible versions across the tree; `arrow-array`/`arrow-schema` held at 58 in
  lock-step with lancedb 0.31. stack-graphs is vendored under `crates/` with two upstream panics
  fixed.

### Removed

- **Per-repo `.basemind/` writes.** basemind no longer creates or gitignores a `.basemind/` directory
  inside a repo; `basemind init` no longer adds it to `.gitignore`.
- **Room comms API.** `room_list` / `room_join` / `room_leave` / `room_post` / `room_history` and the
  CLI `basemind comms rooms` / `join` / `leave` / `room-create` / `post` / `history` surface are
  replaced by thread comms (see Added above).

### Fixed

- **`basemind init` no longer scaffolds into a parent directory.** `init` resolved its target with
  the same ancestor-`.basemind/` walk the other commands use to *attach* to an existing index, so
  running it inside a repo whose parent already had a `.basemind/` wrote `basemind.toml`, `.gitignore`,
  and the rules block into the parent instead of the current repo. `init` now anchors to the closest
  enclosing git repository (falling back to the working directory when not in a repo), so it always
  scaffolds the project you run it in.
- **Root discovery no longer climbs across a nested subrepo boundary.** `discover_root_with_basemind`
  walked up looking for an ancestor `.basemind/` without a ceiling, so running from inside a nested
  git subrepo (checked out under a polyrepo that has its own root `.basemind/`) wrongly attached to
  the parent polyrepo's index. The upward walk is now bounded by the closest enclosing git repository
  — the subrepo's own root is the ceiling. A non-git monorepo (no git boundary anywhere) still walks
  freely to an ancestor `.basemind/`.
- Fixed Codex plugin MCP startup by using the required root `.mcp.json` shape and resolving the
  launcher relative to the installed plugin instead of an unsupported `${PLUGIN_ROOT}` placeholder.
- **`find_callers` reported 2 callers where there were 172.** It ran import resolution first and
  returned early on any hit, so the name scan ran only as a fallback when resolution found nothing.
  When resolution saw 2 of 172 real call sites it reported `total: 2` with no truncation flag — a
  confident, complete-looking, wrong answer an agent would act on. The name scan is now the floor and
  resolution only annotates it (a Python case that returned 7 of ~4,000 now returns the full set).
- **`workspace_grep` returned a fast, confident, wrong zero.** `scan_cap` bounded *files visited*, so
  a default grep over a 68k-file workspace searched ~2.9% of it in arbitrary path order and stopped —
  and a rare identifier, which is precisely what one greps for, does not live in the first 2.9%.
  `limit` now caps *hits*, the whole corpus is scanned (rayon-parallel with a memmem literal
  prefilter), and `total_matches`/`total_files_matched` are exact. A latent cursor bug that dropped a
  file's remaining matches on a mid-file `limit` boundary is fixed by packing `(candidate, hit
  ordinal)` into the cursor.
- **The git-history index was built and read from nowhere on a `--features full` binary.** Fjall's
  directory lock is exclusive even for a read-only open, so serve (which opens read-only under
  `comms`) could not hold the history DB; the handle was `None`, serve never built the index, and
  every git tool silently live-walked forever. The daemon now owns both the build and the reads
  (`GitHistoryIndex` is a `Local | Remote` backend enum), the per-repo build is serialized so N
  sessions cause one walk, and the one-shot CLI routes its history reads through the daemon instead of
  climbing a retry ladder and live-walking. Measured, daemon up: CLI startup ~1.56 s → ~5 ms, a git
  query ~1.59 s → ~10 ms.
- **`search_documents` could never see anything the web lane ingested.** The scanner writes documents
  under the repo scope while `web_scrape`/`web_crawl` write under `web:<host>`, and the search
  hard-filtered on the repo scope with no way to name the other namespace — so every scraped page was
  written, embedded, and then permanently invisible. `scope` is now a search parameter (defaulting to
  the repo scope), and the ingest tools echo the scope they wrote under, so the pair composes.
- **A daemon `--features full` build embedded nothing it scanned.** The forwarded initial scan ran a
  `Deferred` (code map + BM25 keyword) pass only; the vector-fill follow-up the non-daemon path spawns
  was never forwarded, so LanceDB stayed empty for every repository document and code chunk
  (`web_scrape` worked because it embeds inline). The daemon can now run an `Inline` embed pass, and
  `serve` forwards a detached vector-fill follow-up after the fast initial scan — off the boot
  handshake, so startup never blocks on ONNX. Live watcher rescans and the `rescan` tool forward it
  too.
- **A failure in an optional lane destroyed the code map.** The scanner flushed the code map *after*
  the optional lanes (embeddings, documents, code-intel), so any lane that failed took the whole map
  with it — leaving gigabytes of blobs behind a `file_count` of 0 and a full re-scan on every launch.
  The map is now persisted before the optional lanes run, each lane runs under `catch_unwind`, and a
  per-file panic in the resolve pass costs only that file's edges.
- **The machine daemon's footprint climbed to ~13 GB at monorepo scale and never released.** The
  `Inline` embed pass accumulated every file's embedding rows (a 768-dim vector + chunk text per
  chunk) for the whole corpus in RAM before one LanceDB write. It now streams per file, so peak embed
  RAM is one file independent of corpus size (a `phys_footprint`-ratchet regression test pins it flat
  across passes). A related serve-side RSS sawtooth — a whole-map rebuild on every no-op rescan — is
  gone: an `(path, content-hash)` fingerprint proves the blobs are byte-identical and the resident map
  is reused.
- **A panicking request handler pinned its daemon forever.** The idle reaper counts live links, and
  the refcount was released by a plain statement after the accept loop, which an unwind walks past —
  so one panicking handler left the count above zero and that daemon never reaped, stacking an
  immortal process on every run. The refcount is now an RAII `LinkGuard` released by `Drop` (also
  closing the accept/exit race), and idle detection consults in-flight work, not link count alone.
- **Every CLI invocation rebuilt the whole in-RAM map cache** — ~3–5 s flat, up to 11 s under load,
  even for `repo-info`, which never reads the map. The one-shot CLI now builds it lazily at the cache
  barrier, only for tools that scan the corpus (`repo-info` ~534 µs vs `search_symbols` ~1 s). En
  route this fixed `complete()` reading the map without taking the barrier every other cache-reading
  tool takes — an intermittent flake that would have become a permanent empty under lazy-cache.
- **Orphaned workspace caches pinned blobs forever.** Nothing reaped the per-workspace cache dir of a
  deleted repo, so the global blob store could only grow (48 workspace dirs for 2 live repos). A
  marker-based reaper now records the canonical root and reclaims dead workspaces before the blob
  sweep — conservatively keeping anything it can't prove dead (held lock, or a root whose parent is
  also missing, e.g. an unmounted volume).
- **Two agents sharing one identity shared one inbox.** The CLI's persisted-id tier read the
  pre-0.22 in-repo path that no longer exists, so every CLI call collapsed onto a shared
  `"basemind-cli"` constant (and `serve` onto `"anon"`). All four resolvers are consolidated to a
  single chain — env, config, then a generated id persisted in the correct per-workspace cache dir,
  then a process-unique id — never a shared constant, and honouring `comms.agent_id`.
- **A thread scoped to `<dir>/**` was invisible to an agent whose cwd *is* `<dir>`.** globset's `**`
  requires a component after the slash, so the recursive form never matched its own base — the most
  common cwd. The stripped base is retried, covering the root without widening the glob.
- **Cross-file `goto_definition` silently dropped every tsconfig-aliased import.** oxc resolves a
  tsconfig `paths` alias against a *canonicalized* `baseUrl`, so on a symlinked root the resolved
  target's prefix didn't match and the cross-file edge was discarded. The resolver now discovers the
  governing tsconfig per importing file (what `tsc` applies) and `to_repo_relative` retries against
  the canonical root — a no-op on non-symlinked roots.
- **A daemon-writer rescan didn't invalidate in-flight `search_symbols` cursors on unchanged
  content.** The `cache_generation` bump was nested inside the map rebuild, which the fingerprint
  optimization skips on a no-op rescan, so a stale cursor read as valid — contradicting the documented
  `invalidated on rescan` contract. The bump is now unconditional.
- **Telemetry recorded call latency in milliseconds, so every sub-millisecond tool read as `0`.** The
  sink now stamps microseconds (legacy `elapsed_ms` rows are rescaled on read), matching the
  response-level `elapsed_us` reading.
- **Fjall ran on its 32 MiB default block cache against a multi-GB index.** The cache is now sized at
  20% of the on-disk index, clamped to [32 MiB, 256 MiB] (`BASEMIND_INDEX_CACHE_BYTES` overrides);
  `find_references("get")` on django went 907 µs → 285 µs for +15.9 MB RSS.
- **`web_crawl` followed links off the seed host.** crawlberg's `stay_on_domain` defaults off and the
  engine builder never set it, so a crawl wandered onto every domain it linked to. On-host scope is
  forced now (subdomains still excluded).
- **`web_map` returned every URL a site has, uncapped** — 482k URLs on docs.rs drove peak RSS into the
  multi-GB range and OOM-killed the process. `limit` (default 100, max 1000) now caps the response,
  and crawlberg 1.0.7's `map_limit` bounds the sitemap fetch itself, holding peak to roughly the cap
  plus one child sitemap; `total_urls` becomes a floor with `truncated` set.
- **`cache_stats` no longer flakes when a `*.tmp` file vanishes mid size-walk**, and an fsevents
  watcher test race (an event landing before stream registration is lost forever) is closed by
  re-writing until the watcher reports the path.

## [0.21.1] — 2026-07-11

### Changed

- **Extracted the Hermes Agent plugin into a standalone `basemind-hermes-plugin` PyPI package.** The
  helper skills, slash commands, `plugin.yaml`, and the `hermes_agent.plugins` entry point now live in
  the pure-Python `pip-package-hermes/` (import package `basemind_hermes_plugin`), which ships no
  binary and shells out to the `basemind` on `PATH`. The `basemind` pip package is now a pure
  binary-wrapper (no plugin, no entry point). Install the binary via Homebrew/npm/cargo/release and
  `pip install basemind-hermes-plugin` for the plugin, then `hermes plugins enable basemind`. The
  entry-point name stays `basemind`, so the enable command is unchanged.

### Fixed

- **Binary download no longer crashes when `/tmp` and `$HOME` are on different filesystems (#38).**
  `ensure_binary` staged the extracted binary in the system temp dir and then atomically renamed it
  into `~/.cache/basemind`, which raised `OSError: [Errno 18] Invalid cross-device link` on hosts
  where those are separate mounts. Staging now happens under the cache root so the rename stays on one
  device.

### Dependencies

- Bumped `xberg` to `1.0.0-rc.21` and ran `cargo upgrade --incompatible`: `crawlberg` 1.0.5,
  `jsonschema` 0.47, `rmcp` 2.2, `regex` 1.13, `lru` 0.18.1, `tree-sitter-language-pack` 1.12.5.
  `arrow-array`/`arrow-schema` held at 58 to stay in lockstep with `lancedb` 0.31's transitive Arrow.
  Blob + index format unchanged (patch release).

## [0.21.0] — 2026-07-10

Minor bump: `RELEASE_MINOR` advances 20 → 21, so the `.basemind/` index + blob cache is wiped and
rebuilt from source on the next `basemind scan` (expected — the msgpack blob store is content-
addressed and rebuilds losslessly).

### Added

- **`basemind init` onboarding + `/bm-init` slash command.** A re-runnable setup flow that scaffolds
  the root `basemind.toml`, gitignores `.basemind/`, and injects an idempotent, clearly-delimited
  rules block into the repo's agent guidance (`CLAUDE.md` / `AGENTS.md`, or an `.ai-rulez` rule file
  when that's the source of truth) teaching agents to prefer basemind over grep/read/git per
  capability. Interactive by default; `--yes` plus `--with`/`--without`, `--rules-target`,
  `--no-rules`, and `--print` (dry-run) drive it non-interactively. The block is spliced between
  stable `<!-- BEGIN/END basemind -->` markers so re-running never duplicates it and never touches
  content outside the markers.
- **Monorepo-aware root resolution.** basemind now walks up from the working directory to the nearest
  ancestor containing `.basemind/` (the way git finds `.git/`), so running an agent from a subfolder
  of a monorepo attaches to the root index instead of finding nothing. `.basemind` discovery takes
  precedence over git-root discovery; `--root` still overrides.
- **Index lifecycle signalling on MCP responses + `status`.** Read tools attach a `notice`
  (`warming_up` / `building_index` / `rescanning`) with an actionable retry hint so an agent never
  mistakes a mid-warmup or mid-scan result for "no matches". `status` gains `warming` + `warm_ms`.

### Fixed

- **`serve` answers the MCP handshake before warming the code map.** The in-RAM preload (a rayon
  `par_iter` over every L1/L2 blob) ran synchronously before `.serve(transport)`, so on a loaded
  machine — several sessions' scans saturating the shared rayon pool — it could queue for minutes,
  blow the client's plugin-MCP startup window, and the tools would never register. The preload now
  runs in a background task; `initialize`/`tools/list` answer immediately regardless of tree size or
  contention. Cache-reading tools await a bounded (15s) ready signal so a query issued during warmup
  still returns complete data rather than an empty snapshot. `diff_outline`, `blame_symbol`,
  `memory_audit`, and `proposal_accept` — which read the map directly — now await warm too, closing a
  window where they returned a wrong diff, a spurious "not indexed" error, or a false stale verdict.
- **`basemind init` refuses to splice into a file with malformed markers.** A stray or unpaired
  `BEGIN`/`END` marker (a hand-edit or bad merge) could make a re-run replace everything between an
  orphaned marker and an unrelated pair, silently deleting the user's own content. init now bails on
  any malformed marker state instead of guessing, and absorbs a CRLF trailing newline so a
  Windows-line-ending rules file converges to a byte-stable fixpoint (no phantom `--print` diff).

## [0.20.0] — 2026-07-08

Minor bump: `RELEASE_MINOR` advances 19 → 20, so the `.basemind/` index + blob cache is wiped and
rebuilt from source on the next `basemind scan` (expected — the msgpack blob store is content-
addressed and rebuilds losslessly).

### Added

- **Root config at `<root>/basemind.toml` (committable).** Config moved out of the wipe-on-schema-bump
  `.basemind/` cache to the repo root so it survives cache wipes and can be committed. The legacy
  `.basemind/basemind.toml` is still read as a fallback; the root file wins when both exist.
  `basemind init` now scaffolds a fully-documented root config (every value is the built-in default)
  and adds `.basemind/` to `.gitignore`. It refuses to overwrite an existing config and refuses to
  shadow a legacy in-cache config rather than silently stranding your settings.
- **`scan.follow_symlinks`** (default `false`). Symlinks often escape the repo (e.g. Bazel's
  `bazel-*` convenience symlinks); the walker no longer follows them unless you opt in. `extra_roots`
  always follow symlinks regardless.
- **`embed_exclude` glob lists on `code_search` and `documents`.** Keep a file chunked + keyword-
  indexed while skipping its embedding — useful for large generated or vendored files.
- **`documents.extract_archives`** (default `false`). Opt into xberg's recursive archive extractor
  for `.zip/.tar/.jar/…`; off by default so one archive can't explode into thousands of embeds. True
  binaries (`.so/.wasm/.exe/…`) are always rejected regardless of this flag.

### Changed

- **Code embeddings are now OFF by default (`code_search.embed = false`).** A general-English model
  embeds *code* weakly, and the BM25 keyword lane already serves the NL→symbol query over the same
  text, so code embedding isn't worth the ONNX download + ORT pass + vector store. Code is still
  chunked and keyword-indexed; documents and images still embed. Existing code-search users who rely
  on vector search over code must set `code_search.embed = true` to keep it.
- **Always-on exclude floor.** A hard floor of near-universal build-artifact / dependency-cache / VCS
  / editor directories (`node_modules`, `target`, `dist`, `build`, `out`, `.venv`, `__pycache__`,
  `.pytest_cache`, `bazel-*`, `.git`, `.basemind`, `.idea`, `.DS_Store`, …) is applied on top of
  `scan.exclude` — narrowing the user list can add to it but never drops a floor entry.

### Fixed

- **Worktrees reuse the shared cache instead of redoing work.** Scanning a fresh git worktree no
  longer re-parses files whose extraction already lives in the shared (main-worktree) blob store —
  the scanner deserializes the cached L1/L2 frame instead of re-running tree-sitter. The git-history
  index is likewise shared: it is derived entirely from the shared `.git` object database, so a
  linked worktree resolves it to the main worktree's `.basemind` (mirroring the blob store) rather
  than rebuilding its own. On a large monorepo a cold worktree scan drops from ~181s to ~25s (~7×)
  with ~2.6× less peak memory. The scan summary gains a `reused` counter reporting cache hits.
- **Re-embed on same-dim preset model swaps.** The per-file embedding cache keyed on vector dimension
  only, but `balanced` and `multilingual` both produce 768-dim vectors — so switching between them
  reused the old model's cached vectors. Cache reuse now also requires the embedding model/preset to
  match (both the code and document tiers), so a preset change cleanly drops the old vectors and
  re-embeds. `CodeChunkBlob` gains a blob-compatible `embedding_model` field (no schema bump).
- **Hugging Face model downloads no longer hang forever behind a firewall.** Bumped xberg to
  `1.0.0-rc.16`, which adds connect/read timeouts to the HF model fetch so a blocked or silently
  dropped connection (corporate firewall, offline host) fails fast instead of stalling the embedding
  / OCR / NER model download indefinitely.

## [0.19.2] — 2026-07-07

### Added

- **Intel macOS (`x86_64-apple-darwin`) prebuilt binary.** macOS is no longer Apple-Silicon-only —
  the release now ships a native Intel binary with full feature parity. `ort` has no static
  x86_64-apple-darwin ONNX Runtime (Microsoft dropped `onnxruntime-osx-x86_64` after 1.23), so the
  Intel leg builds `--features full,ort-dynamic` (`ort/load-dynamic`) and `package-release.sh`
  vendors `libonnxruntime.dylib` + its closure next to the binary with `@loader_path` install-names;
  ort resolves it relative to the executable, so ONNX loads with zero end-user setup. `brew install`
  now works on Intel Macs too. The release completeness gate expects five archives.

### Fixed

- **Apple Silicon under a Rosetta x86_64 shell now installs.** Rosetta masks `hw.optional.arm64`, so
  the previous probe fell through to the Intel branch and aborted with "Intel macOS not supported" —
  even though the machine is Apple Silicon. All three installers (shell launcher, npm `install.js`,
  pip `downloader.py`) now also probe `sysctl.proc_translated` (set only under Rosetta, which exists
  only on Apple Silicon) and hand such shells the native arm64 binary. A genuine Intel Mac gets the
  new x86_64 binary.
- **`/bm-statusline` now selects the correct (highest) version.** The persisted resolver ranked
  cached `statusline.sh` copies by mtime (`ls -dt`), so an older version dir touched more recently
  won — showing a stale bar. It now resolves the marketplace clone first, else the highest-versioned
  cache copy via `sort -V`, and never renders blank (a one-line hint replaces an empty bar).

### Changed

- **Installers prune stale cached binaries.** The launcher and pip downloader accrued a directory per
  release under `~/.cache/basemind/(bin/)<version>` (users saw six). Each now removes old per-version
  dirs once the current version is confirmed present, so only the running version is kept.
- **Dependencies refreshed** to latest (`cargo upgrade --incompatible`): `xberg` → `1.0.0-rc.11`,
  `fjall` → `3.1.6`, plus transitive lockfile updates. `arrow-array`/`arrow-schema` are intentionally
  held at `58` until lancedb's transitive arrow moves (they must stay in lockstep).

## [0.19.1] — 2026-07-07

### Fixed

- **`scan.extra_roots` now works on Windows.** Files outside the repo root are keyed by their
  absolute path; the Windows drive-prefixed form (`C:/…`) is now recognized on the consuming side —
  previously only POSIX `/…` keys resolved, so external files were unqueryable there. Unix keys are
  unchanged (index format untouched).

### Security

- Bump `crossbeam-epoch` to 0.9.20 for RUSTSEC-2026-0204 (invalid pointer dereference in the
  `fmt::Pointer` impl for `Atomic`/`Shared`), pulled transitively via rayon.

## [0.19.0] — 2026-07-07

> Minor release — the index + blob schema version bumps (`RELEASE_MINOR` 18 → 19), so `.basemind/`
> is wiped and rebuilt on the next scan.

### Added

- **Hermes Agent plugin.** basemind now integrates with [Hermes Agent](https://hermes-agent.nousresearch.com):
  `pip install basemind` exposes a Hermes plugin (discovered via the `hermes_agent.plugins` entry
  point) that bundles the helper skills, slash commands, and agent-comms notifications. Tools are
  wired through Hermes's MCP config (`mcp_servers.basemind` in `~/.hermes/config.yaml`) — a Hermes
  plugin cannot declare an MCP server. See the README "Hermes" install section. Closes #36.

### Changed

- **Claude status line survives version bumps.** `/bm-statusline` now writes a version-independent
  resolver as the `statusLine` command (re-resolving the newest installed `statusline.sh` at each
  render) instead of a version-pinned path that broke on update. The bar also shows the running
  basemind version (`v<version>`, full/compact tiers; opt out with `BASEMIND_STATUSLINE_VERSION=0`).

### Performance

Wide hot-path allocation + algorithmic sweep across the scanner, extraction, git, and MCP query
paths. All changes are internal — no behavior, response-shape, or on-disk-format change, and the
determinism assertions are unchanged.

- **`call_graph`** (`bfs_callees`) precomputes a name→sites map once instead of re-scanning every
  indexed symbol for each discovered callee — O(max_nodes × symbols) → O(symbols + max_nodes).
- **`architecture_map`**: `callee_counts` no longer allocates a `String` on every one of up to 4M
  scanned call sites (allocates only on first sight of a name); the symbol-tier fan-out set skips the
  allocation for duplicate callees; PageRank reuses a single accumulator buffer (20 allocations → 1).
  Output stays byte-identical.
- **Tree-sitter `QueryCursor`** is now pooled per thread alongside the parser pool — ~3 fewer
  allocations per scanned file.
- **git**: `diff_file` no longer clones the (potentially multi-MB) file buffers just to record
  existence; the `blame` / `log` / `commit-files` disk-cache writers serialize from borrowed slices
  instead of cloning the whole payload; `dependents` drops a `RelPath`↔`PathBuf` round-trip over the
  entire file index.
- **document tier**: `build_doc_rows` moves per-chunk text + embedding vectors instead of cloning
  them; the archive-extension check is an `AHashSet` (O(1)) instead of a 40-element linear scan.
- **paths**: the scanner (`scan_paths`) and watcher skip a `\`→`/` `String` allocation on Unix.
- **BM25 index**: the posting value drops a `.to_vec()` (`lsm_tree::Slice: From<[u8; N]>`) and the
  forward-map value is built in one sized allocation instead of an alloc-plus-realloc.

### Internal

- `for_each_call_in_file` (the dual-backend Fjall / in-RAM call-site scan) now lives once in
  `mcp/helpers_calls.rs`, shared by `architecture_map` and the call-graph helpers; `capture_name` is
  deduplicated into `extract/mod.rs`. Removed a dead `last_emitted_key` in `find_implementations`.

## [0.18.1] — 2026-07-06

> **Patch release — blob + index format unchanged.** `RELEASE_MINOR` stays 18; no `.basemind/`
> rebuild. Query-side only.

### Changed

- **`architecture_map` symbol tier now ranks by specificity-weighted fan-in.** The repo-wide
  symbol tier previously ranked by raw name-based call count, so ubiquitous names (`new`, `from`,
  `default`) dominated — on a real repo the top ~60 hubs were all `new`, which misleads an agent
  routing on the result. Fan-in is now divided by the number of files defining that name
  (`RepoGraph::def_counts`), so a genuine single-definition hub outranks a name that is merely
  everywhere. The raw `fan_in` count is still reported verbatim on each node.

### Fixed

- **`architecture_map` symbol-tier `score` is now monotonic with node order.** `fan_out` was
  computed *after* the knee-cut, so the blended `score` never actually influenced selection or
  ordering and the emitted nodes were not sorted by their own `score`. Selection, knee-cut, and
  `score` now all key off the single specificity-weighted signal, matching the module/file tiers;
  `fan_out` and per-file churn remain reported but no longer pretend to affect the ranking.

## [0.18.0] — 2026-07-06

> **Minor release — cache rebuild.** `RELEASE_MINOR` bumps to 18, so both the Fjall index and the
> content-addressed blob store are wiped and rebuilt from source on the next `basemind scan` (the
> standard minor-release migration). No config or on-wire tool-response change.

### Added

- **`architecture_map` MCP tool — deterministic, LLM-free architecture overview.** Ranks modules
  and symbols by graph centrality + churn and surfaces circular-dependency clusters, all derived
  from real call edges (never hallucinated). A whole-repo adjacency graph is built in memory at
  query time from the cached call sites (Fjall `calls_by_path`, or the in-RAM call cache in
  read-only multi-session mode) — no new Fjall partition, no blob I/O. Metrics are cheap and
  deterministic: fan-in/fan-out degree, fixed-iteration PageRank (damping 0.85, 20 iterations,
  id-ordered accumulation — call-twice byte-identical), and iterative Tarjan SCC for cycle
  clusters. Three tiers via `granularity`: `module` (default; files collapsed to a directory
  prefix at `depth`), `file` (the base graph), and `symbol` (function→function drill-down under a
  `focus`). Results are ranked, Kneedle knee-cut, then bounded by the shared `apply_budget` token
  budgeter; edges are emitted only between surviving nodes. Optional `include_churn` overlay
  reuses the `hot_files` aggregation. Node/edge caps set `truncated` + `truncation_reason` on
  overflow. CLI: `query architecture-map`. New internal `kneedle` knee-detection helper
  (Satopaa 2011, dependency-free).

## [0.17.0] — 2026-07-05

> **Minor release — cache rebuild.** `RELEASE_MINOR` bumps to 17, so both the Fjall index and the
> content-addressed blob store are wiped and rebuilt from source on the next `basemind scan` (the
> standard minor-release migration). No config or on-wire tool-response change.

### Added

- **Semantic code search (Phase 1, vector-only).** New `search_code` MCP tool runs vector KNN over
  source-code chunks and `get_chunk` fetches a chunk's body — the same two-call pointer pattern as
  `search_symbols` → `expand`. Chunks are derived from the cached L1/L2 extraction + source bytes
  (no re-parse): one chunk per symbol span plus module-level gap chunks, oversized spans split with
  overlap. A content-addressed `<hash>.chunk.msgpack` sidecar caches chunks + embeddings so an
  unchanged file skips re-embedding. Vectors land in a new LanceDB `code_chunks` table alongside the
  documents/memory tables (shared embedding preset). CLI: `query search-code` / `query get-chunk`.
  Behind the `code-search` cargo feature (folded into `full`); BM25 keyword + RRF hybrid fusion land
  in later phases. The `code_chunks` table is created lazily on open, so it rides the existing
  minor-release `.basemind/` rebuild — no separate schema bump.
- **Keyword code search (Phase 2, native BM25).** `search_code` gains a `mode` parameter:
  `semantic` (the default vector lane) or `keyword`, a native Okapi BM25 (`k1 = 1.2`, `b = 0.75`)
  lane over each chunk's symbol + signature + doc + body text. Postings live in two new Fjall
  keyspaces (`code_bm25_postings` inverted + `code_bm25_by_path` forward for O(prefix) re-scan
  deletes); term frequency and document length are inlined so scoring a term is a single prefix
  scan, and corpus stats (`N`, `avgdl`) are recomputed once per scan. The keyword lane reads only
  Fjall and the chunk sidecar — no LanceDB and no embedder — so it works even with
  `[code_search] embed = false`. RRF fusion of the two lanes plus an exact symbol lane lands in
  Phase 3.
- **Hybrid code search (Phase 3, RRF fusion + exact lane + rerank).** `search_code` gains a third
  `mode`, `hybrid` — now the **default** — that fuses three lanes via Reciprocal Rank Fusion: the
  vector (semantic) lane, the BM25 keyword lane, and a new **exact symbol lane** that resolves an
  identifier-shaped query against the `symbols_by_name` index to the chunks defining that symbol
  (the scope-aware signal a pure vector+keyword stack can't produce). Fusion joins the lanes on
  `chunk_id` and is score-scale-agnostic, so it blends an L2 distance, a BM25 score, and a symbol
  match order without normalization; it degrades gracefully, dropping any lane that is unavailable
  (e.g. the vector lane without embeddings). An optional cross-encoder **rerank** pass (`rerank:true`
  / `[code_search.reranker]`, off by default) reuses the same xberg reranker as the documents tier.
  No index-schema change — reuses the existing keyspaces and the `code_chunks` table.
- **`search_code` why-matched provenance.** Hybrid hits now carry per-lane provenance:
  `matched_lanes` (which of `exact` / `vector` / `keyword` produced the hit, in fixed lane order)
  plus the 1-based `exact_rank` / `vector_rank` / `keyword_rank` the chunk held in each contributing
  lane. Lets an agent tell an exact-symbol match from a semantic neighbor or a cross-lane agreement
  without a second call. Additive, non-breaking response fields (absent outside hybrid mode).
- **`status` reports boot-scan indexing state.** When `serve` auto-scans an empty index on startup,
  that build cost was invisible and could fold into the first query's latency. `status` now carries
  `indexing: true` while the boot scan runs and `index_build_ms` once it completes — so a client can
  tell "index not ready yet, poll again" from "no matches", separating index-build time from query
  time. Additive fields, absent on the common ready path.
- **Code-intelligence tier: scope- and import-resolved navigation.** A post-scan resolve pass links
  each reference to its actual definition — scope-aware, not a name match. Intra-file resolution runs
  across 80+ languages via tree-sitter `locals` (with vendored queries for Python / TypeScript / TSX
  / Go); JS/TS additionally get cross-file resolution via oxc scope analysis + `oxc_resolver` import
  stitching. New `goto_definition` MCP tool + `query goto-definition` CLI resolve a `path:line:column`
  to its definition, following cross-file import bindings. Behind the `code-intel-js` feature (folded
  into `full`) for the JS/TS engine; the `locals` tier is always on.
- **Document tier reuses the content-addressed cache instead of re-embedding every scan.** The
  documents pipeline now tracks each file in a new `Index.doc_files` map and, before extracting,
  reads back the cached `<hash>.doc.msgpack` blob (which already carries chunks + embeddings): an
  unchanged doc is skipped, and byte-identical content at any other path — or, with the shared cache
  below, any other worktree — reuses the extraction instead of re-running xberg + ONNX. Removed docs
  now have their LanceDB rows and tracking entry pruned. `doc_files` is `#[serde(default)]` — additive,
  no schema bump.
- **Shared blob cache across git worktrees.** Linked worktrees now resolve their content-addressed
  blob directory to the main worktree's `.basemind/blobs` (via gix `common_dir`), so a file's
  extraction + embedding is computed once and shared across every worktree of the clone; views +
  LanceDB stay per-worktree. Auto-GC is disabled while a shared cache exists (a single-worktree sweep
  could reap a sibling's blobs).
- **Bounded embedding threads.** All ONNX embedding now runs on a dedicated rayon pool capped at
  `documents.embed_max_threads` (default `0` = auto `max(2, cores/4)`), and xberg's internal fan-out
  is capped to match. Code-map extraction keeps the full pool; embedding can no longer pin every core
  or balloon RSS on a large monorepo.
- **New config:** `documents.max_chunks_per_document` (default 2000) caps vector rows per document;
  `documents.extension_denylist` extends the built-in archive/binary skip list; `documents.embed_max_threads`
  bounds the embedding pool. `[scan] exclude` gains Bazel defaults (`bazel-out/`, `bazel-bin/`,
  `bazel-testlogs/`, `bazel-*/`), and a large-candidate-count scan logs a warning.
- **mtime+size fast-path on rescan.** An unchanged working-tree file is now confirmed with a single
  `stat()` (size + nanosecond mtime match + sidecars present) instead of a full read + blake3 hash —
  the bulk of the per-file cost on a large-monorepo warm rescan. mtime moved to nanosecond resolution
  so the fast-path is effectively race-free; it's an internal comparison value, so no schema bump.

### Changed

- **Serve boot defers embedding to a background pass.** On `serve` auto-scan of an empty index, the
  first pass now runs in a new `EmbedMode::Deferred` mode: it writes the code-map, the BM25 keyword
  lane, and the content-addressed blobs but **skips** the ONNX embedding, so `outline` /
  `search_symbols` / keyword `search_code` are queryable almost immediately instead of waiting on the
  embed of every file. A detached second pass then re-scans in `EmbedMode::Inline` to fill the vectors
  in (reusing the fast pass' content-addressed caches so only not-yet-embedded content is embedded,
  bounded by `embed.max_threads`), followed by GC. The CLI `basemind scan`, the watcher, and manual
  `rescan` stay `Inline` so scripted/CI use remains deterministic. No schema or config change.
- **Index schema revision — new `refs_by_def` / `refs_by_path` Fjall partitions** back the resolved
  edges; per-file resolution facts persist as content-addressed `<hash>.rref.msgpack` blobs. This
  ships as a minor-release cut: `RELEASE_MINOR` bumps at release time, wiping and rebuilding every
  `.basemind/` index + blob store on the next `basemind scan` (the resolve pass repopulates the new
  partitions). The `code_bm25_postings` / `code_bm25_by_path` partitions for the BM25 keyword lane
  (`INDEX_PARTITION_REVISION` → 4) ride the same index rebuild — the blobs are unchanged, so
  re-embedding is skipped and only the secondary index repopulates.

### Fixed

- **Code-search BM25 postings survive an embedder failure.** With `[code_search] embed = true`, a
  failed embedder load / inference / vector-count mismatch previously returned an error for the whole
  file, dropping its BM25 postings too — so a transient embedding fault silently punched keyword-lane
  holes. The scan now degrades to a keyword-only batch (chunks + BM25, no LanceDB rows, no embedded
  sidecar) and retries embedding on the next scan. BM25 is independent of embeddings and is indexed
  as such.
- **L1 extraction no longer drops (or crashes on) symbols in TSLP-fallback languages whose
  `tags.scm` leads a definition with an auxiliary capture.** The combined-L1 dispatch keyed on the
  match's *first* capture, but adapted upstream tag queries (e.g. Ruby) prepend a `@doc` capture for
  a preceding comment — so a commented `def` classified as `Other`, panicking debug builds
  (`debug_assert!`) and silently dropping the symbol in release. A single such file could abort the
  whole parallel scan, leaving the index empty. Dispatch now scans to the first capture that
  classifies to a known L1 class, recovering those symbols across every fallback grammar.
- **`rescan` rejects path parameters that escape the repository root.** The `rescan` tool took raw
  path strings and joined them straight onto the repo root, so with `scan.respect_gitignore = false`
  a `paths: ["../../etc/passwd"]` traversal would read (and index) a file outside the repo, then be
  retrievable via `workspace_grep`. Request paths now pass through the same `normalize_query_path`
  root-boundary guard as `shell_spawn`; an escaping path is rejected with `invalid_params` instead of
  being scanned. Default `respect_gitignore = true` already blocked it structurally.
- **Archives and binaries are no longer unpacked + embedded during a scan.** `should_extract_document`
  rejected nothing by default, so xberg recursively unpacked `.zip/.tar/.jar/.whl/...` and embedded
  every entry, and binary blobs (`.so/.wasm/.class/...`) were embedded to no purpose — a large,
  pointless cost that pinned ~11 cores and grew `.basemind` to gigabytes on a monorepo. A built-in
  extension + MIME denylist now skips them before any xberg work (images stay allowed for OCR).
- **Document blobs are no longer reaped by the blob GC.** Because docs weren't tracked in the index,
  `collect_referenced_hashes` never marked their `.doc.msgpack` blobs live, so the boot/background GC
  deleted the entire doc cache after every scan. Doc-tier hashes are now unioned into the live set.
- **Full scans of large repos no longer abort with a stack overflow.** Tree-sitter parse trees and the
  msgpack (de)serialization of large extraction blobs can recurse past rayon's small default worker
  stack (~2 MiB) on pathological (deeply-nested / machine-generated) files, hard-aborting the process
  on a first full scan (e.g. a freshly-checked-out git worktree; a warm rescan hid it by skipping
  unchanged files). Extraction and the resolve pass now run on a dedicated rayon pool with a 256 MiB
  per-worker stack (reserved lazily, so it costs nothing on the common shallow path).

## [0.16.0] — 2026-07-02

Minor release: `RELEASE_MINOR` bumps 15 → 16, so every `.basemind/` index + blob store (including
`git-history.fjall/`) is wiped and rebuilt on the next `basemind scan`.

### Added

- **Resource footprint in `cache_stats` (disk + RAM).** The `cache_stats` MCP tool and
  `basemind cache stats` CLI now report a `total_bytes` that reconciles with `du` exactly, a
  per-component breakdown that **includes the `git-history.fjall/` index** (previously uncounted —
  the "1.15 GB reported vs 1.5 GB on disk" gap), an `other_bytes` catch-all so an uncounted
  directory can never silently vanish again, and process memory (`rss_bytes` + `peak_rss_bytes`) via
  a new cross-platform RSS reader (`src/sysres.rs`; mach on macOS, `/proc` + `getrusage` on Linux).
  Disk sizing now uses on-disk block counts, so Fjall's sparse journals are no longer over-reported.
  The Claude statusline gains the on-disk total + serve-process RSS.
- **Full CLI↔MCP parity.** Every MCP tool an agent can call is now reachable from the CLI: new
  `git search` (→ `search_git_history`), `governance audit` (→ `memory_audit`), `query expand`
  (→ `expand`), and a new `shells` group (`spawn`/`send`/`capture`/`kill`/`broadcast`/`list`
  → the `shell_*` tools, `--features shells`; sessions persist via the shared rmux daemon). Three
  CLI-only text primitives gained MCP tools — `delta`, `checkpoint`, `detect_waste`. A new
  `tests/cli_parity.rs` guard enumerates the advertised MCP tool set and fails if any tool lacks a
  CLI counterpart.
- **`scan.extra_roots` — index directories outside the repository root** ([#34]). Point the new
  `[scan] extra_roots = [...]` config knob at absolute paths outside the repo (e.g. a Bazel
  external repo cache, `/private/var/tmp/_bazel_<user>/<hash>/external`) and their files join the
  code map (symbol search, references, callers, outlines) and document search. External files are
  keyed by their **absolute** path — repo files stay repo-relative, so the two namespaces never
  collide and `find_references` resolves across the boundary. Missing/unreadable roots are skipped
  with a warning; a root inside the repo is ignored. Symlinks are followed (Bazel `external/` is
  symlink-heavy). Extra roots are (re-)indexed on a full `basemind scan` only — the live watcher
  does not track them — and git blame short-circuits with a clear error for external (untracked)
  files. The feature itself changes no index/blob format; a re-scan populates external files (and
  this release's `RELEASE_MINOR` bump wipes + rebuilds `.basemind/` anyway).

### Fixed

- **Git author/keyword lookups routed to the wrong tool.** `search_git_history` and `recent_changes`
  descriptions now state their contracts sharply — author/"what did X do"/keyword questions belong
  to the full-depth `search_git_history` (scans every commit reachable from HEAD), not the bounded
  100-commit `recent_changes` window. Precision is verified against real `git` at full branch depth
  by the new `tests/git_parity.rs` harness (opt-in via `BASEMIND_GIT_PARITY_REPO`).
- **Double-run lock UX.** `basemind scan` / `rescan` now pre-detect a live `basemind serve`/`watch`
  holding the store lock (via a non-blocking `probe_writer_lock`) and print an actionable notice
  naming the holder, exiting cleanly instead of colliding with a raw lock error.
- **Misleading `status` latency.** `status` uses a non-blocking read, so a multi-minute rebuild
  holding the write lock is no longer recorded as the call's wall-clock; it returns immediately with
  `rebuild_in_progress: true` when a writer is live.
- **`cache_stats` no longer hard-fails on a stale/unreadable index.** It reports sizes regardless and
  flags `blob_accounting_ok: false` when orphan accounting had to be skipped (e.g. a schema-version
  mismatch), instead of erroring out.
- **`/bm-stats` works offline.** The plugin command is now CLI-first (like `/bm-doctor`), reading
  `basemind cache stats` / `basemind telemetry` when the MCP server is disconnected.

[#34]: https://github.com/Goldziher/basemind/issues/34

## [0.15.1] — 2026-07-02

Patch release: docs / skills / plugin-manifest only. No schema or `RELEASE_MINOR` change, so
existing `.basemind/` caches are untouched on upgrade.

### Added

- **Dedicated per-capability skills.** Split the "use basemind" guidance into three new
  narrowly-triggerable skills alongside the existing umbrella `basemind` and `basemind-comms`:
  `basemind-code-search` (symbol search / outline / references / callers / call graph /
  implementations / `workspace_grep`), `basemind-git-history` (history / blame / diffs / churn /
  `search_git_history`), and `basemind-documents` (semantic + full-text document search, NER /
  keyword filters, and web ingestion). Mirrored into the Codex / Cursor / OpenCode plugin trees by
  `scripts/sync-plugin-skills.sh`.
- **`llms.txt`.** Root-level [llmstxt.org](https://llmstxt.org) index pointing agents at the
  capability skills, docs, and config schema.

## [0.15.0] — 2026-07-02

Minor release: `RELEASE_MINOR` bumps 14 → 15, so every `.basemind/` index + blob store (including
`git-history.fjall/`) is wiped and rebuilt on the next `basemind scan` — which is also how existing
repos populate the new git-history full-text search index.

### Added

- **`search_git_history` — full-text search over commit history.** Search commit **author name +
  email** and **message summary + full body**, tokenized (lowercased, split on non-alphanumeric)
  and matched as an AND. `field` scopes to `author`, `message`, or `all` (default); `limit` +
  `cursor` paginate like the other git-log tools. Backed by two new git-history partitions — a
  `gh_term_to_ords` inverted term→commit-ordinal index (reusing the newest-first delta-varint
  posting encoding) and a `gh_commit_text_by_ord` body store kept out of the head-decode hot path.
  When the git-history index isn't fresh (read-only session / mid-build) the tool degrades to a
  bounded live walk over the recent window, flagged `partial` (author + summary only). To support
  it, commit extraction now captures `author_email` + full `body`, and the stored commit-meta head
  gains an `author_email` field (`sha ‖ time ‖ author ‖ email ‖ summary ‖ files`).

### Fixed

- **Multi-session MCP contention: the writer→read-only downgrade race.** A `basemind serve` that
  rightfully held the `.basemind/.lock` write lock could still come up read-only when a concurrent
  reader transiently held Fjall's single-holder index lock — leaving the repo with *zero* writers
  (no auto-scan / watcher / rescan → a silently stale index). Fixed on both single-holder Fjall
  stores: the writer now **retries** a transient `Locked` on open (it already owns `.basemind/.lock`,
  so the contention always clears), read-only openers **probe `.basemind/.lock` and skip the Fjall
  open** entirely when a writer is live (serving from the concurrently-readable blobs), and the
  schema-version check no longer does a throwaway **double-open** that widened the race window. The
  downgrade log now names the real lock holder instead of always blaming "another serve".

### Changed

- **git-history / index / comms Fjall opens** collapsed to a single `Database` open + inline schema
  read (was: a throwaway peek-open before the real open). `GIT_HISTORY_SCHEMA` bumps `+4 → +5` for
  the FTS commit-meta layout, wiping and rebuilding `.basemind/git-history.fjall/` on the next scan
  — a no-op for released users (the git-history index only became user-visible with this release).

## [0.14.0] — 2026-06-30

Minor release: `RELEASE_MINOR` bumps 13 → 14 (a new `SymbolKind::Heading` variant), so every
`.basemind/` index + blob store is wiped and rebuilt on the next `basemind scan` — which is also how
existing repos pick up Markdown headings.

### Added — Markdown / Obsidian support

Markdown files (already parsed, but previously yielding no symbols) are now a first-class surface:
point basemind at an Obsidian vault or any Markdown notes directory and the code-map tools work over
it the same way they do over source. All of this is on by default — `.md` is scanned like any other
file, headings ship via a `src/queries/markdown.scm` override, and the reference graph is harvested
by a fence-aware byte-scan in `extract/l2.rs` (the tree-sitter block grammar models none of these
inline constructs).

- **Headings → outline.** ATX (`#`…`######`) and setext headings become `Heading` symbols, so
  `outline` and `search_symbols` (optionally `kind: "heading"`) navigate a note's structure.
- **Wikilinks → backlinks.** `[[Note]]`, `[[Note#Heading|alias]]`, and `![[Embed]]` become
  references, so `find_references "Note"` returns the notes linking to it. Targets resolve by note
  name (`#anchor` and `|alias` stripped), matching Obsidian.
- **Standard Markdown links → backlinks.** `[text](Note.md)` / `![alt](img/Diagram.png)` also become
  references, normalized to the same note name (directory, `.md`, `#anchor` stripped, `%20`
  decoded), so wikilink and Markdown-link vaults share one backlink graph. External URLs
  (`http(s)://`, `mailto:`) and bare `#anchor` links are ignored.
- **Tags → graph.** Inline `#tag` / nested `#area/sub` and YAML frontmatter `tags:` (both `[a, b]`
  and `- a` list forms) become references keyed on the leading `#`, so `find_references "#project"`
  lists every note carrying that tag. Tags inside fenced code blocks are skipped.

Not yet mapped (deliberate follow-ups): block references (`^id`, `[[Note#^id]]`), frontmatter
aliases / arbitrary properties, and task/callout items.

## [0.13.0] — 2026-06-30

Minor release: `RELEASE_MINOR` bumps 12 → 13, so every `.basemind/` index + blob store is
wiped and rebuilt on the next `basemind scan` (intentional; no action needed).

### Fixed

- **`serve` watcher no longer pegs multi-core CPU on gitignored / nested-`.basemind` churn
  (#33).** A writer `serve` watching an umbrella repo woke on every filesystem event except those
  under its *own* `.basemind/`, then ran a no-op incremental scan that still re-serialized the
  index (`store.flush`) and rebuilt the **entire** `MapCache` over the whole corpus — on every
  debounced batch. With nested child repos each flushing their own `.basemind/` in a mutual loop
  (or any `node_modules` / build churn) this rebuilt the cache indefinitely. Three fixes:
  - The watcher now drops every event a full scan would never index — including **nested**
    child-repo `.basemind/` writes — before waking a rescan, honoring the **full nested
    `.gitignore` hierarchy** (via the `ignore` crate) plus the configured exclude globs. The
    incremental `scan_paths` is now gitignore-aware too, matching full-scan semantics.
  - `scan_paths` short-circuits when a batch changes nothing indexable — no `store.flush`, no
    work.
  - `scan_and_refresh` updates the `MapCache` **incrementally** for the changed paths (re-reading
    only those blobs) instead of rebuilding the whole corpus, and skips the rebuild and
    cache-generation bump entirely when nothing changed (which no longer needlessly invalidates
    paginating clients' cursors).
  - Note: on macOS, FSEvents is kernel-recursive, so excluded subtrees cannot be un-watched;
    their events are now filtered in-process in microseconds instead of triggering a rebuild.

### Changed

- Extracted the scanner's path-filtering (`Filters`, the new `IndexFilter`) into
  `src/scanner_filter.rs` to keep `scanner.rs` under the 1000-line cap.

## [0.12.2] — 2026-06-29

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 12), so no
`.basemind/` rebuild.

### Changed

- **`tree-sitter-language-pack` `1.9.0-rc.45` → `1.12.0`.** The grammar bootstrap
  (`ensure_grammars`) now uses tslp's `prefetch`, which probes real on-disk loadability
  instead of the in-memory `has_language` registry — replacing basemind's hand-rolled
  `DownloadManager::ensure_languages` workaround for the pre-1.12 `download()` short-circuit.
  tslp 1.12's lock-free static `get_language` fast path also removes a global-mutex hop on the
  per-thread parser pool's hot path (tree-sitter is ~30% of scan time). Validated against the
  full harden harness: all 8 OSS repos pass with 0 extraction failures and scan times at or
  under baseline.

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 12), so no
`.basemind/` rebuild.

### Fixed

- **Multiple concurrent sessions on one repo no longer break code-map navigation.** fjall takes
  an exclusive directory lock, so only one `basemind serve` process can open a repo's index at a
  time; additional editor/agent sessions fall back to read-only and previously got an error
  (`read_only_index_unavailable`) from every fjall-backed tool — the reported "blocked / not
  responsive". `find_references`, `find_callers`, `find_implementations`, and `call_graph` now
  answer from in-RAM indexes built from the content-addressed blobs (which are concurrently
  readable), so unlimited sessions work against one repo: one writer on fjall, N readers on the
  shared blobs. `outline` / `search_symbols` / `dependents` / git tools already worked read-only.

## [0.12.0] — 2026-06-29

Minor release: the document/RAG and web-crawl engines move to their renamed,
MIT-relicensed crates. `RELEASE_MINOR` bumps to 12, so the blob + index formats
are considered incompatible — `.basemind/` is wiped and rebuilt from source on
the next `basemind scan`.

### Changed

- **Document tier now builds on `xberg` `1.0.0-rc.1`** (was `kreuzberg` `5.0.0-rc.35`)
  and the web-crawl tier on `crawlberg` `1.0.1` (was `kreuzcrawl` `0.3.0-rc.55`). These
  are the renamed, **MIT-licensed** successors of the same engines, so the `documents`,
  `memory`, and `crawl` feature surfaces are unchanged. The `Elastic-2.0` license
  exceptions for the old crates are dropped from `deny.toml`.
- **Document extraction migrated to xberg's async `extract` API.** xberg 1.0 removed the
  synchronous `extract_file_sync` wrapper; the scanner's doc path now drives the async
  `extract(ExtractInput, &ExtractionConfig)` on a shared multi-thread runtime and reads the
  per-document result out of the new batch-shaped `ExtractionResult.results`.
- **`arc-swap` `1.9.1` → `1.9.2`** via `cargo upgrade`. `arrow-array` / `arrow-schema`
  stay pinned to `58` to match lancedb 0.30's transitive arrow.
- **Web crawling now blocks private/loopback/link-local targets by default** (SSRF
  protection inherited from crawlberg 1.0). `web_scrape` / `web_crawl` / `web_map` against
  `127.0.0.1`, `10.0.0.0/8`, `169.254.0.0/16`, etc. are rejected with an SSRF-policy
  violation. A new `crawl.allow_private_network` config flag (default `false`) re-enables
  them for internal-docs hosts you control; the `CRAWLBERG_ALLOW_PRIVATE_NETWORK` env var is
  also honoured as a process-wide override.

### Fixed

- **Status line no longer renders blank on Linux** — `build_basemind_line` read file mtimes
  with BSD `stat -f %m` first. On GNU coreutils `-f` means "display filesystem status" and
  *succeeds*, printing a multi-line blob instead of failing, so the `|| stat -c %Y` fallback
  never ran; under `set -euo pipefail` the blob's bare `File` word aborted the command
  substitution as an unbound variable, emptying the whole line. An `epoch_mtime` helper now
  tries GNU `stat -c %Y` first and falls back to BSD `stat -f %m` only when `-c` genuinely
  fails (macOS), used at both mtime call sites (#32).

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 11), so no
`.basemind/` rebuild. A `tar` security bump on the npm installer, a Homebrew-tap fix, and
raised runtime floors.

### Security

- **npm installer bumps `tar` 6 → 7.5.16** (GHSA-vmf3-w455-68vh) — the bundled extractor was on a
  `tar` line whose PAX size-override parsing differential allows file smuggling. The fix only exists
  in `tar` 7.5.16+, which is ESM-only and requires Node ≥ 18; `install.js` now pulls `tar` in via a
  dynamic `import()` at the extract site, and the npm package's engine floor moves to Node ≥ 22.

### Fixed

- **Homebrew tap no longer breaks on Intel hosts** — the generated formula's `on_intel` block called
  `odie` at formula-load time, which aborted every `brew` command that read the tap on an Intel Mac
  (poisoning bottle builds and installs for *all* formulae in the tap, not just basemind). The
  Apple-Silicon-only constraint is now expressed via `depends_on arch: :arm64`, evaluated at install
  time instead of load time.

### Changed

- **Runtime floors raised** — the npm package (Node ≥ 22, was ≥ 14) and the opencode plugin
  (Node ≥ 22, was ≥ 18); pip package requires Python ≥ 3.10 (was ≥ 3.8). Older runtimes are
  end-of-life.

## [0.11.0] — 2026-06-26

Minor release: `RELEASE_MINOR` bumps to 11, so the blob and Fjall index formats are considered
stale and every `.basemind/` is wiped and rebuilt from source on the next `basemind scan` (one-time,
intentional). The headline is a precomputed git-history index that turns the history MCP tools from
full-walk-per-query into sub-millisecond lookups, plus an install fix that unblocks Apple Silicon
Macs running an x86_64 shell, Node, or Python under Rosetta.

### Added

- **Precomputed git-history Fjall index** — `commits_touching`, `find_commits_by_path`, and
  `symbol_history` previously walked the commit graph on every call, paying an O(history) cost per
  query. They are now backed by a per-repo Fjall index built once at scan time and refreshed
  incrementally for new commits, so queries are O(result) sub-millisecond lookups. Posting lists are
  stored newest-first, keeping `commits_touching` O(n) in the returned commits rather than in total
  history depth.

### Fixed

- **Apple Silicon installs under Rosetta no longer abort** — `uname -m`, `os.arch()`, and
  `platform.machine()` all report the *process* architecture, so an x86_64 shell, Node, or Python
  running under Rosetta reports `x86_64` on Apple Silicon hardware. All three installers (the shell
  launcher, npm `install.js`, pip `downloader.py`) matched the Darwin/x86_64 branch and aborted with
  "Intel macOS not supported", even though the native arm64 binary runs fine. They now probe a
  hardware-level signal the translation layer cannot spoof (`sysctl -n hw.optional.arm64`) and
  resolve `aarch64-apple-darwin` whenever that probe — or the reported arch — indicates arm64.
  Fixes #28.
- **Windows `harden` test compiles again** — the new on-disk index-size measurement used
  `std::os::unix::fs::MetadataExt::blocks()` unconditionally, breaking the `windows-latest` test
  leg's compile. The block-count path is now `cfg(unix)`-gated with a `len()` fallback on other
  platforms.

### Performance

- **Path-scoped history walks** — `commit_touches_path` computed the full recursive tree diff of
  every walked commit just to test membership of one path, so a single history query over a deep
  monorepo could run for minutes (measured >5 min on a 242k-commit repo). It now looks up the entry
  at the exact path in the commit tree and each parent via `gix` `lookup_entry` (O(path depth) object
  reads, no sibling recursion), comparing `(blob oid, mode)`. Semantics are identical to
  `git log --full-history -- <path>`; the same query drops to ~11 s worst case and ~2.5 s typical.

### Changed

- **Dependencies** — bumped the embedded `rmux` shells trio (`rmux-client` / `rmux-sdk` /
  `rmux-server`) from 0.6 to 0.7 and refreshed the lockfile for compatible updates. `arrow` stays
  pinned at 58 to match what `lancedb` resolves transitively.

## [0.10.3] — 2026-06-25

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 10), so no
`.basemind/` rebuild. Three Windows-only correctness fixes surfaced once the comms suite and the
full tool sweep run on the `windows-latest` CI leg, plus install/release hardening so a partial
publish can no longer leave the plugin launcher unable to install.

### Fixed

- **Windows full scan now produces `/`-separated index keys** — the full-scan walker was optimized
  to feed `Path::to_str()` straight into the index without the `\`→`/` normalization the incremental
  `scan_paths` path still did, so on Windows every nested file was keyed with backslashes
  (`vendored\inner.rs`) and forward-slash lookups missed — breaking all subdirectory queries
  (outline / search / references for any file not at the repo root). Normalize at the walker source;
  the extra allocation is Windows-only and never touches the Unix hot path.
- **Windows comms named pipe now isolates by `comms_dir`** — `comms_socket_path` derived the
  Windows pipe name from the username only (`\\.\pipe\basemind-comms-<user>`), ignoring
  `BASEMIND_COMMS_DIR`, so every comms dir on a host collapsed onto one per-user singleton pipe.
  Parallel comms suites (each isolated to its own tempdir) collided — daemons cross-contaminated,
  one test's teardown killed another's daemon, and concurrent `comms start` races hung to the CI
  timeout. The pipe name now mixes in a hash of `comms_dir`, mirroring the per-dir Unix socket;
  production (which leaves `BASEMIND_COMMS_DIR` unset) keeps a single stable per-user broker. A
  `timeout-minutes: 30` backstop on the CI test job prevents any future hang from blocking the
  queue-not-cancel main concurrency.
- **Windows `comms start` no longer hangs** — the detached broker daemon inherited the launcher's
  stdout/stderr: on Windows `CreateProcess` runs with `bInheritHandles = TRUE` whenever stdio is
  redirected, leaking every inheritable handle into the child — including the pipe a parent captured
  via `Command::output()` (or `serve`'s MCP stdio). The long-lived daemon then held the write end
  open, so the capturing parent never saw EOF and blocked until the daemon died. Unix is immune
  (`Stdio::null` dup2's `/dev/null` over the child fds and Rust sets `CLOEXEC` on its own pipes).
  `spawn_detached_daemon` now clears `HANDLE_FLAG_INHERIT` on its std handles before the detached
  spawn, so the daemon inherits none of them; the real-daemon comms E2E suite runs on Windows again.
- **Plugin launcher names an incomplete release instead of failing opaquely** — when a pinned
  release is missing a platform binary or its checksums file (a partial publish, as 0.10.0 was),
  `mcp-launch.sh` died with a bare "download failed" / "could not fetch checksums" that the MCP
  client surfaced only as "failed to connect". It now reports the release as incomplete and tells
  the user to update the plugin (Claude Code: `/plugin update`) to a complete release.
- **Releases publish atomically** — `create_release` made the GitHub release live before its
  binaries finished uploading, so a failed platform build (as 0.10.0's Linux legs) left it
  half-populated with no checksums, and the launcher's checksum-verified download then fails closed
  for every user pinned to it. The release is now created as a **draft**; a `finalize_release` job
  promotes it only once all four platform archives **and** the checksums file are present (and the
  npm / PyPI / Homebrew publishes gate on that finalize, since they download from the release). A
  failed platform build now leaves a hidden draft, never a live, broken release.

## [0.10.2] — 2026-06-25

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 10), so no
`.basemind/` rebuild. Fixes the Linux release archive, which 0.10.1 shipped broken on a clean host.

### Fixed

- **Linux release binaries: bundled libraries could not find their siblings** — the archive
  bundles native `.so`s (libheif, libaom, …) into `lib/`, but only the main binary carried the
  `$ORIGIN/lib` rpath. A bundled lib with a sibling dependency (`libheif.so.1` → `libaom.so.3`,
  both in `lib/`) had no rpath of its own, so on a clean host the loader failed with
  `libaom.so.3: cannot open shared object file` — even on `basemind --version`. The in-container
  packaging smoke missed it because the build container has those codecs system-installed.
  `package-release.sh` now sets `$ORIGIN` on every bundled lib so sibling-to-sibling deps resolve,
  verified by running the real release artifact on a clean glibc-2.28 host.

## [0.10.1] — 2026-06-25

Patch release: blob and index formats are unchanged (`RELEASE_MINOR` stays 10), so no
`.basemind/` rebuild. This release completes the 0.10.0 distribution — it ships the Linux
binaries and the npm / PyPI / Homebrew packages that 0.10.0 could not produce.

### Fixed

- **Linux release binaries (x86_64 + aarch64)** — the 0.10.0 Linux build linked ort's prebuilt
  ONNX Runtime with `cargo-zigbuild`, which fails because zig links LLVM `libc++` while the
  prebuilt is built against GNU `libstdc++`. The Linux binaries now build inside the official
  `manylinux_2_28` containers with native gcc. The prebuilt ort also references glibc 2.38 symbols
  (`__isoc23_strtol/ll/ull`, `__libc_single_threaded`); a small compatibility shim
  (`scripts/glibc228_compat.c`) backfills them so the binaries keep a **GLIBC_2.28** floor
  (RHEL 8 / Debian 11 / Ubuntu 20.04+ / Amazon Linux 2023). Because 0.10.0's binary legs failed,
  its checksums and the npm / PyPI / Homebrew publishes were skipped; 0.10.1 carries the same
  source and completes every distribution channel.

## [0.10.0] — 2026-06-24

Minor release: `RELEASE_MINOR` bumps 9 → 10, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan`. This rebuild is **one-time and
harmless** — the blob and index formats are unchanged.

### Added

- **Agent shells: embedded rmux daemon + six MCP tools** — spawn detached headless shell sessions
  via `shell_spawn` with optional cwd, env overrides, and title. Drive sessions with `shell_send`
  (write stdin), `shell_capture` (visible screen), `shell_broadcast` (multicast input), `shell_list`
  (enumerate with liveness), and `shell_kill` (terminate). basemind embeds the daemon (re-execs
  itself with `--__internal-daemon` — no external `rmux` binary). Sessions are long-lived across
  tool calls and driven headless via MCP.
- **Visual attach** (Unix + Windows) — when `[shells].visual` is not `headless`, `shell_spawn` opens
  the session in a terminal tab/window attached to it via a hidden `basemind --__internal-attach`
  re-exec (macOS / Linux emulators or Windows Terminal `wt.exe`; basemind ships no external `rmux`
  binary). Presentation is best-effort — a spawn never fails just because no terminal could be driven
  — and the response's `attach_command` is returned for manual re-attach.
- **Comms-coupled session rooms** (Unix + Windows, `comms` feature) — spawned children auto-join a
  session-scoped comms room via inherited `BASEMIND_SESSION_ID` / `BASEMIND_PARENT_AGENT_ID` /
  `BASEMIND_AGENT_ID`, enabling bidirectional parent↔child messaging and forming parent→child
  inheritance chains across agents.
- **`[shells]` configuration sub-tree** — controls visual presentation mode (`current` / `window` /
  `web` / `headless`; default `current` = new tab in open terminal), terminal emulator choice
  (`auto` / `iterm2` / `terminal_app` / `windows_terminal` / `gnome_terminal` / `konsole` /
  `wezterm` / `alacritty` / `kitty` / `xterm`), and session pty dimensions (`default_cols` /
  `default_rows`) and lifecycle (`keep_on_exit`). Requires `--features shells`.
- **Multi-agent comms orchestration** — one `serve` now drives many named sub-identities over a
  single connection via a per-call `as_agent` parameter, backed by a per-identity broker-client
  registry parented to the orchestrator. New `dm_send` delivers a direct message into another
  agent's inbox over a private pairwise room; `agent_register` / `agent_list` advertise and discover
  the live roster. The whole surface is mirrored on the CLI (`basemind comms … --as-agent`, `dm`,
  `room-for-path`), and a `multi-agent-room` skill + a code-review-panel demo show the pattern.
- **Per-repo comms rooms + recency** — agents auto-join a default room keyed by the repo, created on
  demand via `get_or_create_chat_room_for_path`; `Global` is repurposed for machine-wide ops
  coordination. Reads are recency-aware: `room_history` / `inbox_read` default to the last 24h
  (`since_hours` widens, `0` reads the full append-only log), each message carries `age_secs`, and
  `room_list` flags a room `stale` after 7 days of silence.
- **Windows support for agent comms + shells** — the comms broker (named-pipe transport, singleton,
  signal handling) and the embedded rmux shell runtime now build and run on Windows, and the comms
  MCP tools are exposed there. `--features full` therefore ships agent comms and agent shells on
  every published platform — macOS, Linux, and Windows — not Unix only. A dedicated Windows CI job
  builds and tests the `comms` + `shells` surface.

### Changed

- **Dependency refresh** — kreuzberg `5.0.0-rc.30` → `rc.35` (document-tier extraction stack) and
  `rmcp` `1.7` → `1.8`. `arrow` stays at 58 (lock-step with lancedb 0.30). rmcp 1.8 deprecates MCP
  logging (SEP-2577); basemind keeps the capability for now since the status line and `rescan`
  progress depend on it.
- **Linux release binaries target glibc 2.28** (RHEL 8 / Debian 11 / Ubuntu 20.04+ / Amazon Linux
  2023) — the two `*-unknown-linux-gnu` artifacts are built with `cargo-zigbuild` (zig as the
  linker), pinning the required glibc symbol floor without changing runtime behaviour (still
  dynamically linked). A CI `objdump` guard fails the build if a dependency pulls a newer symbol.
  macOS / Windows artifacts are unchanged.

### Removed

- **Experimental A2A server (`--features a2a`)** — the gRPC + JSON-RPC/SSE Agent-to-Agent server,
  its protobuf codegen + `proto/`, the `basemind a2a serve` command, the buf CI job, and the
  a2a-only dependency stack (tonic, prost, axum, axum-server, tower, tower-http, rustls, reqwest,
  subtle, rcgen, …) are removed. It was opt-in and never part of the shipped release surface; the
  maintenance cost (pinned codegen, held-back deps, a dedicated buf CI job) outweighed its
  experimental value. The comms agent-card shapes stay A2A-schema-aligned, so an HTTP front-end can
  still be added behind the `CommsFrontend` trait later without it.

### Fixed

- **Comms daemons no longer pile up** — a busy broker that missed a single liveness ping could be
  wrongly judged dead and have its socket unlinked-and-rebound by a new daemon, orphaning the
  original on a dangling socket that nothing reaped promptly; across many sessions dozens
  accumulated. Two guards: the liveness probe now retries before declaring a daemon dead (so a
  live-but-busy broker is not reclaimed), and a Unix socket-ownership watchdog makes any daemon
  whose socket was unlinked or replaced self-terminate within seconds instead of lingering as an
  orphan.
- **Comms daemon takes over a previous build on load** — when a serve or CLI brings up the broker
  and finds the socket held by an older or protocol-incompatible daemon, it now asks that daemon to
  stop and spawns a current one in its place, converging the singleton on the newest binary; if the
  predecessor will not yield, it errors out clearly (naming the version + pid) instead of silently
  talking to an incompatible daemon — which is how the pre-0.10 skew surfaced as an opaque
  "connection closed". A same-version (or newer) daemon is reused, so concurrent sessions still
  share one broker.
- **Comms messages now expire** — the broker prunes messages past a 7-day TTL on startup and hourly
  thereafter, so the user-global comms store cannot grow without bound. Recency-aware reads already
  hid old messages; this reclaims their storage. Room records and per-room sequence counters are
  left intact so read cursors stay monotonic.
- **Concurrent serve sessions no longer collide** (#26, #27) — the editor plugin spawns one
  `basemind serve` per session, but the store write lock is single-holder. A contending serve now
  starts in a **read-only** mode (instead of exiting and handing the MCP client an opaque `-32000`),
  so its tools register and the session is usable: `outline`, `search_symbols`, `list_files`,
  `dependents`, `workspace_grep`, and the git tools answer from the in-RAM map and git. Fjall takes
  an exclusive lock on its index, so the call/reference tools (`find_references`, `find_callers`,
  `find_implementations`, `call_graph`) cannot read it from a second process — they now return a
  clear "held by another basemind serve" error rather than a misleading empty result. The
  lock-holding serve stays the sole writer; a read-only serve's `rescan` returns the same clear
  error. Lock contention fails fast with the clean lock message rather than the multi-GB busy-spin
  reported on 0.9.0 (#26).
- **Agent-shells hardening** — `shell_spawn` now honours the `[shells].enabled` master switch,
  confines `cwd` to the repository root (rejecting `..` / absolute escapes), threads the configured
  `default_cols` / `default_rows` into the spawned pty, validates `BASEMIND_SHELLS_SOCKET` on the
  client path, rejects carriage returns in env keys/values, and widens the loader-injection warning
  list (`LD_AUDIT`, `DYLD_FALLBACK_LIBRARY_PATH`). The shells and comms modules build cleanly on
  both Unix and Windows (see Added); a build without those features still excludes them entirely.
- **Status line recognizes pre-0.9 indexes** — the basemind status line keyed its "scanned yet?"
  check on the fused `.fm` blob introduced in 0.9, so an index written by an earlier binary (split
  `.l1`/`.l2` blobs) showed "scanning…" indefinitely though it was healthy. It now accepts both
  layouts and never double-counts the secondary blob layer.

### Security

- **`quinn-proto` bumped to 0.11.15** (RUSTSEC-2026-0185) — fixes a remote memory-exhaustion vector
  via unbounded out-of-order QUIC stream reassembly, pulled transitively under `--features full`.
- Known/tracked advisories carried this release (no fix available yet): `bincode` 1.3.3 unmaintained
  (via `rmux-proto`), `memmap2` 0.9.10 unsound pointer offset (via `lancedb`, documents/memory).

## [0.9.0] — 2026-06-23

Minor release: `RELEASE_MINOR` bumps 8 → 9, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan`. This rebuild is **structural** — the
per-file extraction blob format changed from two files (`<hash>.l1.msgpack` + `<hash>.l2.msgpack`)
to one combined frame (`<hash>.fm.msgpack`); any old split blobs left on disk are reclaimed by
`cache gc`. A scanner hot-path pass cuts cold-scan wall time ~22%. Bumps kreuzberg to `5.0.0-rc.30`.

### Changed

- **Scanner: ~22% faster cold scans** — two flamegraph-ranked wins. (1) Per-file Fjall index commits
  are now batched per rayon worker (256 files per commit), removing the ~14% of scan time worker
  threads spent serializing on Fjall's single write lock. (2) The L1 outline and L2 calls are fused
  into one content-addressed blob (`<hash>.fm.msgpack`), so the default eager-L2 scan does one
  `open` + atomic `rename` per file instead of two. Measured min-of-7 cold scans on a 6.7k-file repo:
  2.68 s → 2.07 s; the per-file read-before-write index atomicity is preserved.
- Bump **kreuzberg to `5.0.0-rc.30`** — picks up the upstream Tesseract `tessdata`-path fix, so the
  released binary resolves trained data at runtime instead of baking in the build runner's path.
  Resolves #12.
- Dependency refresh to latest compatible (gix 0.85, criterion 0.8, …). arrow stays at 58 to match
  lancedb 0.30; the experimental `a2a` feature's gRPC deps are held pending a tonic-0.14 migration.

### Fixed

- **Comms daemons no longer leak across sessions.** A daemon orphaned by a dead session (reparented
  to pid 1) used to keep its socket + flock and never exit, so they accumulated indefinitely. The
  broker now tracks connected links + last activity and self-terminates after 30 min with no clients,
  via the normal drain path. A live client (even a quiet subscriber) keeps it alive. Unix-only.
- Removed the document-tier troubleshooting workarounds from the README now that both upstream
  kreuzberg bugs are fixed (TLS-MITM in rc.29, `tessdata` in rc.30). Resolves #14.

### Removed

- The pre-0.9 split-tier blob format (`.l1.msgpack` / `.l2.msgpack`), superseded by the combined
  `.fm.msgpack` frame. `cache gc` reclaims any left behind by the schema-bump refresh.

## [0.8.0] — 2026-06-22

Minor release: `RELEASE_MINOR` bumps 7 → 8, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan` (one-time; the blob/index formats are
unchanged, so the rebuild is harmless). Deepens the MCP surface — prompts, argument completions,
logging, and progress notifications — and hardens the consumer experience: cleaner document-tier
scans, a cold-start scan path agents can invoke, serve diagnostics, and a recovery runbook. Bumps
kreuzberg to `5.0.0-rc.29`.

### Added

- **MCP prompts** (`prompts/list` + `prompts/get`) — four reusable, parameterized workflows that
  teach a client to drive basemind structure-first: `onboard-repo`, `trace-symbol`, `explain-file`,
  `review-working-tree`.
- **MCP argument completion** (`completion/complete`) — autocompletes prompt arguments from the
  in-RAM code map: `trace-symbol`'s `symbol` against indexed symbol names, `explain-file`'s `path`
  against indexed file paths (pure prefix scan, no store lock).
- **MCP logging + progress** — `logging/setLevel` plus a `rescan_complete` log with scan counts,
  and start/done progress notifications on `rescan` when the client supplies a progress token.
- **Tool annotations** on every MCP tool (`read_only_hint` / `destructive_hint` / `idempotent_hint`
  / `open_world_hint`) so clients (Claude Code et al.) can auto-approve read-only tools and only
  prompt for mutating ones.
- **Cold-start indexing for agents** — a `bm-scan` command + `basemind-scan` skill that run
  `basemind scan` via the CLI (no MCP server required), so an agent can build the index when
  basemind reports "no index".
- **Recovery runbook** — a `bm-doctor` command + `basemind-doctor` skill: diagnose the index, detect
  a stale lock via the `.lock.meta` holder pid, rebuild via scan, and reconnect the MCP server
  (client-specific). Plus `basemind serve` now logs its lifecycle (startup pid/version/view and the
  exact exit reason) to its stderr / the client's MCP server logs.

### Changed

- Bump **kreuzberg to `5.0.0-rc.29`** — picks up the TLS-certificate fix for model downloads behind
  corporate TLS-MITM proxies (`hf-hub` now uses the platform `native-tls` provider, honoring the OS
  cert store + `SSL_CERT_FILE`/`SSL_CERT_DIR`). Resolves #14.

### Fixed

- The document tier no longer counts non-extractable files as failures. A file tree-sitter doesn't
  recognize as code (e.g. an Erlang `.app.src`, which `mime_guess` maps to
  `application/x-wais-source`) is now **skipped** — and logged at debug — instead of inflating the
  failed count; genuine extraction errors on real documents still count as failures.

## [0.7.0] — 2026-06-22

Minor release: `RELEASE_MINOR` bumps 6 → 7, so the on-disk index version changes and every
`.basemind/` cache rebuilds from source on the next `scan` (intentional, one-time). The headline is a
first-class **code-aware token-reduction** surface (the `compress`/`expand` tools, per-call budgets,
TOON encoding, behavioral output compression) plus **code-grounded memory/skill governance**
(`memory_audit` + git-mined skill proposals). Also ships MCP tool annotations so clients can
auto-approve read-only tools, and a sweep of CLI/MCP bug fixes.

### Added

- **`compress` MCP tool** — code-aware compression (tree-sitter structural elision + cheap lexical
  passes) with an honest before/after token report; **`expand`** pulls a single symbol's body back.
- **Per-call `max_tokens` budgets** on high-volume tools (`outline`, `search_*`, `find_references`,
  `workspace_grep`, `search_documents`, …): rank-to-fit with an explicit truncation marker + cursor.
- **Opt-in TOON encoding** (`format:"toon"`) for high-volume list responses, and an opt-in **lean
  tool surface** (`BASEMIND_MCP_LEAN`) that advertises three wrapper tools instead of the full set.
- **Behavioral output compression** (`basemind compress-output`/`delta`/`checkpoint`/`detect-waste`)
  with credential-safe, fail-open semantics, plus opt-in plugin hooks (Bash-output compression,
  read-cache deltas).
- **Code-grounded governance** — `memory_audit` verifies stored memories against the live code map
  (structural-hash drift → stale, decay + 90-day auto-archive) and git-mined **skill proposals**
  (`proposals_mine`/`proposals_list`/`proposal_accept`/`proposal_reject`); additive
  provenance/verification fields on `MemoryRecord` and two new lazy Fjall keyspaces.
- **MCP tool annotations** on all tools (`read_only_hint`/`destructive_hint`/`idempotent_hint`/
  `open_world_hint`) so consumers (Claude Code et al.) can auto-approve read-only tools.
- **Real tokenizer-backed token counts** (o200k under `documents`) in the compress report and
  telemetry, and instructive `-32602` parameter errors via a `Lenient<T>` wrapper + serde aliases
  (accept `query`/`pattern`/`needle`/`name`/`regex`/… across the search tools).
- Plugin integrations for **Kimi Code** and **pi**, and a Codex-readable marketplace manifest.

### Changed

- Bump **kreuzberg to `5.0.0-rc.28`**.
- Honest telemetry: `est_tokens_saved` now credits document/list/web tools instead of reporting 0,
  and `search_symbols.total` (`total_is_partial`) / `list_files.limit_clamped` / `status.blob_count`
  expose what was previously silent.
- `scan` / `rescan` exit `0` when per-file read failures occur but the index was still updated
  (previously exit 2 masked a successful scan); `scan --rev` skips submodule gitlinks instead of
  failing; `-q`/`--quiet` now suppresses subsystem INFO/WARN logs.
- `cache clear` gains a `views:<name>` selector to clear a single view without nuking all of them.
- Slimmer tool descriptions to cut per-session schema tokens.

### Fixed

- Lock-contention errors now name the actual holder (`serve`/`watch`/`scan`/`rescan`) via a `.lock`
  sidecar instead of always guessing `watch` (#11).
- `query` commands resolve absolute and `./`-prefixed paths to the indexed key (#19); serving or
  querying a never-scanned named view now errors instead of silently returning empty (#18).
- `comms status` gives an actionable "daemon not running — start with `basemind comms start`" message
  (#21); comms verbs no longer emit a false "`--json` has no effect" warning (#20).
- `git_cache_bytes` documented as the disk layer only (#23); CLI lock / LanceStore-shutdown crashes
  and legacy comms front-matter decoding fixed.
- Known upstream issues in the document tier (OCR `tessdata` path #12, model download behind a
  TLS-MITM proxy #14) are documented with workarounds; fixes tracked in kreuzberg and picked up on a
  later bump.

## [0.6.3] — 2026-06-21

Patch release: schema unchanged (`RELEASE_MINOR` stays 6). Reworks the plugin MCP launcher so it
no longer depends on `npx`/`uvx` at runtime — fixing intermittent start-up failures when several
agent sessions (or the comms-monitor poll loop) launch basemind concurrently.

### Fixed

- **Plugin launcher npx/uvx race** — `scripts/mcp-launch.sh` previously exec'd `npx basemind@<ver>`
  (then `uvx`) as the runtime. npx stages into a shared, spec-hashed `~/.npm/_npx/<hash>` directory,
  so two concurrent launches raced on it and failed with `ENOENT … package.json`; the binary was also
  never cached, so every launch re-resolved over the network and inherited node/python start-up cost
  plus lavamoat postinstall blocks. The launcher now has a single install method: download the
  checksum-verified prebuilt release binary once into a stable per-user cache
  (`${XDG_CACHE_HOME:-~/.cache}/basemind/bin/<version>/`), serialized with an atomic lock, and exec it
  directly on every subsequent launch. `npx`/`uvx` and the `BASEMIND_LAUNCHER` override are removed;
  set `BASEMIND_BIN=/path/to/basemind` to point at a local dev build. The npm/PyPI/Homebrew/cargo
  install channels are unchanged.

## [0.6.2] — 2026-06-21

Patch release: schema unchanged (`RELEASE_MINOR` stays 6). Fixes the macOS binary so the
npm/PyPI/Homebrew/cargo wrappers actually run — 0.6.0 and 0.6.1 shipped an unrunnable macOS binary.

### Fixed

- **macOS binary SIGKILL on launch** — the release packaging bundles non-system dylibs into `lib/`
  and rewrites their `@loader_path` install names with `install_name_tool`, which edits the Mach-O in
  place and invalidates the linker's ad-hoc code signature. macOS then killed the process on first
  page-in (`SIGKILL`, "Code Signature Invalid") with no output, so the binary downloaded by every
  wrapper was unrunnable. The packaging now re-signs each dylib and the binary ad-hoc **after** the
  rewrites and verifies `--strict` before packaging.
- **pip/uvx downloader** — a stale or partial `~/.cache/basemind/<ver>/` made `os.replace()` fail with
  `Errno 66` (cannot overwrite a non-empty directory), wedging installs until a manual cache wipe. The
  downloader now clears a stale cache dir under its lock and retries the atomic move.

## [0.6.1] — 2026-06-21

Patch release: blob/index/comms schema unchanged (`RELEASE_MINOR` stays 6), so no
`.basemind/` rebuild. Fixes the Windows release build that blocked the 0.6.0 wrapper publish.

### Fixed

- **Windows build** — the agent-comms substrate is built on Unix domain sockets
  (`UnixStream`/`UnixListener`, peer-cred auth) with no Windows analogue, but `comms`
  imported `tokio::net::UnixStream` ungated, so the `--features full` `x86_64-pc-windows-msvc`
  build failed to compile (since comms landed in 0.5.0). This failed the Windows binary job and
  held back the npm/PyPI/Homebrew publishes. The comms surface is now gated on
  `all(feature = "comms", unix)` — on Windows the binary compiles with comms absent; unix is
  unchanged. 0.6.0 published to crates.io only; 0.6.1 is the first complete cross-channel release
  of the 0.6 line.

## [0.6.0] — 2026-06-20

Minor release: `RELEASE_MINOR` bumps 5 → 6, so the blob, Fjall-index, and LanceDB schema versions
advance — the first `basemind scan` / `serve` after upgrading **wipes and rebuilds the `.basemind/`
cache and the LanceDB store in place**. Headline: an **experimental, opt-in Agent-to-Agent (A2A)
task server**, plus scanner/query hot-path perf wins and release-pipeline hardening.

### Added

- **Experimental A2A server** — `basemind a2a serve`, gated behind `--features a2a` and intentionally
  excluded from the default and `full` builds and from the shipped release binary. One axum listener
  serves the A2A task protocol three ways — gRPC `A2AService`, JSON-RPC 2.0, and SSE streaming — with
  the agent card at `/.well-known/agent-card.json`. Hardened surfaces: **bearer-token auth** (constant-
  time compare, non-loopback bind refused without a token), **TLS termination** (`--tls-cert` /
  `--tls-key`, ALPN h2 + http/1.1), and **push-notification webhook delivery** behind a real **SSRF
  guard** (rejects loopback, RFC-1918, link-local, and the cloud-metadata IP; re-checks the resolved
  address and pins it on connect).
- **`rescan` levers** (CLI + MCP) plus robust auto-rescan on an empty working-view index.
- **comms `inbox_ack`** — advances the per-agent read cursor (idempotent, non-destructive; the shared
  append-only room log is untouched) — and **richer message front-matter** (`ts_micros`, `tags`,
  `reply_to`, `seq`, and an optional `scope` path/glob set for relevance filtering).

### Changed

- **Scanner/query perf** — cache the parse-timeout env lookup, pre-classify L1 captures, and defer the
  walk allocation in the scanner inner loop; serve `outline` from the in-RAM cache to cut query-path
  allocations; bounded-concurrent crawl indexing aligned with the kreuzberg parallelism pool.

### Fixed

- **Atomic release publishing + resilient wrappers** — a version-consistency gate across all shipped
  surfaces, an unconditional checksums job asserting every archive, `needs`/`result == success` guards
  on each publish job, and fail-closed retrying npm/pip installers with an atomic cache. Fixes the
  v0.5.0 broken-install regression (missing checksums file → every wrapper install failed).
- **comms daemon** reconnects and respawns on a broken pipe instead of failing the request.

## [0.5.0] — 2026-06-20

Minor release: `RELEASE_MINOR` bumps 4 → 5, so the blob, Fjall-index, and LanceDB schema versions
advance — the first `basemind scan` / `serve` after upgrading **wipes and rebuilds the `.basemind/`
cache and the LanceDB store in place**. Headline: basemind becomes a multi-agent **communication
substrate** (scoped rooms + per-agent inbox) on top of its context layer, with split memory and
cross-harness delivery.

### Added

- **Agent-to-agent communication.** A singleton, user-global **broker daemon** (its own Fjall
  store over a Unix socket, independent of any repo's exclusive index lock) hosts **scope-joined
  rooms** — an agent auto-joins every room covering its git remote, a path prefix, or global.
  Messages are **two-tier**: a front-matter envelope (subject · from · id) that `room_history` /
  `inbox_read` scan cheaply, plus a body fetched on demand by `message_get`. An agent's **own**
  posts are excluded from its inbox. New MCP tools `agent_register`, `agent_list`, `room_create`,
  `room_list`, `room_join`, `room_leave`, `room_post`, `room_history`, `message_get`, `inbox_read`,
  with full `basemind comms …` CLI parity.
- **Split memory.** Memory gains an **individual** (per-agent) tier alongside the existing
  **group** (shared) tier, selected by a `visibility` parameter; agent identity resolves from
  `BASEMIND_AGENT_ID` → config → persisted `.basemind/agent-id` → `anon`.
- **Cross-harness delivery.** The comms mandate and a "prefer basemind over grep/git/manual"
  directive ship to every harness via the MCP server `instructions`, a new **`basemind-comms`
  skill**, and the generated per-harness instruction files. **SessionStart + UserPromptSubmit
  hooks** inject unread front-matter on boot and per turn; a **background monitor (~15 s)** surfaces
  new messages while an agent works or idles (Claude Code).
- **Statusline comms segment.** The basemind statusline now shows the agent's unread message count
  (bright when non-zero) and identity, gated on a running broker and TTL-cached so it stays cheap.

### Changed

- **Schema bump: `RELEASE_MINOR` 4 → 5.** The blob, Fjall-index, and LanceDB `memory`/`documents`
  schema versions advance, so the first `basemind scan` / `serve` after upgrading **wipes and
  rebuilds the `.basemind/` cache and the LanceDB store in place** (the latter now guarded by a
  `memory_schema_ver` in `meta.json` so the memory-table column add never faults at query time).
  Stored memory is rebuildable scratch, not a source of truth — re-`memory_put` anything you want
  to keep, or expect it to be re-derived.

## [0.4.0] — 2026-06-19

Minor release: `RELEASE_MINOR` bumps 3 → 4, so the blob + index schema versions advance.
The first `basemind scan` / `serve` after upgrading rebuilds the cache in place. Prebuilt binaries
now ship `--features full` (96 document formats, OCR, embeddings, reranker, semantic search, web
crawl, shared memory). **Windows asset triple changed:** `x86_64-pc-windows-gnu` → `x86_64-pc-windows-msvc`
(ONNX Runtime ships no MinGW prebuilts); anyone hard-coding the old asset name must update.
**Intel macOS (`x86_64-apple-darwin`) is no longer shipped** — macOS prebuilts are Apple Silicon
(`aarch64-apple-darwin`) only; Intel Mac users build from source (`cargo install basemind --features full`).

### Added

- **Full-feature prebuilt binaries via native per-platform runner.** All prebuilt binaries
  (Homebrew, npm, pip, GitHub Releases) now ship `--features full`: 96 document formats (PDF,
  Office, email, HTML, archives, structured, source, markup), OCR (Tesseract), HEIC/AVIF
  (libheif), embeddings + reranker + NER, shared memory, and web crawl. First use downloads ML
  models over the network; binaries are larger. Replaced goreleaser with a new native
  per-platform CI runner pipeline.
- **Enforced binary checksum verification.** All install paths (npm, pip/uvx, direct download via
  `mcp-launch.sh`) now verify the binary's sha256 against `basemind_<version>_checksums.txt` and
  fail closed on mismatch or missing checksum. (This was previously claimed in the README but not
  implemented.)

### Changed

- **`find_references` and `find_implementations` now do real substring matching.** Previously
  documented as substring matching but actually exact-only. Now genuinely substring (case-sensitive),
  so `Foo::bar()` and `bar()` both match `name="bar"`.
- **Dropped Intel macOS (`x86_64-apple-darwin`) prebuilt binaries.** macOS releases are Apple Silicon
  only. npm/pip/Homebrew install on an Intel Mac now fails fast with a clear message; build from source
  instead.

### Fixed

- **SSRF denylist + redirect revalidation for web crawler.** Added safeguards to prevent server-side
  request forgery on the `web_scrape` and `web_crawl` tools.
- **Configured embedder preset.** Eliminates cross-process vector state wipe, improving performance
  and cache isolation.
- **Atomic memory writes.** Prevents torn writes on concurrent `memory_put` across sessions.
- **Index panic on corrupted entries.** Fixed a panic when the Fjall index contained malformed keys
  (now degrades gracefully).
- **Git blame robustness.** Improved handling of edge cases (missing commits, detached HEAD, shallow
  repos).
- **Config layer precedence fixes.** MCP > CLI > env > TOML > defaults now strictly enforced.
- **Hot-path allocation cuts.** Reduced allocations in scanner, extract, and query paths; removed
  unnecessary `String::from_utf8` round-trips and clones in the Fjall secondary-index scans.

## [0.3.0] — 2026-06-18

Minor release: `RELEASE_MINOR` bumps 2 → 3, so the blob + index schema versions advance.
Unlike previous minor bumps, the cache is **no longer hard-wiped** — see "Durable cache
refresh" below. The first `basemind scan` / `serve` after upgrading re-extracts in place.

### Added

- **Full MCP↔CLI parity.** Every MCP tool now has a `basemind` CLI subcommand, running the
  identical in-process tool code against the same on-disk index (read-only, no lock — safe
  to run alongside a live `basemind serve`). Human-readable output by default, `--json` for
  the raw structured response. New groups: `basemind query` (outline, symbol, search,
  references, callers, implementations, call-graph, grep, list-files, status, repo-info,
  dependents), `basemind git` (status, history, blame, diffs, churn, symbol-history),
  `basemind memory`, `basemind web`, and `basemind telemetry`.
- **Cache garbage collection + cleanup.** New `cache_gc` (reclaim orphaned blobs no view
  references), `cache_stats` (on-disk size + orphan accounting), and `cache_clear` MCP
  tools, plus `basemind cache gc|stats|clear` CLI commands. GC runs automatically in the
  background on `serve` startup. Blobs are shared across views; GC only sweeps blobs no
  view's index references, under the store lock so it never races a scan.
- **Continuous background watch.** `basemind serve` now watches the working tree and
  incrementally refreshes its index in the background by default (debounced), keeping
  queries fresh without manual `rescan`. Opt out with `basemind serve --no-watch` for very
  large repos / CI.
- **`basemind-cli` skill** documenting the CLI surface, and a restructured README with two
  equal install paths (MCP plugin vs CLI + skill).

### Changed

- **Durable cache refresh on schema bump.** A schema/version bump now refreshes the
  content-addressed blob store **in place** (re-extract overwrites blobs at their stable
  content-hash path; background GC reclaims the remainder) instead of deleting
  `.basemind/blobs/`. No window where the expensive extraction cache is gone. The Fjall
  secondary index (derived, cheap) still rebuilds from blobs on its own schema bump.
- **Git-cache schema** is now tied to `RELEASE_MINOR` so a release refreshes it consistently
  (it auto-rebuilds from git).

### Fixed

- **Tightened token-savings estimate.** The `telemetry_summary` "tokens saved" figure no
  longer scales with total corpus size (it previously credited ~5% of the whole repo per
  grep-style call). Baselines are now derived from the actual response payload, decoupled
  from corpus size.
- **Read-only store degrades gracefully on a schema bump.** A CLI query run before the first
  post-upgrade scan now reads an empty index ("run `basemind scan`") instead of erroring,
  and never opens the stale Fjall index.

## [0.2.6] — 2026-06-18

### Changed

- **Responsive two-line statusline with per-capability metrics.** The status line now
  renders a context row (model · output-style · vim · dir · branch · context%) above the
  basemind row, since a custom statusLine replaces Claude Code's default and cannot sit
  below it. The basemind row breaks telemetry down per capability — searches, git, docs,
  memory, web — showing only buckets with activity today, alongside calls and estimated
  tokens saved. Width comes from `$COLUMNS` with full/compact/minimal tiers; override with
  `BASEMIND_STATUSLINE` and hide the context row with `BASEMIND_STATUSLINE_CONTEXT=0`.

## [0.2.5] — 2026-06-18

### Added

- **PreToolUse guard hook drives basemind usage.** A configurable hook
  (`hooks/pre-tool-guard`) reaches the agent when it reaches for `Grep`/`Glob` and
  points it at the matching basemind tool — the only Claude Code lever that can also
  cover subagents. `BASEMIND_GUARD` selects the mode: `nudge` (default, advisory once
  per session), `redirect` (deny with a pointer to the basemind tool), or `off`.
- **Full-capability awareness in the agent surfaces.** The always-injected MCP
  `get_info` instructions and the `basemind` skill now describe the whole surface —
  code map across 300+ languages, full-text + semantic search, git intelligence,
  document RAG over 90+ file formats, shared memory, and web crawl — instead of a
  code-map-only framing. The README leads with the same capability set.

### Fixed

- **`Store::open` no longer races on a just-released lock.** `acquire_lock` retries the
  exclusive `flock` with a short backoff, fixing a macOS-only flake where a sequential
  open → close → open hit `Locked`.

## [0.2.4] — 2026-06-18

### Added

- **Context-economy operating discipline shipped across every agent surface.** The
  MCP `get_info` instructions, the `basemind` skill, the SessionStart hook, and the
  README now state one default workflow: these tools return paths, line numbers, and
  signatures rather than file bodies, so agents `outline` before reading,
  `search_symbols` / `find_references` / `workspace_grep` before grep, and `rescan`
  after edits. The SessionStart hook injects this every session instead of only
  nudging when the statusline is unset.

## [0.2.3] — 2026-06-18

### Added

- **`serve` auto-scans a fresh repo on startup.** When the working-view index is
  empty, the server kicks off an initial scan in the background (reusing the
  in-process `rescan` path, so it never contends for the Fjall lock). The agent
  no longer has to run `basemind scan` by hand — the statusline flips from
  "scanning…" to live counts on its own.
- **`.basemind/` is now self-ignoring.** The first time the store is created it
  writes `.basemind/.gitignore` containing `*`, so a user's repository never
  accidentally commits the machine-local index. An existing `.gitignore` is left
  untouched.

### Changed

- **README redesign.** Centered hero with a uniform badge row, a fenced
  statusline preview (replacing a broken image reference), and collapsible
  architecture / harness-setup sections. Clarified that registering the
  marketplace and installing the plugin are two distinct steps.

### Fixed

- **Nightly hardening CI is green again.** The `hardening` job built
  `--features full` (kreuzberg's libheif + tesseract stack) but only installed
  protoc, so the `libheif-sys` build failed. The job now installs the codec dev
  libraries and builds libheif 1.23.0 from source (cached across runs).

## [0.2.2] — 2026-06-17

### Added

- **Claude Code plugin now auto-installs the basemind binary.** The MCP launcher
  (`scripts/mcp-launch.sh`) tries version-matched cached binaries, then npx
  (npm package), uvx (PyPI package), and finally downloads the prebuilt binary
  from the GitHub release with checksum verification. Override the launch strategy
  with `BASEMIND_LAUNCHER=auto|npx|uvx|download`.
- **SessionStart hook pre-warms the binary and nudges statusline setup.** The
  hook runs in the background on session start to ensure the first tool call isn't
  a cold install, and suggests `/bm-statusline` if the statusline isn't yet
  configured.
- **`/bm-statusline` slash command wires the statusline into
  `~/.claude/settings.json`.** Plugins cannot set the main statusline
  automatically; `/bm-statusline` is the one-step opt-in.

## [0.2.1] — 2026-06-17

### Fixed

- **Claude Code slash commands now load.** The `/bm` and `/bm-stats` commands
  were under `.claude-plugin/commands/`, which Claude Code ignores — plugin
  component directories must sit at the plugin root, and basemind's marketplace
  `source` is `"./"`, so the plugin root is the repo root. Moved `commands/`
  (and confirmed `skills/`) to the repo root; `.claude-plugin/` now holds only
  the manifests + statusline. The Codex / Cursor / OpenCode trees keep their own
  in-tree copies (their plugin root is their own directory), kept in sync by
  `scripts/sync-plugin-skills.sh`.

### Changed

- **Publish workflow no longer hard-fails on a missing Homebrew token.** The
  GitHub-release-assets job skips the Homebrew tap publisher when
  `HOMEBREW_TOKEN` is absent (and runs it when present), so an optional tap
  update can't fail the binary release.

## [0.2.0] — 2026-06-17

**Minor release — schema wipe (`RELEASE_MINOR=2`).** The blob and inverted-index
formats are versioned to the release minor, so the next `basemind scan` rebuilds
`.basemind/` from source. This is the intentional migration path; no user action
beyond a rescan is needed.

### Changed

- **Dependencies on crates.io.** `kreuzberg` moved from the `v5.0.0-rc.15` git pin
  to the published crates.io release `=5.0.0-rc.18`; the temporary `allow-git`
  exemptions in `deny.toml` are dropped. With no remaining git dependencies,
  `cargo publish` to crates.io is unblocked — basemind now ships on all four
  registries (crates.io, npm, PyPI, Homebrew) from a single tag.
- **README is now a positioning document.** Rewritten from a code-map feature
  inventory into a full AI-context-layer overview: four pillars (Code, Documents,
  Memory, Web), a feature table mapping every MCP tool to its backend, a
  three-flavour quickstart, comparisons against grep / vector-only RAG /
  repo-map indexers / GitHub search, and a differentiators section. Sub-package
  READMEs (npm / PyPI / OpenCode) collapse to install stubs linking the root.
- **Aligned package metadata across all surfaces.** One canonical description and
  a positioning-led 5-keyword set (`mcp`, `agent-context`, `rag`, `code-map`,
  `tree-sitter`) now ship identically across `Cargo.toml`, the npm / PyPI
  manifests, and every harness plugin manifest. GitHub repo description + topics
  updated to match.

### Added

- **Redesigned Claude Code statusline.** Bright 256-color palette (no more
  low-contrast dim text), a `◆` brand mark, a dedicated serve/scan freshness
  dot, and width-aware wide/narrow layouts. The file count now reads the L1 blob
  set directly (exact, deduped) instead of estimating from index bytes; scan
  recency reads the view index mtime; an empty repo renders an actionable
  `run: basemind scan` hint instead of a blank line. The intelligence row
  (documents / memory / web) appears when a LanceDB store is present.
- **Agent-facing skills + slash commands bundled into every plugin tree.** The
  `basemind` routing skill and `basemind-stats` dashboard, plus `/bm` and
  `/bm-stats` slash commands, now ship inside `.claude-plugin/`, `.codex-plugin/`,
  `.cursor-plugin/`, and the `basemind-opencode` npm tarball — so an installed
  plugin teaches the agent how to use the tools without manual onboarding. A new
  `scripts/sync-plugin-skills.sh` (wired as a poly hook) keeps the canonical
  source and the per-harness copies in lock-step.

## [0.1.1] — 2026-06-15

### Fixed

- **`npm-package`** — `npx basemind …` (and equivalent `npm install -g basemind`
  invocations) used to silently fall through to whatever `basemind` already lived
  on `$PATH` because npm skipped creating the `node_modules/.bin/basemind`
  symlink when the `bin:` target was missing at install time (the native binary
  is downloaded by `postinstall`). The wrapper now ships a Node.js shim at
  `bin/basemind.js` that `spawnSync`'s the downloaded native binary, so the
  symlink target exists at install time and npx routes correctly through the
  wrapper.
- **`publish.yaml`** — re-enabled the Homebrew tap step now that
  `HOMEBREW_TOKEN` is on the path to being provisioned. Set the secret on the
  basemind repo before pushing the next tag; goreleaser will then auto-update
  `Goldziher/homebrew-tap`'s `Formula/basemind.rb`.

## [0.1.0] — 2026-06-15

**Initial public release.** First minor wipes the schema (`RELEASE_MINOR=1`)
so any pre-tag `.basemind/` cache rebuilds on next scan — intentional.

Feature-complete code-map server with the kreuzberg document tier surface
(reranker, keywords, NER, summarization, language detection, TOON output) and
schema-driven config across TOML / CLI / MCP / env vars, distributed across
all major coding-agent harnesses (Claude Code, Codex, Cursor, Gemini, Factory
Droid, OpenCode, Copilot CLI, and the generic MCP path) plus npm / PyPI /
crates.io.

### Added

- **`basemind scan`** — rayon-parallel scanner that indexes a workspace into
  content-addressed msgpack blobs (`.basemind/blobs/`) plus a Fjall-backed
  inverted index (`.basemind/views/<view>/index.fjall/`). Two extraction tiers
  ship: L1 outlines (symbols, signatures, imports, docs) and L2 call sites;
  L3 structural hash available for symbol-history diffing.
- **`basemind serve`** — stdio MCP server (`rmcp`) exposing the full code-map
  and git-history tool surface (`outline`, `search_symbols`, `find_references`,
  `find_callers`, `list_files`, `dependents`, `repo_info`, `status`,
  `symbol_history`, `working_tree_status`, `recent_changes`,
  `commits_touching`, `find_commits_by_path`, `diff_file`, `diff_outline`,
  `hot_files`, `blame_file`, `blame_symbol`).
- **Dynamic 300+ language coverage** via
  [tree-sitter-language-pack](https://github.com/kreuzberg-dev/tree-sitter-language-pack).
  Hand-written `.scm` overrides ship for Rust, Python, TypeScript, TSX,
  JavaScript, Go. Other languages for which TSLP ships a vendored `tags.scm`
  (kotlin, csharp, swift, cpp, scala, solidity, lua, …) get best-effort
  symbol + call extraction via the fallback adapter that rewrites
  GitHub-standard `@definition.*` / `@reference.call` captures into
  basemind's `@symbol.*` / `@call.*` shape.
- **Schema-driven config across TOML / CLI / MCP / env vars.** Rust types in
  `src/config/` derive `schemars::JsonSchema`; the snapshot at
  `schema/basemind-config-v1.schema.json` is regenerated from those types and
  asserted by `tests/config_schema.rs`. Adding a config field lights up all
  four surfaces via `#[command(flatten)]` (clap) and `#[serde(flatten)]` (MCP
  params). Precedence: MCP > CLI > env > TOML > defaults, with per-field
  provenance tracking via `src/config/source.rs`.
- **Document tier** — `search_documents` MCP tool over Lance-backed embedding
  index with kreuzberg ingestion. Per-query overrides on every `documents.*`
  and `llm.*` setting, plus `entity_category` and `keywords_contains`
  post-filters.
- **TOON wire format** for MCP responses via
  `documents.output.format = "toon"` (or `--documents-output-format toon`).
  Round-trip parity with JSON asserted in `tests/mcp_smoke.rs`.
- **Language-aware ingestion.** `documents.language.{auto_detect,
  min_confidence, detect_multiple}` flows into kreuzberg's
  `LanguageDetectionConfig` and the chunking tokenizer. ISO 639-3 codes (e.g.
  `"fra"`) surface on `FileMapDoc.detected_languages`.
- **Cross-encoder reranker** as a post-step on `search_documents`. Off by
  default; `documents.reranker.{enabled,preset,top_k}` opts in. Preset is
  validated upfront against `kreuzberg::get_reranker_preset`; reranked index
  is bounds-checked before reorder.
- **Keyword extraction (YAKE / RAKE) and named entity recognition** at
  extract time. New tail fields on `FileMapDoc` (`keywords`, `entities`)
  with `#[serde(default)]` — blob-compatible with prior blob shapes
  (asserted by `tests/schema_bump.rs`).
- **Extractive + abstractive summarization** via `documents.summarization`.
  Abstractive routes through liter-llm with the new top-level `[llm]`
  section (model in `provider/model` form, api_key, base_url, temperature,
  timeout, retries, max_tokens). NER backend `llm` now wires the resolved
  `LlmConfig`.
- **`SecretString` newtype + `ApiKey` enum** (`Literal | Env | Unset`).
  Secrets mask to `"<redacted>"` in `Debug` / `Display` and across
  `Serialize` (including the toml→serde_json validation round-trip).
- **Real-OSS hardening harness** (`tests/harden.rs`, `./scripts/harden.sh`)
  — clones 8 upstream repos (ripgrep, tokio, typescript, react, django,
  requests, gin, ripgrep-shallow), exercises every MCP tool against each,
  and pins canary lower bounds (tokio: `find_references("spawn") >= 200`,
  django: `find_references("get") >= 200`,
  react: `search_symbols("useState") >= 20`,
  ripgrep-shallow truncation surfaces).
- **Schema sync to release version** — `RELEASE_MINOR` in `src/version.rs`
  drives both `INDEX_SCHEMA_VER` and the blob `SCHEMA_VER`. Minor-release
  bumps wipe `.basemind/` on next scan; patch releases stay compatible.
- Distribution: `brew install Goldziher/tap/basemind`,
  `npm install -g basemind`, `pip install basemind`,
  `cargo install basemind --locked`. Precompiled binaries on GitHub
  Releases for `{x86_64,aarch64}-{linux-gnu,apple-darwin}` and
  `x86_64-pc-windows-gnu`.
- **Per-harness install matrix** — manifests + skill bundle for every major
  coding-agent harness, all bumped in lock-step by
  `task release:sync-version VERSION=…`:
  - **Claude Code** — `.claude-plugin/plugin.json` + `marketplace.json` +
    `statusline.sh` (live one-line summary of the indexed map with true-color
    brand mark, freshness dot, and call/token-savings telemetry from
    `.basemind/telemetry.jsonl`). Install via
    `/plugin marketplace add Goldziher/basemind` then
    `/plugin install basemind@basemind`.
  - **Codex CLI / App** — `.codex-plugin/plugin.json` with full `interface`
    block (`displayName`, category `Developer Tools`, `capabilities`,
    `defaultPrompt`, `brandColor`). `scripts/sync-to-codex-plugin.sh` mirrors
    into a fork of the `openai/plugins` marketplace.
  - **Gemini CLI** — root-level `gemini-extension.json` with `contextFileName`
    and `mcpServers`. Install via
    `gemini extensions install https://github.com/Goldziher/basemind`.
  - **Cursor** — `.cursor-plugin/plugin.json`.
  - **OpenCode** — published as the
    [`basemind-opencode`](https://www.npmjs.com/package/basemind-opencode)
    npm package. Install via
    `{ "plugin": ["basemind-opencode@latest"] }` in `opencode.json`. Skills
    are bundled into the tarball; the plugin shim does dual-mode resolution
    (bundled-path-first, repo-root-fallback) so monorepo dev and npm install
    both work without duplication.
  - **Factory Droid / GitHub Copilot CLI** — reuse the existing
    `.claude-plugin/marketplace.json` per their published patterns.
  - Every manifest carries the same canonical user-facing description and
    crates.io-capped 5-keyword set (`mcp`, `tree-sitter`, `code-map`,
    `scanner`, `indexer`).
- **Release pipeline** — `.github/workflows/publish.yaml` publishes on
  every `v*` tag: GitHub release assets (cross-compiled binaries via
  goreleaser + zig), crates.io, npm × 2 (`basemind` binary wrapper +
  `basemind-opencode` OpenCode plugin), and PyPI. Per-registry idempotent
  via existing-version detection; OIDC-driven trusted publishers on all
  four registries.

### Changed

- `tree-sitter-language-pack`: `=1.9.0-rc.27` → `=1.9.0-rc.45`. Hierarchical
  `data_extraction` + 17 data formats; rc.40–45 are CI/codegen-only.
- `kreuzberg` bumped to the reranker / LLM API surface
  (`=5.0.0-rc.7` baseline → published rc covering reranker + LLM at publish
  time).
- `alloc-stdlib = "=0.2.2"` pin lifted (no longer binding after lock
  re-resolve; single `alloc-no-stdlib 2.0.4` in the tree).
- `src/mcp/types.rs` + `src/mcp/helpers.rs` split into `_documents.rs`
  siblings to stay under the 1000-line cap.

### Performance

- Harden 8/8 green across ripgrep / tokio / typescript / react / django /
  requests / gin / ripgrep-shallow. All canaries pass. Per-repo scan times
  within baseline: typescript 21.7 s (81 324 files), tokio 0.2 s (859
  files), django 2.5 s (7 061 files), react 2.2 s, requests 0.7 s, gin
  1.0 s, ripgrep 4.0 s, ripgrep-shallow 0.16 s. All 25 MCP tools clean
  across all repos.
- `search_documents` post-processing releases the store read-lock before
  blob I/O; `ahash::AHashMap` / `AHashSet` on the post-filter path.

[Unreleased]: https://github.com/Goldziher/basemind/compare/v0.34.0...HEAD
[0.34.0]: https://github.com/Goldziher/basemind/compare/v0.33.0...v0.34.0
[0.33.0]: https://github.com/Goldziher/basemind/compare/v0.32.0...v0.33.0
[0.32.0]: https://github.com/Goldziher/basemind/compare/v0.31.1...v0.32.0
[0.31.1]: https://github.com/Goldziher/basemind/compare/v0.31.0...v0.31.1
[0.31.0]: https://github.com/Goldziher/basemind/compare/v0.30.0...v0.31.0
[0.30.0]: https://github.com/Goldziher/basemind/compare/v0.29.3...v0.30.0
[0.29.3]: https://github.com/Goldziher/basemind/compare/v0.29.2...v0.29.3
[0.29.2]: https://github.com/Goldziher/basemind/compare/v0.29.1...v0.29.2
[0.29.1]: https://github.com/Goldziher/basemind/compare/v0.29.0...v0.29.1
[0.29.0]: https://github.com/Goldziher/basemind/compare/v0.28.0...v0.29.0
[0.28.0]: https://github.com/Goldziher/basemind/compare/v0.27.4...v0.28.0
[0.27.4]: https://github.com/Goldziher/basemind/compare/v0.27.3...v0.27.4
[0.27.3]: https://github.com/Goldziher/basemind/compare/v0.27.2...v0.27.3
[0.27.2]: https://github.com/Goldziher/basemind/compare/v0.27.1...v0.27.2
[0.27.1]: https://github.com/Goldziher/basemind/compare/v0.27.0...v0.27.1
[0.27.0]: https://github.com/Goldziher/basemind/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/Goldziher/basemind/compare/v0.25.2...v0.26.0
[0.25.2]: https://github.com/Goldziher/basemind/compare/v0.25.1...v0.25.2
[0.25.1]: https://github.com/Goldziher/basemind/compare/v0.25.0...v0.25.1
[0.25.0]: https://github.com/Goldziher/basemind/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/Goldziher/basemind/compare/v0.23.1...v0.24.0
[0.23.1]: https://github.com/Goldziher/basemind/compare/v0.23.0...v0.23.1
[0.23.0]: https://github.com/Goldziher/basemind/compare/v0.22.8...v0.23.0
[0.22.8]: https://github.com/Goldziher/basemind/compare/v0.22.7...v0.22.8
[0.22.7]: https://github.com/Goldziher/basemind/compare/v0.22.6...v0.22.7
[0.22.6]: https://github.com/Goldziher/basemind/compare/v0.22.5...v0.22.6
[0.22.5]: https://github.com/Goldziher/basemind/compare/v0.22.4...v0.22.5
[0.22.4]: https://github.com/Goldziher/basemind/compare/v0.22.3...v0.22.4
[0.22.3]: https://github.com/Goldziher/basemind/compare/v0.22.2...v0.22.3
[0.22.2]: https://github.com/Goldziher/basemind/compare/v0.22.1...v0.22.2
[0.22.1]: https://github.com/Goldziher/basemind/compare/v0.22.0...v0.22.1
[0.22.0]: https://github.com/Goldziher/basemind/compare/v0.21.1...v0.22.0
[0.21.1]: https://github.com/Goldziher/basemind/compare/v0.21.0...v0.21.1
[0.21.0]: https://github.com/Goldziher/basemind/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/Goldziher/basemind/compare/v0.19.2...v0.20.0
[0.19.2]: https://github.com/Goldziher/basemind/compare/v0.19.1...v0.19.2
[0.19.1]: https://github.com/Goldziher/basemind/compare/v0.19.0...v0.19.1
[0.19.0]: https://github.com/Goldziher/basemind/compare/v0.18.1...v0.19.0
[0.18.1]: https://github.com/Goldziher/basemind/compare/v0.18.0...v0.18.1
[0.18.0]: https://github.com/Goldziher/basemind/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/Goldziher/basemind/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/Goldziher/basemind/compare/v0.15.1...v0.16.0
[0.15.1]: https://github.com/Goldziher/basemind/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/Goldziher/basemind/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/Goldziher/basemind/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/Goldziher/basemind/compare/v0.12.2...v0.13.0
[0.12.2]: https://github.com/Goldziher/basemind/compare/v0.12.0...v0.12.2
[0.12.0]: https://github.com/Goldziher/basemind/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/Goldziher/basemind/compare/v0.10.3...v0.11.0
[0.10.3]: https://github.com/Goldziher/basemind/compare/v0.10.2...v0.10.3
[0.10.2]: https://github.com/Goldziher/basemind/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/Goldziher/basemind/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/Goldziher/basemind/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/Goldziher/basemind/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/Goldziher/basemind/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/Goldziher/basemind/compare/v0.6.3...v0.7.0
[0.6.3]: https://github.com/Goldziher/basemind/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/Goldziher/basemind/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/Goldziher/basemind/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/Goldziher/basemind/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/Goldziher/basemind/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Goldziher/basemind/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Goldziher/basemind/compare/v0.2.6...v0.3.0
[0.2.6]: https://github.com/Goldziher/basemind/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/Goldziher/basemind/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/Goldziher/basemind/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Goldziher/basemind/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Goldziher/basemind/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Goldziher/basemind/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Goldziher/basemind/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/Goldziher/basemind/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Goldziher/basemind/releases/tag/v0.1.0
