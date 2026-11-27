//! Long help (`--help`, after the options) of the `basemind code` and `basemind git` commands.
//!
//! Each text has the same four parts: `When to use:`, `Returns:`, an `Example:` command line and
//! `Next:`. `tests/cli_parity/help.rs` walks the command tree and fails on a command without them
//! or on an example or a quoted command the CLI does not accept. `-h` stays the one-line summary.

/// `basemind code outline`.
pub const CODE_OUTLINE: &str = "\
When to use: before opening a file, to learn what it defines and where.

Returns: the file's symbols (kind, name, signature, 1-based `line` / `end_line`) and its imports.
A Markdown, config or data file is a document: the answer says so and points at document search.

Example:
  basemind code outline src/main.rs

Next: `basemind code read <path> --symbol <name>` for one body.";

/// `basemind code symbols`.
pub const CODE_SYMBOLS: &str = "\
When to use: you know a name and need its definition, without grepping.

Returns: matching definitions with path, line span, kind and signature, the exact name first,
then prefix and substring matches; `--fuzzy` adds subsequence matches last.

Example:
  basemind code symbols open_store --kind function

Next: `basemind code read <path> --symbol <name>` for the body, `basemind code refs <name>` for
its call sites.";

/// `basemind code read`.
pub const CODE_READ: &str = "\
When to use: you need one symbol's source or a line span, not the whole file.

Returns: the body of `--symbol` (add `--line` or `--kind` when several definitions share the
name), or the lines `--line`..`--end-line`. A body over 128 KiB is cut at a line end and flagged
`truncated`; `stale` means the file changed since the scan and was read as it is now.

Example:
  basemind code read src/main.rs --symbol main

Next: `basemind code refs <name>` for its call sites, `basemind git blame <path> --symbol <name>`
for who changed it.";

/// `basemind code grep`.
pub const CODE_GREP: &str = "\
When to use: you are looking for exact text: an identifier, a string literal, an error message.

Returns: each hit as `<line>: <text>` under its `# <path>` and enclosing `## <symbol>` headers.
The pattern is literal unless `--regex`; `--path` is a substring of the path, never a glob;
`--context N` (0-10) adds the lines around each hit.

Example:
  basemind code grep \"cache_root(\" --path src/

Next: `basemind code read <path> --line <n>` around a hit.";

/// `basemind code search`.
pub const CODE_SEARCH: &str = "\
When to use: you know what the code does but not what it is called.

Returns: pointers (path, line span, symbol), not bodies, ranked by meaning and by words, and the
lanes that ran. Needs a build with the `code-search` feature.

Example:
  basemind code search \"retry with backoff\"

Next: `basemind code read <path> --line <n> --end-line <m>` for a hit.";

/// `basemind code refs`.
pub const CODE_REFS: &str = "\
When to use: you need the call sites, implementations or importers of a name, or a file's
importers and the import cycles.

Returns: rows matching the whole identifier (`--match substring` widens it), each call site with
its `resolution` grade. `--relation importers --path <file>` walks the file import graph and
`--relation cycles` lists import cycles; neither takes a name.

Example:
  basemind code refs open_store

Next: `basemind code read <path> --line <n>` for a site, `basemind manual resolution` for the
grades.";

/// `basemind code callers`.
pub const CODE_CALLERS: &str = "\
When to use: several definitions share a name and you want the callers of the one in a file.

Returns: the call sites of that definition with their resolution grades, the same rows as
`code refs <name> --path <path>`.

Example:
  basemind code callers src/store.rs open

Next: `basemind code explain <path>:<line>` for a briefing on a caller.";

/// `basemind code implementations`.
pub const CODE_IMPLEMENTATIONS: &str = "\
When to use: you need the types that implement a trait, interface or base class.

Returns: each implementing type with its path and line, the same rows as
`code refs <name> --relation implementations`.

Example:
  basemind code implementations Drawable

Next: `basemind code outline <path>` on an implementing file.";

/// `basemind code dependents`.
pub const CODE_DEPENDENTS: &str = "\
When to use: you need the files that import a module by its import name.

Returns: the importing files, the same rows as `code refs <module> --relation importers`.

Example:
  basemind code dependents tokio::sync

Next: `basemind code impact <path>` for everything that depends on a file, transitively.";

/// `basemind code definition`.
pub const CODE_DEFINITION: &str = "\
When to use: you are at a reference and need the definition it binds to (go to definition).

Returns: the definition's path and line span; `--column` picks the occurrence when the name appears
more than once on the line.

Example:
  basemind code definition src/main.rs 42 open

Next: `basemind code read <path> --symbol <name>` on the definition.";

/// `basemind code find`.
pub const CODE_FIND: &str = "\
When to use: you know part of a file's name or path but not where it lives.

Returns: matching paths, code and documents alike, a query with one typo included. With no query
it lists indexed files; `--min-lines` lists code files of at least that many lines, largest first.
`--indexed true|false|any` picks files whose contents are indexed, list-only files
(lockfiles, archives), or both.

Example:
  basemind code find readme

Next: `basemind code outline <path>` on a code file.";

/// `basemind code files`.
pub const CODE_FILES: &str = "\
When to use: you want the indexed files under a directory or of one language.

Returns: the indexed paths, the same rows as `code find` with no query.

Example:
  basemind code files --path src/mcp/

Next: `basemind code outline <path>` on a file.";

/// `basemind code chunk`.
pub const CODE_CHUNK: &str = "\
When to use: you have a `code search` hit and want the indexed chunk it ranked, not the file.

Returns: the chunk's text. With neither `--chunk-id` nor `--byte-start`, a file with one chunk
prints it and a file with several lists their ids.

Example:
  basemind code chunk src/store.rs --byte-start 0

Next: `basemind code read <path> --line <n>` for the surrounding lines.";

/// `basemind code impact`.
pub const CODE_IMPACT: &str = "\
When to use: before changing a file, a symbol or a branch, to see what depends on it.

Returns: the files that import the target, transitively up to `--depth` levels (default 1, at most
5), each with its depth and whether it sits in an import cycle. The target is a file, a symbol
(`--symbol`), a change (`--diff <rev>`, `--diff A..B`) or the uncommitted changes (`--changed`).
Each impacted file is bucketed prod, config, test or external with counts, and `order` says what to
change first. `--fail-above <n>` exits 1 when more than `n` files are impacted.

Example:
  basemind code impact src/store.rs --depth 2

Next: `basemind code explain <path>` on a dependent file.";

/// `basemind code cycles`.
pub const CODE_CYCLES: &str = "\
When to use: auditing the import graph, or checking that a new import would not close a cycle.

Returns: import cycles, smallest first, each with an import chain that closes it; with
`--check <from> <to>`, whether `from` importing `to` would close one, and the chain it would close.

Example:
  basemind code cycles --path src/engine/

Next: `basemind code explain <path>` on a file in a cycle.";

/// `basemind code explain`.
pub const CODE_EXPLAIN: &str = "\
When to use: you need to understand one symbol or file in a single call.

Returns: for a symbol, its definition, doc, body (60 lines or fewer), callers, callees, importers
and last commits; for a file, its outline, imports, importers, calls in and out, hotspot rank,
last commits and owners. The answer stays within `--max-tokens` (default 6000) and lists the
sections it cut with the call that continues each.

Example:
  basemind code explain src/store.rs

Next: `basemind code read <path> --symbol <name>` for a body the briefing left out.";

/// `basemind code deadcode`.
pub const CODE_DEADCODE: &str = "\
When to use: looking for files, definitions or imports nothing uses, before deleting them.

Returns: files no entry point reaches, symbols nothing else names and imports that resolve to
nothing, each with a confidence tier (`high`, `medium`, `low`) and its reason, plus coverage per
language. `--fail-on-dead-code` exits 1 for CI.

Example:
  basemind code deadcode --tier high

Next: `basemind code refs <name>` to confirm a finding before deleting it.";

/// `basemind code migration check`.
pub const CODE_MIGRATION_CHECK: &str = "\
When to use: tracking a migration away from old calls, imports or identifiers.

Returns: per rule of `.basemind/migrations/<name>.toml` and per file, the remaining uses of the
old forms, the share migrated and the progress across runs. `--fail-on-remaining` exits 1 for CI.

Example:
  basemind code migration check

Next: `basemind code read <path> --line <n>` on a remaining use.";

/// `basemind git log`.
pub const GIT_LOG: &str = "\
When to use: you need history: recent commits, a path's commits, a message or author search, a
symbol's history, or one commit in full.

Returns: commits (sha, date, author, subject) newest first; `--rev` gives one commit with its full
message and changed files. A symbol's or a directory's history pages by window: follow
`next_cursor` until it is absent.

Example:
  basemind git log --path src/store.rs --symbol open

Next: `basemind git diff <path> --from <rev>` for what a commit changed.";

/// `basemind git blame`.
pub const GIT_BLAME: &str = "\
When to use: you need who last changed lines of a file, and in which commit.

Returns: blame hunks (lines, commit, author, date) for the file, a 1-based span (`--line`,
`--end-line`) or one symbol (`--symbol`), located as the file is at `--rev` (default HEAD).

Example:
  basemind git blame src/store.rs --line 120 --end-line 160

Next: `basemind git log --rev <sha>` for a hunk's commit.";

/// `basemind git diff`.
pub const GIT_DIFF: &str = "\
When to use: you need what is dirty in the working tree, or what changed in a file.

Returns: with no path, the working-tree status; with a path, its text hunks from `--from`
(default HEAD) to `--to` (default the working tree), or with `--structural` the symbols added or
removed. A diff stops at 200 hunks or 128 KiB and sets `truncated`.

Example:
  basemind git diff src/store.rs --from HEAD~1

Next: `basemind code read <path> --symbol <name>` on a changed symbol.";

/// `basemind git status`.
pub const GIT_STATUS: &str = "\
When to use: you need the staged, modified and untracked files.

Returns: the working-tree status, the same as `git diff` with no arguments.

Example:
  basemind git status

Next: `basemind git diff <path>` on a changed file.";

/// `basemind git recent`.
pub const GIT_RECENT: &str = "\
When to use: you want the latest commits.

Returns: recent commits newest first, the same as `git log` with no filter.

Example:
  basemind git recent --limit 10

Next: `basemind git show <rev>` for one commit in full.";

/// `basemind git touching`.
pub const GIT_TOUCHING: &str = "\
When to use: you want the commits that changed a file or, with a trailing `/`, a directory.

Returns: those commits newest first, the same as `git log --path <path>`.

Example:
  basemind git touching src/store.rs

Next: `basemind git show <rev>` for one commit in full.";

/// `basemind git search`.
pub const GIT_SEARCH: &str = "\
When to use: you remember words of a commit message, however far back.

Returns: commits whose message matches, the same as `git log --query <query>`.

Example:
  basemind git search \"fix race\"

Next: `basemind git show <rev>` for a match.";

/// `basemind git show`.
pub const GIT_SHOW: &str = "\
When to use: you have a commit and want its full message and changed files.

Returns: the commit, the same as `git log --rev <rev>`.

Example:
  basemind git show HEAD~1

Next: `basemind git diff <path> --from <rev>~1 --to <rev>` for a file it changed.";

/// `basemind git symbol-history`.
pub const GIT_SYMBOL_HISTORY: &str = "\
When to use: you want the commits that changed one function's or type's body.

Returns: those commits newest first, paged by window, the same as
`git log --path <path> --symbol <name>`.

Example:
  basemind git symbol-history src/store.rs open

Next: `basemind git show <rev>` for one of them.";

/// `basemind git blame-symbol`.
pub const GIT_BLAME_SYMBOL: &str = "\
When to use: you want who last changed each line of one symbol.

Returns: blame hunks for the symbol's line span, the same as `git blame <path> --symbol <name>`.

Example:
  basemind git blame-symbol src/store.rs open

Next: `basemind git show <rev>` for a hunk's commit.";

/// `basemind git diff-outline`.
pub const GIT_DIFF_OUTLINE: &str = "\
When to use: you want which symbols a file gained or lost since a revision.

Returns: the symbol-level diff of the file against the indexed working tree, the same as
`git diff <path> --structural --from <rev>`.

Example:
  basemind git diff-outline src/store.rs --rev HEAD~5

Next: `basemind code read <path> --symbol <name>` on an added symbol.";

/// `basemind git churn`.
pub const GIT_CHURN: &str = "\
When to use: you want the files that change most often.

Returns: the top `--top-k` files (default 20) by commit count over the last `--window` commits
(default 200).

Example:
  basemind git churn --top-k 10

Next: `basemind git hotspots` to weigh change against size.";

/// `basemind git hotspots`.
pub const GIT_HOTSPOTS: &str = "\
When to use: looking for the files where frequent change meets size, the likeliest to need care.

Returns: per file, the commits and authors in the `--since` window (default `90d`), lines,
symbols, the longest function and importers, listing files with two or more of these in the top
10% of the files changed in the window. Raw numbers, no score.

Example:
  basemind git hotspots --since 30d

Next: `basemind code explain <path>` on a hotspot.";

/// `basemind git by-path`.
pub const GIT_BY_PATH: &str = "\
When to use: you want the commits that touched any path matching a pattern.

Returns: commits, newest first, whose changed paths match the regex, within the last `--window`
commits (default 200).

Example:
  basemind git by-path \"^src/mcp/\"

Next: `basemind git show <rev>` for one of them.";
