# docs_search evaluation corpus

A small document corpus and task file for measuring `docs_search` retrieval. Every document is original text
about invented subjects (a river city's water utility, a board game, a small-satellite mission, three
singletons), so the corpus carries no third-party licence. Topics overlap across formats on purpose: the flood
procedure is a PDF and is summarised in an email, the tournament policy is Markdown and is repeated in an
email, so a MIME filter changes the answer.

`gen_corpus.py` (Python 3, stdlib only) writes `corpus/` and `tasks.jsonl`. Output is deterministic; never edit
either by hand. Regenerate with `python3 benchmarks/eval/docs/gen_corpus.py`. MIME strings live in
`MIME_BY_EXT`, task definitions in `BASE_TASKS`, `FILTERED_KEYS` and `FILTER_DECIDES`.

## Corpus

| Format | Files | MIME type | Selectivity |
|---|---|---|---|
| Markdown | 9 | `text/markdown` | 0.31 |
| HTML | 4 | `text/html` | 0.14 |
| PDF | 3 | `application/pdf` | 0.10 |
| Email | 3 | `message/rfc822` | 0.10 |
| Word | 2 | `application/vnd.openxmlformats-officedocument.wordprocessingml.document` | 0.07 |
| Notebook | 2 | `application/x-ipynb+json` | 0.07 |
| Plain text and log | 2 | `text/plain` | 0.07 |
| CSV | 1 | `text/csv` | 0.03 |
| JSON | 1 | `application/json` | 0.03 |
| YAML | 1 | `application/yaml` | 0.03 |
| TOML | 1 | `application/toml` | 0.03 |

## Tasks

93 tasks, `mode: docs`, `k: 10`, gold paths relative to `corpus/`. `meta.variant` is `question`, `keywords`
or `rare_term` (a term the script checks occurs in exactly one document). PDF golds carry `meta.gold_page`;
text and data golds carry `meta.gold_line`, the line that answers the query.

- `docs-corpus-0001`..`0056`: unfiltered (`meta.selectivity: "none"`).
- `0057`..`0081`: the same queries with `args.mime_type` set to the gold's type.
- `0082`..`0086`: filtered queries whose topic has a near-duplicate in another format; the filter decides.
- `0087`..`0093`: unfiltered queries on the text and data files (a log, a CSV, JSON, YAML, TOML, a wrapped
  plain-text note).

## Running

```sh
BASEMIND_DATA_HOME=$(mktemp -d) BASEMIND_COMMS_DIR=$(mktemp -d) \
  benchmarks/eval/docs/run.sh <basemind built with --features documents> <out dir>
python3 benchmarks/eval/docs/summarize.py benchmarks/eval/docs/tasks.jsonl <out dir>/results.ndjson
```

`run.sh` copies the corpus to `<out dir>/workspace` with a `basemind.toml` that enables documents, scans it and
runs `basemind admin eval --mode docs`. `summarize.py` breaks the run down by selectivity and variant, with the
p50 / p95 of response tokens and latency. `EVAL_RERANK=1` turns the reranker on; `EVAL_DISTRACTORS=N` adds N
plain-text distractors (`grow_corpus.py`) that repeat the task queries, so the corpus holds more chunks than a lane's
candidate cut and a filtered task only finds its gold if the filter runs first. `citations.py` checks the first gold
hit's citation (section, PDF page, Markdown lines, the gold line of a text or data file) against the task. The
`docs-eval` workflow (`workflow_dispatch`) runs all three on a release build and uploads the tables.
`summarize.py` exits 1 when the mean recall@10 over all tasks is under `MIN_RECALL_AT_10` (0.5, a conservative
placeholder the owner sets from a measured run), which fails the workflow after the tables are published. Results
before and after the ADR-0029 pipeline are in the polyrepo's `reports/retrieval-eval.md`.
