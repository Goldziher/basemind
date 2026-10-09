# basemind retrieval-quality and token-savings evaluation

`basemind admin eval` scores basemind's lookups against gold generated from the repository itself,
and measures whether the token savings the dashboard claims (`src/mcp/savings.rs`) hold up.
Nothing here is specific to one repository: the generators only need a git checkout
(the AST-based ones read Python; the rest are language-agnostic).

## Workflow

```sh
cd /path/to/repo && basemind scan                    # the index must exist and match HEAD
E=/path/to/basemind/benchmarks/eval

python3 $E/gen_symbols.py    --repo . --seed 1 --n 60 --exclude 'node_modules' --exclude '**/migrations' --out symbols.jsonl
python3 $E/gen_outline.py    --repo . --seed 1 --n 40  --out outline.jsonl
python3 $E/gen_references.py --repo . --seed 1 --n 60 --mode both --out references.jsonl   # references + callers
python3 $E/gen_dependents.py --repo . --seed 1 --n 40  --out dependents.jsonl
python3 $E/gen_grep.py       --repo . --seed 1 --n 60 --language python --ext py --out grep.jsonl
python3 $E/gen_find.py       --repo . --seed 1 --n 60 --out find.jsonl
python3 $E/gen_git_search.py --repo . --seed 1 --n 40  --out git_search.jsonl
python3 $E/gen_docs.py       --repo . --seed 1 --n 30  --out docs.jsonl                    # needs the documents feature

cat *.jsonl > tasks.jsonl
basemind admin eval --tasks tasks.jsonl --warmup --out results.ndjson --report report.json --markdown report.md
```

Rules for honest numbers:

- **Clean checkout, index at HEAD.** Gold is read from the HEAD tree (`git cat-file`, `git grep ... HEAD`),
  never the working tree, so output is reproducible for a fixed HEAD sha and `--seed` (byte-identical
  across runs and `PYTHONHASHSEED`). basemind queries the working tree, so uncommitted edits
  show up as misses. Scan first, and rescan after moving HEAD.
- **Mirror your scan config.** Pass the same ignore globs as `--exclude` that your `basemind.toml`
  excludes from the scan (`**` crosses directories; a glob without `/` matches at any depth).
  Gold for files basemind never indexed would be unreachable, not a basemind miss.
- **Use the same tokenizer on both sides.** With the `tokenizer` feature (part of `documents`) tokens are real o200k
  counts; without it, `bytes/4`. The report records which (`tokenizer`); `--baseline` refuses to compare across the two.

Every generator takes `--repo`, `--seed`, `--n`, `--exclude GLOB` (repeatable), `--ext`, `--max-bytes`, `--out`.

| generator | mode(s) | gold |
|---|---|---|
| `gen_symbols.py` | `symbols` | every def / class / simple assignment target (any depth) whose name contains the query, as `path:line` |
| `gen_outline.py` | `outline` | `path:line` of every module- and class-level def, class and assignment (never function locals) |
| `gen_references.py` | `references`, `callers` | `path:line` of every `ast.Call` whose callee identifier is the name, cross-checked with `git grep -n -w` |
| `gen_dependents.py` | `dependents` | files whose import statement text contains the module string |
| `gen_grep.py` | `grep` | `git grep -n -E` lines with the same path/language filters |
| `gen_find.py` | `find` | the source path of a basename / typo / consonant-skeleton query (ranked) |
| `gen_git_search.py` | `git_search` | shas of all commits whose message tokens contain the query's rare subject tokens (ranked) |
| `gen_docs.py` | `docs` | the markdown file holding a sentence that names backticked code (ranked) |

Each generator's docstring states the exact basemind semantics its gold encodes, measured against
the real tools (for example: `references` counts call sites only and reports the line where the
call expression starts; `dependents` is a substring match on the whole import statement;
`git search` is a tokenized AND, not a regex). A generator skips candidates it cannot make exact,
for instance names that also occur in other languages' files (`--no-cross-language-check` disables that).

**Pyrefly.** `pyrefly` 1.x has no find-references command (its CLI is `check`, `infer`, `coverage`, `lsp`, ...). Scope-resolved
references would need a `textDocument/references` request to `pyrefly lsp`, which these generators do not make. The
name-only `ast` gold is the right oracle for basemind's name-only `references`/`callers` anyway.

## Task format

One JSON object per line (blank lines and `#` lines are skipped; ids are unique):

```json
{"id":"symbols-0003","mode":"symbols","args":{"name":"Widget","limit":200},
 "gold":["pkg/widget.py:1"],"scoring":"set","k":10,"line_slack":0,
 "baseline":{"grep":"git grep -n ...","read":["pkg/widget.py"]}}
```

- `mode`: `symbols`, `outline`, `references`, `callers`, `grep`, `find`, `dependents`, `git_search`, `docs`.
- `args`: the tool's arguments without `mode` (`name`, `pattern`, `path`, `query`, `module`, `language`, `limit`, ...).
- `gold`: expected items. `path` or `path:line`; see the item table below. Gold with any `path:line` entry is scored at line level.
- `scoring`: `set` (order-insensitive) or `ranked`. `k` is the rank cut-off (default 10). `line_slack` tolerates off-by-n lines.
- `baseline` (optional): what an agent would do without basemind. `grep` is one shell command or a list, run with `sh -c`
  in the workspace root (exit 0 or 1 are fine; anything else fails the baseline); `read` lists files read whole.
- Unknown fields (for example `meta`) are ignored.

What a returned item is, per mode: `symbols` / `outline` / `references` / `callers` / `grep` give `path:line`
(1-based), `find` / `dependents` / `docs` give `path`, `git_search` gives the commit sha.

## Metrics

Per task (NDJSON row): `elapsed_us` (the in-process tool call), `tokens` (the response text as an agent receives it), `score`.

- **Set**: precision, recall, F1 over de-duplicated items. Items are de-duplicated by path, or by `(path, line)` when
  the gold has lines, so ten hits in one file do not count ten times against a path-level gold. Empty gold: recall 1,
  precision 1 only for an empty answer.
- **Ranked** (adds): the answer is cut to `k` before P/R/F1; hit@1, hit@5, hit@k; MRR (1/rank of the first relevant item);
  nDCG@k with binary relevance (each gold entry can score once).
- **Errors**: a tool error is a failed task, scored as an empty answer and listed on stderr.

Aggregate per mode (JSON `--report`, markdown `--markdown`): task and error counts, p50/p95 latency (nearest-rank), mean P/R/F1,
hit@1 / hit@5 / MRR / nDCG (means over ranked tasks only), mean response tokens, and the savings block.

Latency is the in-process call on a warm one-shot server (`--warmup` runs every task once untimed first). It excludes
transport and process start-up, so it is a lower bound of what an MCP client sees.

## Token savings

For tasks with a `baseline`, the harness runs it in the workspace root, counts the output of the commands plus the whole
files in `read` with the same tokenizer, and records `baseline_tokens / basemind_tokens` and tokens saved.
**Savings only count when the basemind answer was correct**: recall must reach `--min-recall` (default 0.8) and the
gold must be non-empty; otherwise the task is listed as withheld. A cheap wrong answer is not a saving.

Per mode the report gives the credited distribution (median and p90 of the ratio and of tokens saved) and compares the
median ratio with what `src/mcp/savings.rs` would assume for the same response (its fixed per-mode multiplier, calibrated
from this eval: `outline` 1.2x, `symbols` 25x, `references` / `implementations` 1.2x, `docs` 2.5x; 1x, i.e. no saving, for
`callers`, `dependents`, `find`, `files`, `grep` and `git search`; see the constants in that file). A mode whose measured median
deviates by more than 25% is flagged `DEVIATES`. The harness only reports; it does not change `savings.rs`, so re-run it
and update the constants when a response shape changes. A mode whose median is below 1 claims no saving there.

The measured ratio depends on how heavy your baseline is. Generated baselines model a reasonable agent (a targeted
`git grep` plus up to three whole-file reads); edit them in the task file to model a different one. Ratios below 1 are
real: for a single narrow hit, `grep` is cheaper than a structured response.

## Regression gate

```sh
basemind admin eval --tasks tasks.jsonl --report new.json --baseline committed-report.json \
  [--tolerance 0.02] [--cost-tolerance 0.25]
```

Prints `REGRESSION ...` lines and exits non-zero when, per mode, mean F1, recall, hit@1, hit@5, MRR or nDCG drops by more than
`--tolerance` (absolute), p95 latency or mean response tokens rise by more than `--cost-tolerance` (relative, and latency by
over 1 ms), errors increase, or a mode disappeared. Use the same task file for both runs.

## Tests

`python3 -m unittest discover benchmarks/eval` runs the generator tests (a throwaway git repo with known gold, determinism
across hash seeds, the cross-language guard). The harness itself is covered by `cargo test --lib eval::` and
`cargo test --test eval_smoke`.

## Limitations

- The AST generators (`symbols`, `outline`, `references`, `dependents`) emit Python gold only. The harness and the other generators are language-agnostic.
- `outline`/`symbols` gold encodes basemind's Python symbol rules: module- and class-scope definitions and assignments only. Names bound inside a function or lambda (locals, nested defs) are not indexed, so the generators never descend into function bodies.
- `find` covers every path basemind indexes (code map plus document tier). Pass `--indexed FILE` to `gen_find.py` (one indexed path per line) so tasks are never drawn from files the scan config leaves out; use `--ext md,rst,...` for a documents-only batch.
- `docs` and semantic lookups need the `documents` build; without it those tasks error.
- Gold is derived by syntax and text, not by scope resolution, so it is exact for basemind's name-based tools and only an approximation of "the right answer" for an agent that wanted a specific binding.
