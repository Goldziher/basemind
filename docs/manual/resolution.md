# Resolution

`code_refs` finds call sites by name, then grades each one by what it is known to bind to. The
grade refines a result set; it never shrinks it. A call without `resolution` lists every call site
whose callee is the name, graded, so `total` stays a sound count of candidates.

Each call site carries `resolution`, one of:

| Value | Meaning |
|---|---|
| `same_file` | Binds to a definition in the same file. |
| `imported` | Binds through an import to a definition in another file. |
| `scope_resolved` | Binds to a local binding that is not an indexed definition: a variable, a parameter, or an import whose target is not indexed. |
| `elsewhere` | Checked against the definition pinned with `path`, and binds to another one. |
| `name_only` | No resolution facts for the site: matched by name alone. |
| `external` | No resolution facts, and the index defines no symbol of the name. |
| `path_match` | An HTTP call linked to a route by its method and normalized path. |

A page names its most common grade once as `resolution_default`, and a row leaves its own out when
it is that one; a page with no `resolution_default` is all `name_only`. `resolution_counts` gives
the rows per grade, before any filter.

- **Callers of one definition.** Add `path` (and `line` when several definitions in the file share
  the name): the answer carries that `definition`, and `resolved_total` counts the hits proven to
  bind to it. When resolution could not run, `resolved_total` is absent and `resolution_note` says
  why: `unsupported for <language>`, `no resolution facts for this file`, or
  `no resolution facts for this definition`.
- **Precision on request.** `resolution` filters rows by grade, comma-separated:
  `{"name":"open","resolution":"same_file,imported"}` (CLI: `--resolution same_file,imported`).
- **Why the default keeps unresolved rows.** Resolution cannot see through a module-object import
  (`from pkg import mod`, then `mod.f()`) or an unresolvable path alias. Dropping those rows would
  lose real callers and report the rest as complete. Filter when you want precision; trust `total`
  when you need completeness.
- **Coverage.** JavaScript and TypeScript (oxc), Python and Java (stack-graphs rules) resolve
  across files through imports; every other language gets tree-sitter scope binding.
- **JS/TS workspaces.** A bare import of a workspace package (`@acme/ui`, `@acme/ui/button`)
  resolves to its source with no `node_modules`. Packages come from the root `pnpm-workspace.yaml`
  `packages:` globs and the root `package.json` `workspaces` field, each match with a named
  `package.json`. A symlink into the repository resolves to its real path, and a literal
  `require('./x')` is a file edge.
- **From a reference to its definition.** `relation: "definition"` returns the definition the
  reference at `path` / `line` binds to, or a `resolution_note` when there is none.
- **HTTP routes.** A route is a `route` symbol named `METHOD /path`, parameters written `:param`;
  a call with a literal URL is a call site of the same shape. The callers of a route (its name, or
  `path` pinning it) are the calls it matches best, graded `path_match`: the methods agree (a route
  of `ANY` takes every method) and the paths match segment for segment, else one is a suffix of the
  other (a client base URL, or a router mounted in another file). A match through a literal segment
  beats one through parameters and wildcards alone (a catch-all such as `GET /*` never takes a call
  from the route naming its path), an exact match a suffix, a longer suffix a shorter one, and a
  literal segment a parameter. A call that several routes match equally well carries `ambiguous`:
  two services may expose the path. The callers of a handler (`path` pinning it) add the routes
  naming it and their calls, `ambiguous` for a route in another file when more than one file
  defines the name. `relation: "definition"` on a call returns its route with
  `resolution: "path_match"`, or the tied routes under `ambiguous` and no `definition`. URLs built
  from variables are out of reach.
