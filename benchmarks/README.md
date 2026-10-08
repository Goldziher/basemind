# basemind benchmarks

A small harness that compares a `basemind` CLI command against a `grep`/`rg`/`cat`/`find`
baseline for the same task, on two axes: real token cost and wall-clock time.

## What it measures

For each task, `run.sh`:

1. Runs the `baseline` command (a literal shell command) and captures its stdout.
2. Runs the equivalent `basemind` command and captures its stdout.
3. Pipes **both** captured outputs through `basemind admin tokens --stdin`, which counts
   tokens with the real o200k tokenizer (`src/mcp/tokens.rs::count_tokens`) — the same
   vocabulary a model actually pays for. This is deliberately *not* the `bytes / 4`
   heuristic that `src/mcp/savings.rs` falls back to when the `tokenizer` feature isn't
   compiled in: a benchmark that used the heuristic for one side and the real tokenizer for
   the other (or the heuristic for both) would not be measuring anything real.
4. Times both commands with `date +%s%N` deltas (wall-clock, millisecond resolution).
5. Prints one report row per task (tokens, delta %, milliseconds for each side), plus a
   `TOTAL` row summing every column.

Delta % is `(baseline_tokens - basemind_tokens) / baseline_tokens * 100` — positive means
basemind used fewer tokens than the baseline for that task.

**Timing caveat**: each `basemind <cmd>` invocation here is a fresh one-shot CLI process
that opens the on-disk index from scratch (~2.5-3.5s of startup on this repo's index). A
long-lived MCP server or `basemind serve` amortizes that cost across many calls; this
harness does not, so its basemind-side timing numbers are a worst case, not what an agent
session actually experiences. The token numbers are unaffected by this — they only depend
on stdout content.

**Token-count caveat**: basemind's structured output (headers, column labels, `context_before`
/`context_after`) carries real per-call overhead. For a narrow, single-line match, that
overhead can make basemind's token count *higher* than a bare `grep` line — you will see
negative delta % on tasks like that in the example run below. Basemind's win shows up on
broad or structural queries (an outline vs. `cat`-ing a whole file; a symbol/reference lookup
vs. walking every file in the repo) where the alternative is reading far more raw text. Both
outcomes are real; the harness reports what actually happened, not what would flatter the
tool.

## Out of scope for this round

There is no web-crawl/scrape tier (`basemind web crawl`/`scrape`) in this harness yet. Wiring
that up needs a set of stable, offline fixture pages (or network access + a `crawl`-feature
build) that this round didn't set up — left for a follow-up.

## Prerequisites

1. **Build with the `tokenizer` feature** — this is the one feature both sides of every task
   need, since it's what makes `basemind admin tokens --stdin` work at all:

   ```sh
   cargo build --bin basemind --features tokenizer
   ```

   (`--features documents` also includes `tokenizer`, plus real semantic/document search —
   see the note below on why that's not what the example tasks use.)

2. **The index must be populated.** `run.sh` calls `basemind admin status --json` and fails
   fast with a clear message if `file_count` is `0`. Populate it once with:

   ```sh
   ./target/debug/basemind scan
   ```

   `run.sh` does not run this for you (a full scan can be slow on a large repo the first
   time, and re-scanning on every benchmark run would make timing noise dominate the
   report) — run it once yourself, then re-run `run.sh` as often as you like.

3. `run.sh` also needs `python3` (with `pyyaml`) and `jq` on `PATH`, to parse the YAML task
   file. Both are already required elsewhere in this repo's tooling.

`run.sh` picks up the binary from `$BASEMIND_BIN`, defaulting to
`<repo-root>/target/debug/basemind`. Override it if you built a release binary or a binary
with more features:

```sh
BASEMIND_BIN=./target/release/basemind ./benchmarks/run.sh benchmarks/tasks.example.yaml
```

## Why the example tasks don't use `basemind memory documents`

The `documents` tier is meant to represent basemind's document/prose-search surface as
compared to a `grep`/`cat` baseline. The "correct" command for real semantic search is
`basemind memory documents <query>`, but that mode is gated behind the `documents` Cargo
feature, which pulls in `xberg`'s ONNX runtime (`ort`) for embeddings/reranking/NER. Building
that feature downloads prebuilt ONNX binaries over HTTPS from `cdn.pyke.io` at compile time,
and in this environment that download fails inside `cargo build` with `invalid peer
certificate: UnknownIssuer` — even though a plain `curl` to the exact same URL, from the same
shell, succeeds (a rustls/webpki-vs-OS-trust-store mismatch behind this environment's network
egress, not a real network outage). That made `--features documents` (and `code-search`,
which also touches `ort` via reranking) unbuildable here.

Given that, `benchmarks/tasks.example.yaml`'s `documents`-tier tasks use `basemind code grep
--path-contains <file>` (full-text, structured content search) against a single markdown doc
instead of true semantic search — it only needs the `tokenizer` feature, so it actually runs
in this environment. If you have a binary built with `--features documents` in an environment
where the ONNX download succeeds, swap those two tasks for real `basemind memory documents
<query>` calls; the harness itself doesn't care which CLI surface a task uses.

## Task file format

A YAML file with one top-level `tasks:` list. Each entry:

```yaml
tasks:
  - name: <short id, used as the report row label>
    tier: code_search   # or: documents
    description: <one line of human context>
    basemind: <args appended after the basemind binary, e.g. "code outline src/main.rs">
    baseline: <a literal shell command, e.g. "cat src/main.rs">
```

`basemind` is passed as `"$BASEMIND_BIN" <basemind>` inside `bash -c`; `baseline` is run
verbatim inside `bash -c`. Both can use quotes, pipes, and flags freely. See
`benchmarks/tasks.example.yaml` for six self-test tasks against this repo (four
`code_search`, two `documents`) that need no external fixtures.

Two things worth knowing when writing your own tasks:

- `grep -r`/`find .` over this repo's working tree will also crawl `target/` (hundreds of
  thousands of build artifacts) unless you exclude it — `--exclude-dir=target` for `grep`, or
  `\( -path './target' -o -path './.git' \) -prune -o ... -print` for `find` (plain
  `-not -path './target/*'` filters *output*, not traversal, and is still slow). The example
  tasks all do this.
- `basemind code grep --path-contains <substring>` matches any indexed path containing that
  substring — if the same filename is vendored under multiple plugin directories (this repo
  ships several copies of `commands/bm-init.md`, for instance), pick a search pattern that
  only occurs in the file you actually mean, not just a path fragment that happens to be
  ambiguous.

## Running it

```sh
./benchmarks/run.sh benchmarks/tasks.example.yaml
```

`run.sh` exits non-zero with a clear message before running any tasks if
`basemind admin tokens --stdin` doesn't work (wrong feature build) or the index has 0 files
(needs a scan first).

## Reading the report

Example real run against this repo (`tokenizer`-feature build, index already scanned):

```text
task                             tier             bm_tok   base_tok   delta%      bm_ms    base_ms
--------------------------------------------------------------------------------------------------
callers_of_count_tokens          code_search         161        356    54.8%      3119ms       382ms
outline_init_cli                 code_search         906       8586    89.4%      2593ms        23ms
definition_of_codecmd            code_search          47         14  -235.7%      3316ms       402ms
find_admin_cli_file              code_search          61          7  -771.4%      3199ms       167ms
bm_init_doc_settings_mention     documents           172         48  -258.3%      2862ms        25ms
readme_performance_section       documents            85         59   -44.1%      2954ms        26ms
--------------------------------------------------------------------------------------------------
TOTAL                            (6 tasks)          1432       9070    84.2%     18043ms      1025ms
```

Columns: `bm_tok`/`base_tok` are real token counts of each side's captured stdout;
`delta%` is the token savings of basemind over the baseline (negative = basemind used more
tokens); `bm_ms`/`base_ms` are wall-clock milliseconds (see the timing caveat above). The
`TOTAL` row sums every column across all tasks and recomputes delta % from the summed token
counts (not an average of the per-row percentages).

The big win here is `outline_init_cli` (structural outline vs. reading a 792-line file
whole) — that's what drives the 84.2% overall token delta despite four of the six individual
tasks showing negative delta on their own. That's the expected shape of this benchmark: pick
narrow, single-hit tasks and grep wins on tokens; pick broad or structural tasks and basemind
wins by a wide margin.

## Retrieval quality and token savings: `basemind admin eval`

`run.sh` compares one CLI call to one baseline command per task and reports tokens and
wall-clock. It does not check that basemind's answer was *right*. `basemind admin eval` does:
it runs a JSONL task file against the indexed workspace through the same in-process tool code
the MCP server uses (no per-call process startup), scores each answer against gold, measures
server-side latency and response tokens, and, for tasks that carry a `baseline`, measures what
the grep/read alternative would have cost.

```sh
basemind scan
basemind admin eval --tasks tasks.jsonl --out results.ndjson \
  --report report.json --markdown report.md
basemind admin eval --tasks tasks.jsonl --report new.json --baseline report.json  # gate
```

Task files are generated per repository by the stdlib-only scripts in
[`benchmarks/eval/`](eval/README.md), which also documents the task format, the metrics, and the
comparison with the fixed multipliers in `src/mcp/savings.rs`.

### Measured results

One run of the nine generated task files (`benchmarks/eval/`, `--warmup`, tokenizer `o200k`) against a
74k-file, ~350 MB monorepo checkout (the armis repository), on development builds of the changes listed
under `[Unreleased]` in the CHANGELOG, 2026-10-08. Latency is the in-process tool call; `grep` is taken from
[ADR-0012](../docs/adr/0012-grep-content-prefilter.md) (the run with the trigram prefilter on, 125-134 ms
p50). Token ratios are `baseline_tokens / basemind_tokens` over tasks whose recall reached 0.8; the
baseline is a targeted `git grep` plus up to three whole-file reads.

| mode | tasks | p50 | p95 | P | R | ranked | median token ratio | credited / withheld | `savings.rs` assumes |
|---|---|---|---|---|---|---|---|---|---|
| `symbols` | 60 | 3.7 ms | 5.3 ms | 0.97 | 1.00 | | 21.6x (p90 98.9x) | 60 / 0 | 3x |
| `outline` | 60 | 1.0 ms | 2.2 ms | 1.00 | 1.00 | | 0.68x (p90 1.21x) | 60 / 0 | 5x |
| `references` | 60 | 1.4 ms | 6.0 ms | 0.81 | 1.00 | | 1.20x (p90 1.80x) | 60 / 0 | 3x |
| `callers` | 60 | 4.6 ms | 10.4 ms | 0.87 | 1.00 | | 0.75x (p90 1.12x) | 60 / 0 | 3x |
| `dependents` | 60 | 4.0 ms | 5.6 ms | 0.84 | 1.00 | | 0.64x (p90 0.90x) | 60 / 0 | 2x |
| `grep` | 15 | ~130 ms | 190-270 ms | 0.91 | 1.00 | | 0.41x (p90 0.61x) | 15 / 0 | 1x |
| `find` | 60 | 6.6 ms | 9.0 ms | 0.29 | 0.75 | hit@1 0.72, MRR 0.73 | 0.12x (p90 0.42x) | 18 / 2 | 1x |
| `git_search` | 60 | 55 ms | 96 ms | 1.00 | 0.91 | hit@1 1.00, nDCG 0.94 | 0.16x (p90 0.67x) | 49 / 11 | 1x |
| `docs` | 60 | 106 ms | 260 ms | 0.18 | 0.63 | hit@1 0.48, hit@5 0.63, MRR 0.54 | 0.15x (p90 5.67x) | 38 / 22 | 5x |

How to read it: the ratio only compares answer size with the baseline's. A definition lookup
(`symbols`) replaces a grep plus file reads and is far cheaper than the dashboard's fixed 3x; a
single narrow hit (`outline` of a small file, `references`, `callers`, `dependents`, `find`) can be
*cheaper* with `grep`, because basemind's structured response carries per-call overhead. That is why
the harness flags every mode whose measured median deviates from `src/mcp/savings.rs` by more than 25%
(all of them here) and why `basemind admin telemetry` savings are estimates. The latency columns are
the point of the resident term index, name dictionaries and grep prefilter: the same `symbols`,
`dependents` and `grep` tasks took seconds before them (`symbols` and `dependents` p50 4.1 s and 4.6 s on
the previous implementation; `grep` 3.6-7.8 s). Rerun the workflow in
[`eval/README.md`](eval/README.md) on your own repository before relying on any of these.
