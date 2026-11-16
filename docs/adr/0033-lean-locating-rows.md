---
status: proposed
date: 2026-11-16
---

# ADR-0033: Lean rows for the locating tools

- **Deciders:** the repository owner
- **Related:** ADR-0031 (value gate by job), ADR-0018, ADR-0022, ADR-0030

## Context

ADR-0031 holds a locating tool to at most one and a half times the tokens of the shell answer. The
first value-gate run measured `code_symbols`, `code_grep`, `find_files` and `git_log` at four to ten
times that answer (`git grep -n`, `git ls-files`, `git log --oneline`). Most of the excess was not
information: every row repeated its file's path and the `"path":` key, every row carried a byte
column, every response echoed its arguments (`pattern`, `regex`, `name`, `match`, `module`,
`trait_name`), carried `elapsed_us`, and a `git_log` row carried a 40-digit sha, the message body
and a list of `{path, change}` objects.

## Decision

The locating tools answer with what the shell prints plus what they add over it.

1. **Rows grouped by file.** `code_grep` hits and `code_refs` hits (call sites, callers,
   implementations) are written as `{"<path>": [row, ...]}`. Files keep the order of their first
   row; rows keep the order they were found.
2. **Column only when the line does not place the row.** A grep hit carries `column` when its file
   holds another match on that line or its text is a clipped window of the line; a call site when
   another call of the same callee is on its line. Both are decided over the whole result, so a
   page boundary never changes a row. An implementation carries it when another row of its page
   shares the line.
3. **No echoes.** A response does not repeat its arguments. A `code_refs` callee equal to the name
   asked for and an implementation's trait equal to the trait asked for are left out.
   `total_files_matched` is dropped; `code_grep`'s `total_matches` is `total`.
4. **No `elapsed_us`.** Latency is recorded by telemetry and timed by the CLI, not written into each
   answer.
5. **Plain values where a row held an object.** A grep hit's `symbol` is the enclosing symbol's
   name, `find_files` lists bare paths, and a `code_symbols` row is path, line, kind, name and
   signature.
6. **Commits.** A `git_log` list row is a 12-digit abbreviated sha, summary, author and time, with
   `files` as a map from path to change. A search hit carries no body; `git_log {rev}` shows the
   whole commit with its full sha.
7. **Diffs.** `git_diff` drops a hunk's `kind`, writes `present_at_*` only when false, and leaves
   out an empty status list.

The CLI's `--json` output has the same shapes. The eval extractor reads them and matches an
abbreviated sha (7 or more hex digits) as a prefix of the gold's full sha.

## Consequences

- A breaking change to the MCP and CLI JSON contract, shipped in the single breaking release of
  ADR-0027. Clients that read `hit.path`, `symbol.name`, `files[].path` or a 40-digit list sha must
  change; `docs/UPGRADING.md` ("Lean rows") lists each.
- A consumer that wants a flat list re-attaches each row's path from its key; one that matches on
  column must accept its absence and fall back to the line.
- A 12-digit sha can become ambiguous in a very large history. `git_log {rev}` and `git_blame` keep
  the full sha, so an exact commit is always one call away.
- A path that is not UTF-8 is written lossily as a group key, since a JSON key is a string.

## Alternatives considered

- **Keep the flat rows and lower the gate** — rejected: ADR-0031 already set the floor at 0.67, and
  the excess measured was repetition, not information.
- **A tabular encoding (column headers plus value arrays)** — rejected: smaller still, but every
  consumer would need a decoder, and a per-file group already removes the largest repetition.
- **Abbreviate shas to 7 digits** — rejected: collisions are common in large histories; 12 is the
  abbreviation the Linux kernel uses.
- **Keep `elapsed_us` behind a flag** — rejected: a parameter that only adds diagnostics widens every
  schema in `tools/list` for no answer it improves.
