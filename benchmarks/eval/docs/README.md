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
| Markdown | 9 | `text/markdown` | 0.39 |
| HTML | 4 | `text/html` | 0.17 |
| PDF | 3 | `application/pdf` | 0.13 |
| Email | 3 | `message/rfc822` | 0.13 |
| Word | 2 | `application/vnd.openxmlformats-officedocument.wordprocessingml.document` | 0.09 |
| Notebook | 2 | `application/x-ipynb+json` | 0.09 |

## Tasks

86 tasks, `mode: docs`, `k: 10`, gold paths relative to `corpus/`. `meta.variant` is `question`, `keywords`
or `rare_term` (a term the script checks occurs in exactly one document). PDF golds carry `meta.gold_page`.

- `docs-corpus-0001`..`0056`: unfiltered (`meta.selectivity: "none"`).
- `0057`..`0081`: the same queries with `args.mime_type` set to the gold's type.
- `0082`..`0086`: filtered queries whose topic has a near-duplicate in another format; the filter decides.

## Running

```sh
BASEMIND_DATA_HOME=$(mktemp -d) BASEMIND_COMMS_DIR=$(mktemp -d) \
  benchmarks/eval/docs/run.sh <basemind built with --features documents> <out dir>
python3 benchmarks/eval/docs/summarize.py benchmarks/eval/docs/tasks.jsonl <out dir>/results.ndjson
```

`run.sh` copies the corpus to `<out dir>/workspace` with a `basemind.toml` that enables documents, scans it and
runs `basemind admin eval --mode docs`. `summarize.py` breaks the run down by selectivity and variant. Results
before and after the ADR-0029 pipeline are in the polyrepo's `reports/retrieval-eval.md`.
