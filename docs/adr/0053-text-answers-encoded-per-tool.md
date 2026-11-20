---
status: proposed
date: 2026-11-19
amends: ["0034", "0052"]
---

# ADR-0053: Text answers encoded per tool, by measured tokens

- **Deciders:** the repository owner
- **Related:** amends ADR-0034 (lean rows; it rejected a tabular encoding) and ADR-0052 (rows as the
  shell prints them); ADR-0018, ADR-0031 (value gate by job), ADR-0051

## Context

Every verb tool answered with its JSON twice: as MCP `structuredContent` and, serialized, as the
text block a model reads. On the value-gate corpora (basemind `a144ebbf`, tokio `b53b2001`) at
`ec67738c`, `read_around` (one `code_grep` with `context: 5` against `rg -n -C5`) measured 1.31x on
basemind against the 1.5 floor of a reducing kind; 13% of `code_grep`'s tokens were JSON keys,
quotes and the `\"` escapes code is full of.

TOON (Token-Oriented Object Notation, toonformat.dev) encodes the JSON data model as indented
`key: value` lines, writes an array of flat objects as one table (`name[N]{field,...}:` and a
comma-separated row each) and quotes a string only where it would be misread. Each tool answer of
both corpora's 1,240 committed tasks was taken from the eval (one page each) and written three ways,
counting o200k tokens with the eval tokenizer: compact JSON, TOON (`toon-format` 0.7, spec-conformant,
comma and tab delimiters; every answer decoded back to the same JSON), and for the `code_grep` kinds
plain text. Median tool tokens over credited tasks with a baseline (basemind / tokio):

| kind | JSON | TOON | TOON, tab | plain |
|---|---|---|---|---|
| `grep` | 44 / 58 | 47 / 61 | 48 / 63 | 39 / 53 |
| `grep_changed` | 50 / 79 | 53 / 82 | 54 / 88 | 47 / 74 |
| `read_around` | 366 / 249 | 370 / 253 | 393 / 279 | 340 / 231 |
| `outline` | 875 / 576 | 669 / 430 | 676 / 438 | - |
| `diff` | 401 / 219 | 381 / 209 | 383 / 209 | - |
| `diff_structural` | 204 / 85 | 158 / 81 | 160 / 82 | - |
| `find` | 12 / 14 | 11 / 13 | 12 / 14 | - |
| `find_large` | 93 / 105 | 112 / 129 | 112 / 129 | - |
| `symbols` | 48 / 93 | 48 / 93 | 52 / 97 | - |
| `read` | 188 / 114 | 190 / 116 | 190 / 116 | - |
| `references` | 32 / 44 | 40 / 53 | 42 / 55 | - |
| `callers` | 70 / 70 | 81 / 81 | 82 / 82 | - |
| `implementations` | 59 / 67 | 54 / 65 | 56 / 67 | - |
| `dependents` | 20 / 27 | 19 / 26 | 18 / 27 | - |
| `impact` | 47 / 75 | 50 / 78 | 48 / 80 | - |
| `git_search` | 32 / 33 | 32 / 33 | 33 / 34 | - |
| `git_path` | 286 / 138 | 287 / 136 | 308 / 143 | - |
| `git_symbol` | 32 / 30 | 33 / 30 | 34 / 31 | - |
| `blame` | 74 / 119 | 80 / 115 | 81 / 117 | - |
| `blame_symbol` | 81 / 84 | 89 / 91 | 90 / 92 | - |
| `explain` (CLI only) | 339 / 320 | 357 / 337 | 360 / 343 | - |

TOON shortens what is a table: an outline's symbols, a structural diff's symbols, hunks. It
lengthens what ADR-0052 already made strings. Of 2,103 `code_grep` rows, 1,761 must be quoted in
TOON (a `:` forces quoting, and every match row has one), which then escapes the code's quotes and
backslashes; a path key holds `/` and is quoted too; and a map of path to count (`find_large`), each
entry on its own indented line, costs more than JSON's.

## Decision

The text block of a verb tool is written in the encoding that measured fewest tokens for it.
`structuredContent` stays JSON for every tool, and so does the CLI's `--json` output.

1. **`code_grep` rows: a plain block.** The answer's other fields are TOON `key: value` lines; then
   each file is a line `# <path>`, each enclosing symbol `## <symbol>`, and the rows follow as
   ADR-0052 writes them, without JSON's quotes: `<line>: <text>`, `<line>:<col>,<col>: <text>`,
   `<line>- <text>`. Rows before any symbol header are outside every symbol; a later group outside
   every symbol is opened by a bare `##`. A header is bare unless the name starts with `"`, has
   leading or trailing whitespace or holds a control character, and then it is a JSON string. A
   row always starts with its line number, so it is never read as a header, and a TOON line never
   starts with `#`, so the block starts at the first file header. An answer whose rows the block cannot
   carry (a row with a line break or carriage return, or one not starting with its line) and an
   answer with no hits are compact JSON. The `ranges` argument of `code_grep`, `code_refs` and
   `code_read` is removed, since `code_grep` with `context: N` returns the lines around each hit in
   one call.
2. **TOON for `code_outline`, `git_diff` and a `find_files` list**, through the `toon-format` crate
   (MIT, the toon-format organisation's Rust implementation, without its default `cli` feature).
3. **Compact JSON for every other tool and shape**: TOON measured no shorter (`code_symbols`,
   `code_read`, `git_log`, `code_refs` taken together, `git_blame`, `find_files` with `min_lines`) or
   was not measured (`code_search`, `docs_search`, the memory tools, `index_status`,
   `index_rescan`).
4. **One place.** `crate::wire::encode(tool, value)` writes the text block in `src/mcp/adapter.rs`,
   after the cursor is bound and before the telemetry row, so telemetry costs the text sent.
   `crate::wire::decode` reads any of the three back to the JSON; the eval scores the text through
   it, and an answer it cannot read is a task error, never an empty answer. A resource mirrors its
   tool's encoding: `basemind://outline/{path}` is TOON, served as `text/toon`.
5. **Budgets charge what is written.** `max_tokens` costs a `code_grep` hit as its rows and the
   headers it opens in the block, an outline symbol and a `find_files` path as their TOON table row
   or cell, and keeps the JSON cost elsewhere.

## Consequences

- A breaking change to the MCP text of four tools; `docs/UPGRADING.md` ("Text answers") lists them.
  A client that parsed the text block as JSON reads `structuredContent`, or decodes the text.
- Measured with the gate after the change, recall unchanged everywhere (basemind / tokio, tool
  tokens and ratio): `grep` 39 / 53 tokens, 0.78x / 0.86x; `grep_changed` 47 / 74, 0.83x / 0.98x;
  `read_around` 340 / 231, 1.43x / 1.61x; `outline` 669 / 359, 4.71x / 2.21x; `diff` 381 / 209,
  1.16x / 1.17x; `diff_structural` 114 / 74, 32.19x / 13.46x; `find` 11 / 13, 0.73x / 0.75x. Every
  other kind is unchanged.
- **`read_around` still fails the 1.5 floor on basemind (1.43x).** What remains over the rows the
  shell prints is the `# <path>` and `## <symbol>` headers: the enclosing symbol is a fact `rg` does
  not give, and this record keeps it. No threshold changes.
- A host that hands the model `structuredContent` rather than the text block sees JSON, and none of
  these savings.
- MCP asks a tool that returns structured content to mirror it as serialized JSON in a text block
  for backwards compatibility; these four tools mirror it in another encoding of the same data.
- The tools now answer in three encodings. A tool moves from one to another only on a measurement
  like the one above.
- A new dependency, `toon-format` 0.7 (pre-1.0; its encoder is the spec's reference port). Only
  `serde`, `serde_json`, `indexmap` and `thiserror` come with it, all already in the tree.

## Alternatives considered

- **TOON for every tool** — rejected by the table: it costs more than JSON on `code_grep`,
  `code_refs` references and callers, `git_blame` and `find_large`, because their rows are strings
  that hold `:`.
- **TOON with a tab delimiter** — no shorter: a tab is no cheaper than a comma, and `:` still forces
  quoting.
- **rg's own row form, `<line>:<text>` and `<line>-<text>` without the space** — measured at 336 /
  225 tokens for `read_around` (1.44x / 1.63x) against 340 / 231; the space keeps ADR-0052's rows,
  which the column form `<line>:<col>,<col>: <text>` needs to stay unambiguous.
- **Drop the symbol headers from `read_around` answers** — rejected: the enclosing symbol is what
  `code_grep` adds over `rg`; dropping it to pass a gate would trade the answer for the metric.
- **A JSON text block and a separate compact block** — rejected: the model would read both.
