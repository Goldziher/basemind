---
status: proposed
date: 2026-11-19
amends: ["0034"]
---

# ADR-0051: Locating rows as the shell prints them

- **Deciders:** the repository owner
- **Related:** amends ADR-0034 (lean rows); ADR-0031 (value gate by job), ADR-0018

## Context

ADR-0034 grouped rows by file and dropped echoes. The second value-gate run
(`reports/eval-0.34-gate-2.md`, at `f5fcf1b4`) still measured `code_symbols` (0.38x / 0.40x),
`code_grep` (0.52x / 0.59x), `find_files` (0.50x / 0.53x, `find_large` 0.44x / 0.50x), all three
`git_log` kinds (0.33x to 0.43x) and `code_refs` impact (0.58x on basemind) below ADR-0031's 0.67
floor for a locating kind, and `git_blame` by symbol at 1.06x on tokio against the 1.5 floor of a
reducing kind. Recall was at parity in every one of them; the excess was the cost of a row:

- A `code_symbols` row repeated every key and the name inside its own signature: 43 tokens against
  17 for the `git grep -n` line.
- A `git_log` row carried author and time beside sha and summary: 95 tokens against 38 for the same
  two commits from `git log --oneline`.
- A `find_files` answer of one path paid a `total` equal to the one path listed.
- A `code_grep` match was an object with three keys, and a second match on a line repeated the
  line's text.
- A blame hunk carried the 40-digit sha, the 7-digit sha, the source line, author, time and summary,
  and two adjacent hunks of one commit stayed two hunks.
- `code_refs` impact repeated each importer that shares a cycle with the file under `in_cycle`.

ADR-0034 rejected a tabular encoding because every consumer would need a decoder. A row written the
way `git grep -n`, `git log --oneline` and `wc -l` print it needs none: an agent reads it as it
reads the shell, and a program splits it at its first separator.

## Decision

A locating row that is a place and one line of text is written as that text, the way the shell
prints it; facts a row shares with its neighbours are written once.

1. **`code_symbols`.** A row is `"<path>:<line>: <signature>"`, as `git grep -n` prints it, and
   `results` keeps rank order, so rows are not grouped by file (grouping moved lower-ranked rows of
   one file ahead of better matches in another, and cost recall at a rank cut-off). A signature that
   does not hold the name, or a symbol without one, is written `"<path>:<line>: <kind> <name>"`,
   followed by `; <signature>` when there is one. A file edited since the scan is listed in a
   top-level `stale`.
2. **`code_grep`.** `hits` is `{"<path>": {"<symbol>": ["<line>: <text>", ...]}}`, grouped by the
   innermost enclosing symbol (`""` outside every symbol). Matches on one line are one row,
   `"<line>:<column>,<column>: <text>"`; a clipped line writes its match's column the same way. With
   `context`, the lines around a match are rows `"<line>- <text>"`, each written once. A file
   edited since the scan is listed in `stale`, and its rows carry no symbol (`""`).
3. **`find_files`.** With `min_lines`, `files` is `{"<path>": <lines>}`; the byte size is left out
   (`index_status` keeps it for the largest files).
4. **`git_log` lists.** A commit is `"<sha> <summary>"`, the sha still 12 hex digits. Author and
   time are left out: `git_log {rev}` shows them with the whole message. A symbol's history marks
   `[introduced]` and `[removed]` after the sha; a directory's history keeps each commit's files in
   `files: {"<sha>": {"<path>": "<change>"}}`. `hash_mode` (an echo) is dropped, `kind` is written
   only when set and `commits_inspected` only beside a `next_cursor`.
5. **`git_blame`.** `hunks` is `["<line>[-<end_line>] <sha>[ <source_path>]", ...]`, adjacent lines
   of one commit (and one source path) merged into one hunk; `commits` maps each sha to `author`,
   `time` (unix seconds) and `summary`, once. The sha is 12 hex digits, as in `git_log`. `path`,
   `name`, `suspect_sha`, `kind`, `short_sha` and `source_line` are dropped; `elapsed_us` stays, as
   on every tool whose answer replaces reading content.
6. **`code_refs` importers by path.** `in_cycle` is the number of listed importers that share an
   import cycle with the file; `code_refs {relation: "cycles", path}` lists that cycle. A walk cut
   at the `depth` the caller asked for is not `truncated`: `truncated` is left for the node cap.
7. **`total` only beside `next_cursor`** on `code_symbols`, `code_grep`, `find_files` and
   `code_refs` importers: a page without a cursor lists every match, so the count repeats the rows.

The eval extractor reads these shapes. The CLI's `--json` output has them too.

## Consequences

- A breaking change to the MCP and CLI JSON of six tools, in the same breaking release as
  ADR-0034. `docs/UPGRADING.md` ("Lean rows") lists each.
- A consumer that wants fields splits a row at its first `": "` (`code_grep`), at the first `:<line>:` (`code_symbols`), its
  first space (`git_log`, `git_blame`), or reads a key. A grep row's line text is written whole; only
  the separator is added.
- `code_grep` rows of one file are in line order within each symbol, not across symbols.
- Who wrote a commit and when is one more call for a `git_log` list (`rev`), and still in every
  `git_blame` answer.
- Which importers share a cycle with the file is one more call (`relation: "cycles"`).
- An agent no longer learns from `code_refs` importers that a deeper walk would find more; it asks
  with a larger `depth`.

## Alternatives considered

- **Keep objects and drop more keys** — rejected: `{path, line, signature}` alone is 24 tokens for
  the 17-token `code_symbols` answer, so no field cut reaches the floor on one-row answers.
- **Abbreviate shas to git's default length** — rejected for now: 12 digits already clear the floor,
  and ADR-0034's reason (collisions in large histories) stands.
- **Keep author and date in `git_log` rows** — measured: date alone holds `git_search` at 0.51x /
  0.53x, author and date at 0.43x / 0.45x.
- **A per-row `in_cycle` flag in `by_depth`** — rejected: the list of paths would become a list of
  objects, which costs more than the duplicate list did.
