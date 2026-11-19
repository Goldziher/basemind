---
status: proposed
date: 2026-11-18
---

# ADR-0048: HTTP routes as symbols, client calls as call rows, linked by path

- **Deciders:** the repository owner
- **Related:** ADR-0028 (content-keyed rows), ADR-0031 (value gate), ADR-0047 (#125 resolution
  values, no new verb tool)

## Context

Issue #126 asks which handler serves an HTTP endpoint and which client code calls it. Across a
service boundary there is no import or call edge, so neither `code_symbols` nor `code_refs` can
answer it today. What exists:

- Extraction runs hand-written queries in `src/queries/<pack>.scm`, split into
  `;; section: <name>` blocks (`symbols`, `imports`, `calls`, `implementations`, `docs`). A
  language without an override falls back to its adapted TSLP `tags.scm`, but a language with an
  override reads every section from it (`query_source`, `src/lang.rs`). Overrides exist for
  JavaScript, TypeScript, TSX, Python, Rust and Go; Java has none.
- A `Symbol` carries `name`, `kind` and `signature`; a `Call` carries `callee` and its position
  (`src/extract/mod.rs`). `symbols_by_name` and `calls_by_callee` index both by name.
- `SymbolKind` maps to fixed ordinals in `symbol_kind_byte` (`src/index/keys.rs`); the last is
  `Heading` at 21. New kinds extend the tail.
- 0.34.0 rebuilds every index (ADR-0028, "Upgrade"), and the schema has room for a few arguments
  only after ADR-0047's reclaim.

## Decision

1. **A route is a symbol.** A new tail `SymbolKind::Route` (ordinal 22). Its `name` is the method
   and the normalized path, `"GET /users/:param"`, with a router mount prefix folded in when it is a
   literal in the same file. Its `signature` is the handler's name.
2. **A client call is a call row.** A call whose URL is a literal (or a template with literal
   segments) becomes a `Call` whose `callee` is `"GET /users/:param"`. Path normalization keeps
   literal segments and turns `${…}`, `{…}` and `:x` parameters into `:param` form, so both sides
   compare equal.
3. **Extraction.** Two new query sections, `;; section: routes` and `;; section: http_calls`, with
   two new `QueryKind`s, in the language overrides. Java gains its first override,
   `src/queries/java.scm`, which ports the adapted TSLP symbol, call and implementation sections
   before adding the route sections, since an override replaces the fallback for every section.
   `EXTRACT_EPOCH` (`src/extract/mod.rs`) is bumped.
4. **Scope, held for 0.34:**

   | Language | Servers | Clients |
   |---|---|---|
   | JavaScript, TypeScript | Express | `fetch`, `axios` |
   | Python | FastAPI, Flask | `requests`, `httpx` |
   | Rust | axum | `reqwest` |
   | Go | gin | `net/http` |
   | Java | Spring MVC | none |

   Other frameworks and clients go to a follow-up issue.
5. **No new keyspace and no new tool.** The symbol and call row families carry both. Queries:
   - `code_symbols {kind:"route"}` lists routes; `find_files` is unchanged.
   - `code_refs` callers of a route, named `METHOD /path` in any parameter syntax or pinned by
     `path`, are the client calls it matches best. A request belongs to the routes that match it
     best: a match through a literal segment before one through parameters and wildcards alone (so
     a catch-all such as `GET /*` never takes a request from the route naming its path), then exact
     before suffix, the longest suffix, more literal segments, and the method itself before `ANY`.
   - `code_refs` callers of a handler definition add the route rows naming it in `signature` (by
     the handler's last component: `users.show` names `show`) and the client calls linked to those
     routes. A route in another file than the definition is `ambiguous` when more than one file
     defines the name.
   - `code_refs` relation `definition` on a client call, named by its own `METHOD /path` and its
     `line`, returns the route it reaches, or the tied routes under `ambiguous`. The line is a line
     of the indexed file, so an edited file is refused, as for any other reference.
   - These rows carry the #125 value `resolution: "path_match"`, and `ambiguous` when several
     routes match. Under exact matching a client call is not a call of a name its path ends in.
   - The `code_refs` description says that URLs built from variables are out of reach.
   - Linking reads a route table built once per in-RAM map snapshot and bucketed by each route's
     last literal segment, so a request is scored only against the routes that can match it.

## Consequences

- Routes and endpoint callers are found with the tools agents already have; the schema grows by
  about 70 bytes (`route` in `code_symbols.kind` and one clause on `code_refs`), paid for by
  shortening the shared `limit`, `cursor` and `max_tokens` field docs.
- `path_match` rows add tokens to `code_refs` `callers`, which is near the 0.67 floor on tokio
  (ADR-0031). The per-row elision of ADR-0047 decision 6 applies; `callers` and `references` are
  re-measured after the linking lands.
- A path match is heuristic: two services may expose one path, and a client may call a different
  host. `ambiguous` marks the first case; the second is out of reach without configuration.
- Java's new override must reproduce what the TSLP fallback extracted; a fixture test compares
  Java's symbol and call counts before and after.
- No eval task kind exists for routes (no gold source). Extraction counts on the gin and django
  harden repositories are reported instead, so the routes do not count toward the gate.
- Fixtures per framework under `tests/fixtures/http/` and a new `tests/http_routes_smoke.rs` cover
  extraction; `tests/rust_extraction_smoke.rs` covers axum.

## Alternatives considered

- **A `routes` keyspace or a new verb tool** — rejected: no room in the schema (ADR-0047), and the
  existing name-indexed rows already answer by name.
- **A route as a call row on the server side** — rejected: a route is a definition; as a symbol it
  appears in outlines and in `code_symbols`.
- **Resolve URLs built from variables or configuration** — rejected: it needs data flow across
  files; the description states the limit instead.
- **Every framework the issue lists** — rejected for 0.34: the six server frameworks and their
  common clients are the scope the owner accepted; each further one is a query section and a
  fixture, and goes to a follow-up issue.
- **Change `query_source` so a section missing from an override falls back to TSLP** — rejected: a
  language would then read two query sources at once; the override stays the one source for its
  language.
