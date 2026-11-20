---
status: proposed
date: 2026-11-19
amends: ["0034", "0047"]
---

# ADR-0052: Locating rows as the shell prints them

- **Deciders:** the repository owner
- **Related:** amends ADR-0034 (lean rows) and ADR-0047 §5 (importers); ADR-0031 (value gate by job),
  ADR-0051 (failed baselines), ADR-0018

## Context

ADR-0034 grouped rows by file and dropped echoes. The second value-gate run on the committed
corpora (basemind `a144ebbf`, tokio `b53b2001`), at `f5fcf1b4`, still measured these kinds below
ADR-0031's 0.67 floor for a locating kind (median baseline/tool tokens over credited tasks,
basemind / tokio): `symbols` 0.38x / 0.40x, `grep` 0.52x / 0.59x, `find` 0.50x / 0.53x,
`find_large` 0.44x / 0.50x, `git_search` 0.43x / 0.43x, `git_path` 0.41x / 0.43x, `git_symbol`
0.33x / 0.38x and `impact` 0.58x on basemind; and `blame_symbol` at 1.06x on tokio against the 1.5
floor of a reducing kind. Recall was at parity in every one of them; the excess was the cost of a
row:

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
   does not hold the name as a whole word (`$`, `_`, ASCII letters and digits and every non-ASCII
   byte continue a word), or a symbol without one, is written `"<path>:<line>: <kind> <name>"`,
   followed by `; <signature>` when there is one. A file edited since the scan is listed in a
   top-level `stale`.

2. **`code_grep`.** `hits` is `{"<path>": {"<symbol>": ["<line>: <text>", ...]}}`, grouped by the
   innermost enclosing symbol (`""` outside every symbol). Matches on one line are one row,
   `"<line>:<column>,<column>: <text>"`; on a line clipped around each match, matches whose windows
   differ are a row each, with its column. With
   `context: N` (0-10), the N lines before and after a match are rows `"<line>- <text>"`, each
   written once where windows overlap, as `rg -C N` merges them, and a window stops at the file's
   last line. A file
   edited since the scan is listed in `stale`, and its rows carry no symbol (`""`). The token budget
   charges each symbol key once per file, as it charges each file key.

3. **`find_files`.** With `min_lines`, `files` is `{"<path>": <lines>}`; the byte size is left out
   (`index_status` keeps it for the largest files).

4. **`git_log` lists.** A commit is `"<sha> <summary>"`, the sha still 12 hex digits. Author and
   time are left out: `git_log {rev}` shows them with the whole message. Two lists differ:
   - A search by `author` writes `"<sha> <author>: <summary>"`: `author` matches a substring of the
     name or the email, so one search can select several people, and the row says whose commit it
     is.
   - A symbol's history marks `[introduced]` and `[removed]` after the sha, and `[modified]` only
     when a changed commit's summary itself starts with `[`, so a marker is never read out of a
     summary.

   The recent list and a directory's history keep each commit's files in
   `files: {"<sha>": {"<path>": "<change>"}}`, what changed being the point of both; the map is
   left out when empty. `hash_mode` (an echo) is dropped, `kind` is written only when set and
   `commits_inspected` only beside a `next_cursor`.

5. **`git_blame`.** `hunks` is `["<line>[-<end_line>] <sha>[ <source_path>]", ...]`, adjacent lines
   of one commit (and one source path) merged into one hunk as they are read, so `limit` and the
   token budget count merged hunks and a cursor resumes past the last line of the last one.
   `source_path` is the rest of the row and may hold spaces. `commits` maps each sha to `author`,
   `time` (unix seconds) and `summary`, once, and is left out when there is no hunk. The sha is 12
   hex digits, as in `git_log`: this changes ADR-0034's consequence that `git_blame` keeps the full
   sha; the full sha is `git_log {rev}`'s, one call away as before. `path`, `name`, `suspect_sha`,
   `kind`, `short_sha` and `source_line` are dropped; `elapsed_us` stays, as on every tool whose
   answer replaces reading content.

6. **`code_refs` importers by path.** `in_cycle` is the number of listed importers that share an
   import cycle with the file; `code_refs {relation: "cycles", path}` lists that cycle. A walk cut
   at `depth` is not `truncated`, which changes ADR-0047 §5 ("cut as `truncated_reason` `depth` or
   `scan_cap`"): `truncated` is left for the node cap. Since `depth` defaults to 1, a caller may not
   have chosen it, so `deeper: true` says importers lie past it.

7. **`total` only beside `next_cursor`** on `code_symbols`, `code_grep`, `find_files` and
   `code_refs` importers: a page without a cursor lists every match, so the count repeats the rows.

8. **The CLI.** `--json` prints these shapes. Human output prints a string row per line and a nested
   map as an indented tree (grep hits under file and symbol, a log's files under their sha,
   importers under their depth), the blame `commits` map as a table, and counts the matches a grep
   page lists in its "showing N of M" warning.

The eval extractor reads these shapes.

## Consequences

- A breaking change to the MCP and CLI JSON of six tools, in the same breaking release as
  ADR-0034. `docs/UPGRADING.md` ("Lean rows") lists each.
- These shapes are the `structuredContent` and `--json` of every tool. ADR-0053 writes the MCP text
  block of `code_grep` as `# <path>` and `## <symbol>` headers over the same rows, and of a
  `find_files` list as TOON; there the file order is the header order, and the budget charges
  headers rather than keys.
- Measured on the same corpora with ADR-0051's gate (median ratio, basemind / tokio): `symbols`
  0.75x / 0.79x, `grep` 0.69x / 0.76x, `grep_changed` 0.73x / 0.93x, `find_large` 0.85x / 0.89x,
  `git_search` 0.78x / 0.77x, `git_path` 0.85x / 0.83x, `git_symbol` 0.77x / 0.92x, `impact` 0.72x /
  1.49x, `blame` 6.71x / 3.84x and `blame_symbol` 4.71x / 2.09x, with recall unchanged.
- **`find` sits exactly at the floor on basemind:** 0.667x, the 2/3 that ADR-0031's "at most one
  and a half times" allows (20 of 25 tasks compared, 5 excluded as failed baselines under
  ADR-0051); 0.69x on tokio. The answer is `{"files":["x"]}` against the bare path `x`, 12 tokens
  against 8: the `"files"` key is the remaining cost, and a
  bare list would lose the response's room for `next_cursor` and notices. This ADR changes no
  threshold.
- A consumer that wants fields splits a row at its first `": "` (`code_grep`), at the first
  `:<line>: ` (`code_symbols`), its first space (`git_log`, `git_blame`), or reads a key. A path that
  itself holds `:<digits>:` makes a `code_symbols` row ambiguous, as it makes `git grep -n` output.
  A grep row's text is the line, or for a line over 400 bytes a window around the match marked `…`
  on each cut side; only the separator is added.
- A row whose signature holds the name drops its kind, so a Python method and a function
  (`def f(self)`) look alike in `code_symbols`; `code_outline` tells them apart.
- Rank order rides on JSON key order for `find_files` with `min_lines` and for grep files. serde and
  Python keep it; a consumer that reorders keys (a Go map, JavaScript's integer-like keys) must read
  the text in order.
- `code_grep` rows of one file are in line order within each symbol, not across symbols.
- When a commit was written is one more call for every `git_log` list (`rev`), including a symbol's
  history: a date on each history row measured `git_symbol` at 0.62x on basemind (0.72x on tokio),
  below the floor. Who wrote it is in a search by `author`, and in every `git_blame` answer.
- A blame row no longer names the line the moved lines had in their source file
  (`source_line`); reading the file at that commit shows them.
- Which importers share a cycle with the file is one more call (`relation: "cycles"`).

## Alternatives considered

- **Keep objects and drop more keys** — rejected: `{path, line, signature}` alone is 24 tokens for
  the 17-token `code_symbols` answer, so no field cut reaches the floor on one-row answers.
- **Abbreviate shas to git's default length** — rejected for now: 12 digits already clear the floor,
  and ADR-0034's reason (collisions in large histories) stands.
- **Keep author and date in `git_log` rows** — measured: date alone holds `git_search` at 0.51x /
  0.53x, author and date at 0.43x / 0.45x; a date on symbol-history rows holds `git_symbol` at
  0.62x on basemind.
- **A top-level `authors: {"<sha>": "<name>"}` map for a search by author** — rejected: it repeats
  each sha a second time, where the row already names the commit.
- **Drop the recent list's files** and stop computing them — rejected: which files changed is what
  "what changed recently" asks, and no gate kind measures the recent list.
- **A per-row `in_cycle` flag in `by_depth`** — rejected: the list of paths would become a list of
  objects, which costs more than the duplicate list did.
