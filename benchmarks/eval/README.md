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

`mode` below is the eval task kind (the task file's `mode` field), not an MCP argument. Each maps to one
tool: `symbols` to `code_symbols`, `outline` to `code_outline`, `read` to `code_read`, `references` / `callers` /
`implementations` / `dependents` to `code_refs`, `grep` to `code_grep`, `find` to `find_files`, `git_search` /
`git_path` / `git_symbol` to `git_log`, `blame` / `blame_symbol` to `git_blame`, `diff` / `diff_structural` to
`git_diff`, `docs` to `docs_search`. The analysis kinds map to the tool whose argument they exercise: `impact` and
`cycles` to `code_refs`, `grep_changed` to `code_grep`, `find_large` to `find_files`, `read_around` to `code_read`
(after a `code_grep` with `ranges`), and `explain` to the CLI-only `code explain`.

| generator | mode(s) | gold |
|---|---|---|
| `gen_symbols.py` | `symbols` | every def / class / simple assignment target (any depth) whose name contains the query, as `path:line` |
| `gen_outline.py` | `outline` | `path:line` of every module- and class-level def, class and assignment (never function locals) |
| `gen_references.py` | `references`, `callers` | `path:line` of every `ast.Call` whose callee identifier is the name, cross-checked with `git grep -n -w` |
| `gen_dependents.py` | `dependents` | files whose import statement text contains the module string |
| `gen_rust.py` | `symbols`, `outline`, `references`, `callers`, `implementations`, `dependents`, `read` | Rust gold from `rust_gold` (syn, not tree-sitter); see its docstring and `_rust.py` |
| `gen_git.py` | `git_path`, `git_symbol`, `blame`, `blame_symbol`, `diff`, `diff_structural` | git's own output on the pinned commit; symbol bodies and names from `rust_gold` |
| `gen_grep.py` | `grep` | `git grep -n -F` (literal) or `-E` (regex, `regex: true`) lines with the same path/language filters; Rust tasks also carry the enclosing definition per line (`annotations`) |
| `gen_find.py` | `find` | the source path of a basename / typo / consonant-skeleton query (ranked) |
| `gen_git_search.py` | `git_search` | shas of all commits whose message tokens contain the query's rare subject tokens (ranked) |
| `gen_docs.py` | `docs` | the markdown file holding a sentence that names backticked code (ranked) |
| `gen_graph.py` | `impact`, `cycles` | file-level runtime import edges from `use` resolved through each package's `Cargo.toml` (Rust, `rust_gold`; `mod` declarations are not edges, ADR-0047 decision 4) or `import` (Python, `ast`), see `_edges.py`: importers within a depth, files of each import cycle |
| `gen_changed.py` | `grep_changed` | the lines of the files changed since `HEAD~k` that hold a literal |
| `gen_large.py` | `find_large` | files with at least N lines, counted as the scanner counts them (a final unterminated line counts) |
| `gen_read_around.py` | `read_around` | every line within 5 of a literal's hits, merged into ranges; the baseline is what `rg -n -C5` prints |
| `gen_explain.py` | `explain` | a Rust fn's definition, call sites and `->callee` names, from `rust_gold` |

The last five cover the 0.34 analysis kinds (ADR-0047). Their engine ops have landed, so `basemind admin eval` runs,
scores and gates their tasks like every other kind.

Each generator's docstring states the exact basemind semantics its gold encodes, measured against
the real tools (for example: `references` counts call sites only and reports the line where the
call expression starts; `dependents` is a substring match on the whole import statement;
`git_log` with `query` is a tokenized AND, not a regex). A generator skips candidates it cannot make exact,
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

- `mode`: `symbols`, `outline`, `read`, `references`, `callers`, `implementations`, `dependents`, `grep`, `find`,
  `git_search`, `git_path`, `git_symbol`, `blame`, `blame_symbol`, `diff`, `diff_structural`, `docs`, `impact`,
  `cycles`, `grep_changed`, `find_large`, `read_around`, `explain`.
- `args`: the arguments of the tool the mode maps to, exactly as the MCP tool takes them; an unknown argument fails
  the task. `symbols`: `{name, limit}`. `outline`: `{path}`. `references`: `{name, limit}`. `callers`:
  `{path, name, limit}`. `dependents`: `{name, relation: "importers"}`. `grep`: `{pattern, regex: true, language,
  path, limit}` (`pattern` is literal text without `regex`; `path` is a path substring). `find`: `{query, limit}`.
  `git_search`: `{query, limit}`. `docs`: `{query}`. The other kinds take their tool's arguments the same way
  (`implementations`: `{name, relation: "implementations"}`; `read`: `{path, symbol}`; `git_path`: `{path}`;
  `git_symbol`: `{path, symbol}`; `blame`: `{path, line, end_line}`; `blame_symbol`: `{path, symbol}`; `diff`:
  `{path, from, to}`; `diff_structural`: `{path, from, structural: true}`).
- `gold`: expected items. `path` or `path:line`; see the item table below. Gold with any `path:line` entry is scored at line level.
- `scoring`: `set` (order-insensitive) or `ranked`. `k` is the rank cut-off (default 10). `line_slack` tolerates off-by-n lines.
- `baseline` (optional): what an agent would do without basemind. `grep` is one shell command or a list, run with `sh -c`
  in the workspace root (exit 0 or 1 are fine; anything else fails the baseline); `read` lists files read whole.
  `items` names how the output is read as items, so the baseline is scored against the same gold: `grep_lines`
  (`path:line:` prefixes), `grep_context` (`rg -n -C` / `grep -n -C` output: `path:N:` match and `path-N-` context
  rows, split at the earliest separator whose prefix is a file), `paths`, `shas` (first token of each line,
  abbreviations resolved with `git rev-parse`), `blame` (`git blame` default output), `diff` (a unified diff's
  added/removed lines), `diff_symbols` (definition names on added/removed lines), `read` (every line of the files read),
  `explain` (`path:line` of each `path:N:text` row of a file, and `->name` for each name called on any other line that
  is not a path or a `git log` row). Without `items` the baseline is charged for tokens only.
  A baseline that exits above 1 or prints nothing is a **baseline failure** (`baseline_failed` on the row): it scores
  recall 0 where it has `items`, and its ratio is left out of the median (see the gate). The baseline runs even when
  the tool errored, so both arms are scored on the same tasks.
- `annotations` (`grep`, optional): `path:line` -> the definition that line sits in; scored as the share of returned
  gold hits whose attached `symbol` has that name (`annotation_accuracy`, not part of the gate).
- Unknown fields (for example `meta`) are ignored.

What a returned item is, per mode: `symbols` / `outline` / `references` / `callers` / `implementations` / `grep`
give `path:line` (1-based), `read` every `path:line` of the span, `find` / `dependents` / `docs` give `path`,
`git_search` / `git_path` / `git_symbol` the commit sha, `blame` / `blame_symbol` `sha:line` per line, `diff`
`+path:line` / `-path:line` per added / removed line, `diff_structural` `+name` / `-name`.

## Metrics

Per task (NDJSON row): `elapsed_us` (the in-process tool call), `tokens` (the response text as an agent receives it), `score`,
and `baseline_score` when the baseline names `items`. A set-scored task follows `next_cursor` (up to 50 pages, `pages`;
`pages_cut` when the cap stopped it) and is charged the tokens of every page, as an agent that wants the whole answer
must page; a ranked task reads one page.

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
median ratio with what `src/mcp/savings.rs` would assume for the same response (its fixed per-tool multiplier, keyed by
tool name and calibrated from this eval: `code_outline` 1.2x, `code_symbols` 25x, `docs_search` 2.5x; 1x, i.e. no
saving, for `code_refs` (which serves `references`, `callers` and `dependents`), `find_files`, `code_grep` and
`git_log`; see the constants in that file). A mode whose measured median
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

`uv run --no-project --with pytest python -m pytest benchmarks/eval -q` runs the generator, gate-summary and fetch tests
(throwaway git repositories with known gold, determinism across hash seeds, the cross-language guard). The harness is
covered by `cargo test --lib eval::`, `cargo test --test eval_smoke` and `cargo test --test eval_gate_smoke` (every
kind and its baseline on a hand-worked fixture, and the same tasks with a wrong gold scoring zero), `rust_gold` by
`cargo test --offline --manifest-path benchmarks/eval/rust_gold/Cargo.toml`. `test_analysis_generators.py` covers the
analysis kinds' gold, edge resolution and baselines on a fixture with a known module graph.

## Limitations

- The AST generators (`gen_symbols.py`, `gen_outline.py`, `gen_references.py`, `gen_dependents.py`) emit Python gold only;
  `gen_rust.py` and `gen_git.py` cover Rust. The harness and the other generators are language-agnostic.
- `outline`/`symbols` gold encodes basemind's Python symbol rules: module- and class-scope definitions and assignments only. Names bound inside a function or lambda (locals, nested defs) are not indexed, so the generators never descend into function bodies.
- `find` covers every path basemind indexes (code map plus document tier). Pass `--indexed FILE` to `gen_find.py` (one indexed path per line) so tasks are never drawn from files the scan config leaves out; use `--ext md,rst,...` for a documents-only batch.
- `docs` and semantic lookups need the `documents` build; without it those tasks error.
- Gold is derived by syntax and text, not by scope resolution, so it is exact for basemind's name-based tools and only an approximation of "the right answer" for an agent that wanted a specific binding.

## Value gate (ADR-0022, by job as ADR-0031 amends it)

The gate runs the committed task files over a pinned corpus and gives each tool a verdict; a tool passes when every one
of its kinds passes on every corpus. Every kind has a job, declared in one table (`job` in `src/eval/gate.rs`), and the
job picks the rule a kind must meet on each corpus, counted with o200k:

| job | kinds | rule |
|---|---|---|
| locating: the answer is a list of places | `symbols`, `grep`, `references`, `callers`, `implementations`, `dependents`, `find`, `git_search`, `git_path`, `git_symbol`, `diff`; `impact`, `grep_changed`, `find_large` | credited >= 0.70, median ratio >= 2/3 (the answer costs at most 1.5x the shell's), and mean recall no more than 0.02 below the baseline's |
| reducing: the answer replaces reading content | `outline`, `read`, `blame`, `blame_symbol`, `diff_structural`, `docs`, `semantic` (no task kind yet); `read_around`, `explain` | credited >= 0.70 and median ratio >= 1.5 |
| correctness: no shell command answers it | `cycles` | credited >= 0.70 |

`grep` also needs the enclosing-symbol accuracy of `code_grep` at 0.95 or more (ADR-0031 rule 4). The kinds before the
semicolons are ADR-0031's; the 0.34 analysis kinds after them (and the correctness job) are ADR-0051 (proposed),
which classifies them as the second 0.34 gate run did. Counting rules:

- **Credited**: tool recall >= 0.8 on a non-empty gold, over every task of the kind, errors included.
- **Ratio**: baseline tokens / tool tokens, nearest-rank median over the credited tasks whose baseline answered. A
  baseline failure (it exited above 1 or printed nothing) is left out: an empty shell answer is not a cheap one. The
  locating floor is exactly 2/3, ADR-0031's "at most one and a half times the shell's": 12 tokens against 8 passes
  (until the third 0.34 gate run it was 0.67, which failed exactly 1.5x). A failed ratio is printed with the digits
  that tell it from its floor (`median 0.666x < 0.667x`).
- **Recall parity**: the tool's and the baseline's mean recall over the tasks whose baseline is scored (it names
  `items`), a failed baseline scoring 0. A locating kind whose baselines name no `items` has no verdict.
- **No verdict** (`n/a`): tokens not counted as o200k, no tasks, a credited set whose baselines all failed, or a check
  with nothing to compare. A failing check wins over an undecidable one.

`gate.py` lists every kind under the tool it drives and refuses a report that gates a kind it does not list, or lists
under another tool than the report names, so a failing analysis kind always fails its tool in `gate.md`.

### Reproduce

```sh
# tokenizer: o200k counts (bytes/4 gives no verdict); code-intel: the Rust import resolver (impact, cycles)
cargo build --release --features tokenizer,code-intel --bin basemind
corpus=/path/outside/the/repo/eval-corpus
benchmarks/eval/fetch_corpus.sh "$corpus"                     # clone, check out and verify every pin
benchmarks/eval/gen_tasks.sh "$corpus" /tmp/tasks && diff -r /tmp/tasks benchmarks/eval/tasks   # optional: the
                                                              # committed tasks regenerate byte-identically
BASEMIND_DATA_HOME="$(mktemp -d)" BASEMIND_COMMS_DIR="$(mktemp -d)" \
  benchmarks/eval/run_gate.sh target/release/basemind "$corpus" /tmp/gate-results
```

- `corpus.tsv` is the corpus: each repository's URL, pinned commit, the sha256 of its `git ls-tree -r` listing and its
  file count. basemind is pinned at v0.33.0 and `tokio` at tokio-1.54.1 (the harden harness clones `tokio` at whatever
  its default branch holds, so the pin lives here). Nothing is vendored; `fetch_corpus.sh` refuses a directory inside the
  repository and runs no hooks. The clones are data: nothing in them is executed.
- `tasks/<corpus>.jsonl` are the task files (`gen_tasks.sh`, `--seed 1`, 25 per kind or variant), regenerated only when
  a pin moves. Gold never comes from basemind: Rust structure from `rust_gold` (syn), git, grep and find gold from git's
  own output on the pinned commit.
- `run_gate.sh` refuses to start unless `BASEMIND_DATA_HOME` and `BASEMIND_COMMS_DIR` are set to existing, empty
  directories outside the default cache, refuses a binary without the `tokenizer` feature, refuses to run `read_around`
  tasks without `rg` on PATH (its version goes into `run.tsv`), refuses a binary whose `code cycles --json`
  `coverage` has no Rust resolver row (no `code-intel`) for a task file with `impact` / `cycles` tasks, scans each repository,
  runs `basemind admin eval --tasks ... --min-recall 0.8`, and stops any daemon it could have started on exit.
- `gate.py RESULTS_DIR` turns the per-corpus reports into `gate.md` / `gate.json`.
- The binary: each report's `build` holds the Cargo version, `build_id` (a hash of the executable) and the features
  compiled in (`code-intel` when both its parts are, so a `tokenizer,code-intel` build reads as such), and `run.tsv` adds `source_commit`, the commit of the checkout `run_gate.sh` runs from (`-dirty` with
  uncommitted tracked changes). Run it from the checkout the binary was built from: the version string is not bumped
  on main, so it does not identify the build. `gate.md` prints all of them.

### Read the table

`results/gate.md` names the binary, lists each tool's verdict, then one row per tool, kind and corpus: the job, task and
error counts, credited tasks (fraction), baseline failures, the tool's and the baseline's mean recall against the same
gold (over the tasks whose baseline is scored), the tool's precision, median tokens of each arm over all the kind's
tasks, the median ratio, and for `grep` the share of hits whose attached symbol was right. A ratio below 1 means the shell answer was cheaper. Latency is in the per-corpus markdown and
`run.tsv` with the load average next to it; the machine is shared, so it is reported, never gated.

### First run

`results/` holds the first run (2026-11-16): a dev-profile `--features tokenizer` binary built from this branch at
`52991f35` (version 0.33.0 plus unreleased commits), over a fresh fetch of the corpus. A second run from a separate
cache gave the same scores and credits; token counts moved by at most 1 on about a tenth of the tasks, because
responses carry their own `elapsed_us`. Only `code_read` passes. `code_outline` passes on basemind and fails on
tokio (credited 0.68); `git_blame` fails `blame_symbol` on tokio (1.11x); `git_diff` passes `diff_structural` and fails
`diff` (1.0x); `code_symbols`, `code_grep`, `code_refs`, `find_files` and `git_log` fail every kind on token ratio.

### Baseline per kind

The baseline is the cheapest shell command a competent agent would run to get the same items, scored against the same
gold. `fd` is not installed and the gold generators do not require `rg`, so `git grep -n` (the output shape
of `rg -n`) and `git ls-files | grep` stand in for them; ADR-0022 names a targeted `git grep` too. `read_around` is the
exception, below.

| kind | baseline |
|---|---|
| `symbols` | `git grep -n -E` for a definition keyword followed by a name matching the query (substring, exact, prefix, or a subsequence for `fuzzy`) |
| `outline`, `read` | reading the whole file (`read` greps for the `fn` line first) |
| `references`, `callers` | `git grep -n -w -F NAME -- '*.rs'` |
| `implementations` | `git grep -n -E 'impl ... TRAIT ... for '` |
| `dependents` | `git grep -l -E 'use .*a::b'` |
| `grep` | the same `git grep -n -F` / `-E` the gold comes from |
| `find` | `git ls-files \| grep -i -F NAME`, or a case-insensitive subsequence regex for typos and skeletons |
| `git_search` | `git log --oneline -i --all-match --grep=...` |
| `git_path` | `git log --follow --oneline -- PATH` |
| `git_symbol` | `git log -s -L '/fn NAME[(<]/,/^INDENT}/:PATH'` |
| `blame`, `blame_symbol` | `git blame -L A,B` / `git blame -L '/fn NAME[(<]/,/^INDENT}/'` |
| `diff`, `diff_structural` | `git diff FROM TO -- PATH` / `git diff FROM -- PATH` |
| `impact` | `git grep -l -E` for `use` lines naming the target module as a path segment (for a crate root, every `use` through the crate), once more per depth-1 importer at depth 2 |
| `cycles` | none: finding a cycle means resolving every import to a file and walking the graph, which no shell command an agent would run does. The old baseline, every import line of the repository (about 80k and 110k tokens), measured the repository's size and could not fail, so the kind is judged on correctness alone |
| `grep_changed` | `git diff --name-only REV \| xargs git grep -n -F PATTERN` |
| `find_large` | `git ls-files \| xargs awk ...` printing `<lines> <path>` (the `wc -l` listing with an unterminated last line counted) `\| sort -rn \| awk '$1>=N'` |
| `read_around` | `rg -n -C5 -F -e PATTERN -g '*.EXT'`, its literal output (`grep_context`) |
| `explain` | one command per section the tool answers: the file's definition lines, `git grep -n -w NAME` (callers), `sed -n` from the first attribute or doc comment to the body's end (doc, body, callees), the `impact` depth-1 `git grep -l -E` `use` lines for the definition's file (importers), `git log -n 5 -s --format='%h %an %aI %s' -L '/fn NAME[(<]/,/^INDENT}/:PATH'` (history); scored with the `explain` reader (recall reported, not gated: a reducing kind) |

`read_around` runs `rg` itself: the baseline is the literal output of the command an agent runs for "the lines
around each hit", not a format built for the comparison. rg merges overlapping and adjacent context windows and
prints the path on every row (`path:N:text` for a match, `path-N-text` for context, `--` between groups) because
that is what it prints without a tty, as the harness runs it. `-g '*.EXT'` keeps the gold's file set; rg skips
hidden and ignored files `git grep` would search, which counts against the baseline's recall. rg is pinned in
`mise.toml`; `run_gate.sh` still refuses to run this kind without it on PATH and records `rg --version` in
`run.tsv`.

### What the gold cannot cover

- Name-based, not scope-resolved (`rust-analyzer` is not pinned and not installed here): `callers` gold is every call of
  the name, so a call that binds to another item of the same name counts.
- Macro bodies are re-parsed as items, statements or expressions; bodies none of those parse (`select!`, `json!`,
  `tracing` field syntax) only contribute calls found lexically (`ident(`), and no definitions.
- Gold is Rust-only for the code kinds: hits in other languages cost the tool precision, never recall.
- `git_path` and `git_symbol` do not follow renames; `git_symbol` skips merges and compares by name within one file.
- `find` gold is every tracked file, including assets (`.png`, `.svg`) `find_files` does not index.
- Import edges (`impact`, `cycles`) are runtime `use` paths only, as the graph answers by default (ADR-0047
  decision 4): a `mod` declaration builds the module tree but is no edge, and a path in an expression
  (`crate::a::f()`) is none either; `#[cfg]` is ignored, so every variant of a `mod` declared under several
  conditions is walked. A path through the package's own name (from its tests,
  binaries, examples) or a path, `workspace = true` or `[patch]`ed path dependency resolves inside the repository;
  the standard library and registry dependencies are external.
- `docs_search` and `code_search` are not run: both need an embedding model download, and this harness only uses the
  network to clone the corpus. `code_read` spans are judged against the definition's first non-attribute line to its
  last line.
