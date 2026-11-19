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
- **From a reference to its definition.** `relation: "definition"` returns the definition the
  reference at `path` / `line` binds to, or a `resolution_note` when there is none.
