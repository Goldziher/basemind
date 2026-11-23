---
status: accepted
date: 2026-11-23
amends: ["0047"]
---

# ADR-0056: `code impact`: callers graded by resolution, and downstream dependencies

- **Deciders:** the repository owner
- **Related:** amends ADR-0047 (the analysis commands; the importer view of `code impact`, §1 and
  §4-5); ADR-0031 and ADR-0051 (value gate), ADR-0048 (`path_match`)

## Context

ADR-0047 ships `code impact` at file level: the files that import the target, transitively. A symbol
target yields only the importers of its defining files. Before an edit an agent also needs the call
sites that reach the symbol, how far each can be trusted, and what the target itself depends on.
`code_refs` already grades every call site by how it was resolved (ADR-0047 §1) and `code explain`
walks callers over proven edges.

## Decision

1. **Callers.** For a symbol target `code impact` looks up the call sites of each definition through
   the pinned `code_refs` callers read, then of the callable definition holding each proven site, up
   to `--depth` hops. The walk looks up at most 256 definitions and lists at most 500 callers; the
   answer says which bound stopped it (`callers_truncated_reason`: `depth`, `limit`, `scan_cap`).
2. **Grade.** A caller row carries `resolution` (the `code_refs` value) and `proven`. Only
   `same_file` and `imported` are proven. `name_only` and `path_match` rows are listed and never
   followed to the next hop. Sites that bind to another definition (`elsewhere`, `scope_resolved`)
   are not callers and are left out. A language with no resolution engine lists every caller
   `name_only`, and a note says why.
3. **Downstream.** `dependencies` lists the files the target files import, over the same graph,
   depth and tiers as the importers. For a symbol, `calls` lists the distinct calls its body makes:
   `same_file` or `imported` (proven, with the defining file) when the resolution facts bind the
   call, else `name_only` with no file.
4. **Cycles.** `targets_in_cycles` and `in_cycle` on every file row, as before, say that a target
   shares an import cycle (ADR-0047 §5).
5. **Surface.** The fields are added to the `code impact` answer only: the command stays CLI-only, so
   no tool schema changes, and the `impact` value-gate kind (which scores `code_refs` importers)
   is unchanged.

## Consequences

- A symbol target costs one `code_refs` callers read per definition examined, bounded by the caps.
- Sites of a name defined elsewhere are dropped, so the caller list can be shorter than the name
  scan of `code_refs`.
- A `path_match` caller is an HTTP call linked to a route by its path; it is not proof of a call.

## Alternatives considered

- **Follow `name_only` callers** — rejected: a name shared by unrelated definitions would fan out
  into false transitive dependencies.
- **A new `code_refs` relation** — rejected: ADR-0047 §3 keeps the tool schema at its byte ceiling.
