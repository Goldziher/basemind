# Code navigation

The code tools answer structural questions from the index and return paths, 1-based line numbers
and signatures rather than file bodies. Outline a file before opening it, then read only the
symbol or span you need. Paths are repository-relative with forward slashes.

| Question | MCP call | CLI |
|---|---|---|
| What is in this file? | `code_outline {"path":"src/store.rs"}` | `basemind code outline src/store.rs` |
| Where is X defined? | `code_symbols {"name":"open_store"}` | `basemind code symbols open_store` |
| Show one symbol's body | `code_read {"path":"src/store.rs","symbol":"open"}` | `basemind code read src/store.rs --symbol open` |
| Show a line span | `code_read {"path":"src/store.rs","line":120,"end_line":160}` | `basemind code read src/store.rs --line 120 --end-line 160` |
| Where does this text appear? | `code_grep {"pattern":"cache_root("}` | `basemind code grep "cache_root("` |
| Find code by what it does | `code_search {"query":"retry with backoff"}` | `basemind code search "retry with backoff"` |
| What calls X? | `code_refs {"name":"open_store"}` | `basemind code refs open_store` |
| What calls this one definition? | `code_refs {"name":"open","path":"src/store.rs"}` | `basemind code callers src/store.rs open` |
| What implements X? | `code_refs {"name":"Drawable","relation":"implementations"}` | `basemind code implementations Drawable` |
| What imports module M? | `code_refs {"name":"tokio::sync","relation":"importers"}` | `basemind code dependents tokio::sync` |
| Which files import this file? | `code_refs {"relation":"importers","path":"src/store.rs","depth":2}` | `basemind code refs --relation importers --path src/store.rs --depth 2` |
| Which import cycles are there? | `code_refs {"relation":"cycles"}` | `basemind code refs --relation cycles` |
| Which definition does this bind to? | `code_refs {"name":"open","relation":"definition","path":"src/main.rs","line":42}` | `basemind code definition src/main.rs 42 open` |
| Which HTTP calls reach this route? | `code_refs {"name":"GET /users/:param"}` | `basemind code refs "GET /users/:param"` |
| Which file is named like X? | `find_files {"query":"readme"}` | `basemind code find readme` |
| List files under a directory | `find_files {"path_prefix":"src/mcp/"}` | `basemind code files --path-prefix src/mcp/` |

## Contracts

- `code_outline` lists a code file's symbols (kind, name, signature, `line` / `end_line`) and its
  imports. A Markdown, config or data file is a document, not code: the call says so and points at
  `docs_search`.
- `code_symbols` ranks the exact name first, then prefix, then substring; `fuzzy` adds fuzzy
  subsequence matches last. `kind` filters by symbol kind.
- `code_read` takes `symbol` (plus `line` or `kind` when several definitions share the name) or a
  `line`..`end_line` span, not both. A body over 128 KiB is cut at a line end and flagged
  `truncated`: read on from `end_line` + 1. `stale` means the file changed since it was indexed
  and was parsed as it is now.
- `code_grep` matches `pattern` literally unless `regex` is true; a regex anchors `^` and `$` at
  every line. `path` is a literal substring of the path, never a glob. A `path` or `language` that
  no indexed file has is an error, not an empty result. Each hit is its line `text` plus the
  enclosing `symbol`.
- `code_search` returns pointers (path, `line` / `end_line`, symbol), not bodies. It reports the
  lanes that ran and `lanes_unavailable` for the ones that did not. It needs a build with the
  `code-search` feature; without it the call is an error naming the feature.
- `code_refs` matches the whole identifier: `bar` finds `Foo::bar()` and `bar()`, not `bar_baz()`.
  `match: "substring"` widens it. Each call site carries a `resolution` grade; see
  `basemind manual resolution`. `ranges` answers the call sites as each file's merged read spans
  (`{"<path>": {"ranges": [[start, end], ...], "hits": n}}`, every site widened by 5 lines), to pass
  to `code_read` `ranges`.
- `code_refs` with a `METHOD /path` name (`GET /users/:param`, any parameter syntax) lists the
  HTTP calls with a literal URL that reach that route, graded `path_match`; with `path` pinning a
  route symbol it lists that route's calls. `relation: "definition"` with the call's own
  `METHOD /path` (its row's `callee`) and its `line` returns the route it reaches. A URL built from
  variables is out of reach.
- `code_refs` `importers` with `path` and no `name` walks the file-level import graph: the files
  importing that file, grouped `by_depth`, `depth` hops out (default 1, at most 5). `cycles` lists
  the import cycles, smallest first; with `path`, only that file's own. Both report per-language
  coverage, and a language without an import resolver is reported as not analysed, never as having
  no importers or no cycles. Type-only imports and Rust `mod` declarations are left out.
- `find_files` covers code, documents and every other file git tracks; a query with one typo still
  finds its file. `min_lines` lists code files of at least that many lines, largest first.
- `changed` on `code_grep`, `code_symbols`, `code_refs` (call sites) and `find_files` keeps only
  the files changed since a revision, diffed against the working tree (`""` means `HEAD`: the
  uncommitted changes), or between `A..B`. Append `+importers` to add the files importing them.
  The response counts the files in scope as `scope_files`; an empty scope is a result, an unknown
  revision an error.

## Failures name their fix

An argument a tool does not take is refused with a did-you-mean and the accepted list. A failure
the caller can fix is a result with `isError` whose text says what to do: a document path points at
`docs_search`, an unindexed file at `index_rescan`, a missing file at `find_files`, a name several
definitions share lists each of them so you can add `line`.

## CLI-only analyses

These read the index (the daemon answers them when one runs) and have no MCP tool:

- `basemind code impact <target>` lists the files that depend on a file (`path` or `path:line`), on
  a symbol's defining files (`--symbol`), or on a change (`--diff <rev>`, `--diff A..B`,
  `--changed` for the uncommitted changes), transitively through the import graph up to
  `--depth` levels (default 1, at most 5), each with its depth and whether it sits in a cycle.
- `basemind code cycles` lists import cycles, smallest first, each with an import chain that closes
  it; `--path` narrows to a file or directory and `--max-size` drops larger cycles.
  `basemind code cycles --check <from> <to>` says whether `from` importing `to` would close a cycle.
  Both commands take `--include-type-imports` and `--include-module-decls` to follow type-only
  imports and Rust `mod` declarations too.
- `basemind code migration check [name]` counts the remaining uses of the old forms each rule of
  `.basemind/migrations/<name>.toml` names (a `call`, `import`, `identifier` or `text` pattern),
  per rule and file, with the share migrated and the progress across runs; the history is kept in
  the workspace cache directory, never the repository. `--fail-on-remaining` exits 1 while an old
  form remains, for CI; `--ranges` gives each file's uses as spans for `code read --ranges`.
- `basemind code chunk` fetches one semantic-search chunk body by path.
