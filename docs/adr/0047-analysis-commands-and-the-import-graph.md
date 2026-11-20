---
status: proposed
date: 2026-11-18
amends: ["0018", "0022"]
---

# ADR-0047: Analyses ship as daemon-answered CLI commands; the file-level import graph

- **Deciders:** the repository owner
- **Related:** amends ADR-0018 (how a capability enters the surface, the `code_refs` row shape) and
  ADR-0022 (decision 4's tool table, contract 3's byte ceiling, the value gate); ADR-0017,
  ADR-0028, ADR-0031, ADR-0039, ADR-0048, ADR-0049

## Context

Release 0.34.0 takes every open issue from #120 to #135. Eight of them ask for an analysis, some as
"a command and a deferred tool": #120 explain, #121 impact, #123 dead code, #130 migration, #131
overview, #133 manual, #134 hotspots and #135 cycles. Facts on `main` that constrain the answer:

- **The schema has no room.** `tests/fixtures/tools_list.json` serializes the fifteen verb tools to
  19,991 bytes against the 20,000-byte ceiling (`VERB_TOOLS_BYTE_CEILING`,
  `tests/mcp_schema_wire.rs`). A new verb tool costs about 900 bytes; no argument fits either.
- **The schema carries bytes that say nothing.** `compact_schema` (`src/mcp/tool_contract.rs`)
  drops the `null` type alternative of an optional field but keeps its `"default": null`: 75
  occurrences, about 1,125 bytes. Nine `max_tokens` docs repeat "Token budget for the list (default
  6000).", and some field docs hold `\n`.
- **CLI-only commands already reach the daemon by name** (ADR-0039 rule 4): `CLI_ONLY` and
  `cli_only_call` in `src/mcp/server_handler.rs`, with a `cli_only(...)` row per command in
  `tests/cli_parity/capabilities.rs`.
- **The value gate has run** (`benchmarks/eval/results/gate.md`, ADR-0031). `code_refs` `callers`
  on tokio measures 0.68x against the 0.67 floor for locating kinds, so bytes added to its rows can
  fail it.
- **There is no file-level import graph.** `import_targets` holds per-binding cross-file rows and a
  probe manifest at `PROBE_MANIFEST_START` (`src/index/keys_probe.rs`). Specifiers resolve only for
  JS/TS, Python and Java (`SpecifierResolver::for_language`, `src/intel/resolver.rs`), so both eval
  corpora (basemind and tokio, both Rust) and the gin harden repository have no edges. Impact,
  cycles, dead files, the overview's most-imported files and hotspots' importer counts all need
  those edges.
- **A reference does not say how it was resolved.** `ReferenceHit.resolved: Option<bool>`
  (`src/engine/types/refs.rs`) is set only when `path` pins a definition; name scans carry the
  constant `NAME_ONLY` note.
- **0.34.0 rebuilds every index anyway** (ADR-0028, "Upgrade"), so a changed row costs no migration
  if it lands before the tag.

## Decision

### 1. Analyses are CLI commands first

The eight analyses ship as CLI commands, each with one engine op and no verb tool:

| Issue | Command | Engine op |
|---|---|---|
| #120 | `basemind code explain <target>` | `src/engine/ops/explain.rs` |
| #121 | `basemind code impact <target>` | `src/engine/ops/impact.rs` |
| #123 | `basemind code deadcode` | `src/engine/ops/deadcode.rs` |
| #130 | `basemind code migration check <name>` | `src/engine/ops/migration.rs` |
| #131 | `basemind overview` | `src/engine/ops/overview.rs` |
| #133 | `basemind manual [topic]` | none: pages compiled in from `docs/manual/` |
| #134 | `basemind git hotspots` | `src/engine/git/hotspots.rs` |
| #135 | `basemind code cycles [path]` | `src/engine/ops/cycles.rs` |

- Every command that reads the index is answered by the daemon: it is a `CLI_ONLY` entry with a
  dispatch arm, and a `cli_only(...)` parity row. `manual` reads no index and prints in the CLI
  process. `basemind sessions` (#128) runs locally under ADR-0049.
- **Promotion.** A command becomes a verb tool only when it has an eval task kind (`EvalMode` in
  `src/eval/task.rs` with a gold generator) and that kind passes the ADR-0031 rule for its job, on
  both corpora. Promotion happens in a surface-breaking release and pays for its bytes within the
  ceiling. #120 gains an `Explain` kind (reducing) in 0.34 so the next surface release can decide.

### 2. MCP gains only arguments on existing tools

No verb tool is added in 0.34; ADR-0022's table of fifteen stands. The MCP parts of the issues are
these arguments and fields:

| Tool | Added | Issue |
|---|---|---|
| `code_refs` | `resolution` filter; per-row `resolution` | #125 |
| `code_refs` | `relation` gains `cycles`; `depth` (with `importers` and `path`); `name` optional, its shapes enforced in the body | #121, #135 |
| `code_refs` | `changed`; route linking (`path_match`) | #122, ADR-0048 |
| `code_grep` | `changed` | #122 |
| `code_symbols` | `changed`; `kind` value `route`; `path` and `language` | #122, ADR-0048, #252 |
| `find_files` | `changed`; `min_lines` | #122, #127 |
| `code_search` | `changed`; `path` and `language` | #122, #254, #252 |
| `index_status` | response fields `large_files`, `most_imported`, `deps_unavailable` (no argument) | #127, #131, ADR-0049 |

`docs_search` gains nothing: dependency documents are filtered by its existing `path` (ADR-0049).

**One path filter (#252).** `path` is the one path filter of `code_grep`, `find_files`,
`docs_search`, `code_symbols` and `code_search`: a repo-relative prefix (`src/mcp/`), or a glob over
the whole path when it holds `*` (`*` stays within a segment, `**` crosses `/`). It replaces
`code_grep`'s substring, `find_files`' `path_prefix` and `docs_search`'s glob-only `path`.
`docs_search`'s `kind` filter is named `format`, and `memory_search` takes a `tags` list, as
`memory_save` does, where it took one `tag`. A `path` or `language` that no indexed file passes is
refused.

### 3. Schema bytes are reclaimed, the ceiling stays

- `compact_schema` also removes `"default": null` from every optional property.
- Every `max_tokens` doc is one short clause, and field docs hold no `\n`.
- The byte count is measured after every merge. If an addition would cross 20,000 bytes, the cuts
  are, in order, `changed` on `code_search`, then the `resolution` filter (the per-row value stays).
  Removing `ranges` (ADR-0053) frees the bytes that `changed` on `code_search` costs.

### 4. The file-level import edge row

- **Row.** One row per importer in `import_targets`, under the importer's prefix at the reserved
  `local_start = u32::MAX - 1`, beside `PROBE_MANIFEST_START`. Its value holds the deduplicated
  first-hop targets with the offset of their first import, a per-edge kind byte, the counts of
  external and unresolved imports, and the unresolved specifiers (capped). No keyspace is added.
  The value is `external:u32 ‖ unresolved:u32 ‖ edges:u32 ‖ edge* ‖ specifier*`, an edge being
  `kinds:u8 ‖ u16:len ‖ target ‖ local_start:u32` and a specifier `u16:len ‖ bytes` (big-endian).
- **Edge kinds.** `kinds` is a bit set (`EdgeKinds`, `src/index/keys_probe.rs`) of every kind of
  import behind the edge: `import` (binds a name: a JS named or default import, a Rust `use`, a Go
  import), `type_only` (`import type`), `module_only` (a JS side-effect import, namespace import
  or `export … from` re-export: runtime, binds no export) and `module_decl` (a Rust `mod x;`: the
  parent module contains the child file). An edge with `import` or `module_only` is a runtime
  edge; `local_start` points at its first runtime import, else its first declaration, else its
  first type-only import. An unknown bit or an empty set makes the row malformed.
- **Writer.** `stitch_cross_file_edges` (`src/intel/xfile.rs`) writes it once a target path is
  known and before export matching, so barrel files, side-effect imports and unresolved imports are
  counted. The row is deleted with the importer prefix, so invalidation is inherited.
  Every change to the row bumps `IMPORT_PROBE_VER` (`src/index/mod.rs`), which forces one
  wholesale pass; the kind byte took it to 5.
- **Rust and Go resolvers,** file level only (no bindings, no facts), in new
  `src/intel/module_resolver.rs`, `module_rust.rs` and `module_go.rs`, hooked into
  `src/intel/resolve_pass.rs`:
  - Rust: `mod x;` maps to `x.rs` or `x/mod.rs` beside the importer; `use crate::…` to the longest
    existing module file under the crate root of the nearest `Cargo.toml`; `super::` and `self::`
    are relative.
  - Rust paths that start at a crate name are classified by the package's `Cargo.toml`:
    `std`, `core`, `alloc`, `proc_macro`, `test` and registry dependencies (all dependency
    tables, by their code name) are external. The package's own library (from its tests,
    binaries, examples), a path dependency inside the repository, a `workspace = true` dependency
    whose `[workspace.dependencies]` entry has a path and a dependency the workspace `[patch]`es to
    a path are crates of the repository: the path resolves like `crate::…` in that crate, so a
    workspace member is an internal edge, not a separate bucket. A path through a name the file
    binds (its own item, or a name another `use` brought in) is no edge and is not counted. Any
    other name is unresolved: it is no crate the manifest declares, so the resolver failed to
    place it. Without a `Cargo.toml` such a path is external.
  - Go: an import path under the `go.mod` module prefix maps to the package directory, one edge per
    non-test `.go` file in it. Build tags are ignored and the coverage note says so.
- **Two tiers are left out by default.** Type-only imports and Rust `mod` declarations are excluded
  from every graph answer (cycles, importer and dependency walks, in-degree, reachability) unless
  the caller asks: the CLI flags `--include-type-imports` and `--include-module-decls`. No MCP
  argument selects them. An edge that also holds a runtime import is always in. A `mod`
  declaration is containment, not use: with it, a child's `use crate::…` closes a cycle through
  every parent above it, which on this repository merged 392 files into one component.

### 5. The graph is built at query time

- `src/engine/import_graph.rs` prefix-scans the edge rows through a `file_edges()` accessor on
  `IndexView` (`src/index/view.rs`), interns paths and builds forward and reverse adjacency. It is
  cached on `Workspace` keyed by `cache_generation`, within a byte budget.
- Algorithms: iterative Tarjan for cycles; a bounded reverse walk for importers (depth default 1,
  at most 5, at most 5,000 nodes, cut as `truncated_reason` `depth` or `scan_cap`); a forward walk;
  in-degree; reachability from a set of entry points.
- A graph answer is refused or carries a notice until `import_probes_complete()`.
- Every answer reports coverage: per language the resolved, external and unresolved counts, and
  the languages that have no resolver. A repository with no resolvable language answers "no edges:
  no resolver for X", never "no cycles".
- Transitive caller walks (#120, #121) follow resolved edges only; a `name_only` edge stops at one
  hop and is marked. This keeps out the name-only graph ADR-0017 removed.

### 6. Each reference says how it was resolved (#125)

- `ReferenceHit.resolved` is replaced by `resolution`, computed at query time, on every row:

  | Value | Meaning |
  |---|---|
  | `same_file` | binds to a definition in the same file |
  | `imported` | binds through an import to a definition in another file |
  | `scope_resolved` | binds through stack-graph or oxc scope resolution, not an import |
  | `elsewhere` | checked; binds to another definition than the one asked for |
  | `name_only` | no resolution facts for that file or language |
  | `external` | no definition of the name in the index |

  ADR-0048 adds `path_match`. `elsewhere` and `name_only` stay apart: one is a checked negative,
  the other an absence of evidence.

- A row writes `resolution` only when it differs from the page's `resolution_default`, its most
  common value. The response adds `resolution_counts`, and the free-text response field
  `resolution` becomes `resolution_note`.

- The request filter `resolution` takes one value or a comma list.

- Precision under the filter is reported by the eval, not gated (ADR-0031 rule 10).

### 7. Queries scoped to a change (#122)

- `changed` takes a rev, a range `A..B` or a range `A...B` (#254):
  - a bare rev is its merge base with `HEAD` diffed against the working tree, plus untracked files:
    a branch's own changes, committed or not, without what the rev gained since (`""` means `HEAD`);
  - `A..B` diffs the two trees;
  - `A...B` diffs the merge base of `A` and `B` against `B`: what `B` changed since it left `A`.

  An empty side of a range is `HEAD`, as in git.

  The suffix `+importers` adds the depth-1 importers of every changed file through the graph.
- The scope is a path predicate in each op's existing filter step (`src/engine/filters.rs`), and a
  post-filter on `code_search`. The response adds `scope_files`. An empty scope answers
  `scope_files: 0` and is not an error; an unknown rev is refused.

## Consequences

- No analysis costs schema bytes in 0.34, and none is advertised to agents through `tools/list`; an
  agent reaches them through the shell and the skills. Each is a candidate for a verb tool with a
  measured case.
- Each new command adds a `CLI_ONLY` arm and a parity row. W0d moves the dispatch to
  `src/mcp/cli_only.rs` so these appends stay small.
- The row elision in decision 6 is mandatory: without it `callers` on tokio falls under the 0.67
  floor. `callers` and `references` are re-measured after #125 and after the route linking.
- `resolved` disappears from `code_refs` rows: a breaking change to the answer, taken in 0.34 with
  the rest of the surface.
- The import graph is as good as the resolvers. Rust `pub use` re-exports and `#[path]` modules
  under-resolve; an unresolved import is counted, not guessed. A language without a resolver is
  reported as not analysed by every command that uses the graph.
- The rebuild of 0.34.0 covers the new row and the `IMPORT_PROBE_VER` bump.
- `src/intel/resolver.rs` is at 982 lines, so the new resolvers live in their own files.

## Alternatives considered

- **New verb tools for the analyses** — rejected: each costs about 900 bytes in every session, there
  are not enough, and ADR-0022 requires a task kind and a measured win first.
- **Raise the byte ceiling** — rejected: the ceiling is the cost every session pays; reclaiming
  bytes that say nothing is free.
- **One mode-dispatched `analyze` tool** — rejected: ADR-0018 removed mode dispatch for the verb
  surface.
- **The full overview on `index_status`** — rejected: it would bloat every health check. Only
  `most_imported` and `large_files` ride along.
- **A stored import graph in its own keyspace** — rejected: ADR-0028 keeps cross-file rows under the
  view's path keys, and the row beside the probe manifest inherits their invalidation; the graph is
  cheap to build per cache generation.
- **A stored resolution column** — rejected: resolution follows from facts already stored, and a
  column would go stale with every cross-file change.
- **Merging `elsewhere` into `name_only`, as #125 proposed** — rejected: a checked negative and a
  missing check need different follow-ups.
- **A second field for change expansion on five tools** — rejected: the `+importers` suffix costs no
  schema bytes.
- **Type-only imports in the default graph** — rejected: they create cycles that do not exist at
  run time.
- **`mod` declarations in the default graph** — rejected: every Rust parent-child pair plus any
  `use crate::…` back to an ancestor is a cycle, so the cycle report names the module tree, not
  dependencies.
- **One kind per edge instead of a bit set** — rejected: a parent that declares a module and also
  `use`s it, or a JS file importing one target both at runtime and by type, would lose the
  weaker kind; the set keeps both and the tier test stays one mask.
- **A separate `workspace` count for Rust workspace crates** — rejected: they are files of the
  repository, so resolving them to the member crate's module files gives real edges.
