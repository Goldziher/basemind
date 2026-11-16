# Retrieval backend benchmark

Measures stores for document retrieval on one synthetic corpus and applies the decision rule of ADR-0029, rule 12
(`docs/adr/0029-document-retrieval-pipeline-and-store.md`). Nothing here
changes what basemind does: the harness is the `retrieval_bench` example, it takes every directory as an argument and
never opens basemind's data home.

| Arm | Store | Vector lane | Lexical lane |
|---|---|---|---|
| A | LanceDB as basemind uses it today: no index | filtered flat scan | none |
| B | LanceDB with BTree, Bitmap, LabelList and FTS indexes | exact flat scan; with `--ann`, IVF_HNSW_SQ above 50 000 pre-filtered rows | Lance FTS (BM25) |
| C | Fjall rows and postings plus memory-mapped vector files; links nothing new | exact SQ8 scan, f32 rescoring | BM25 over Fjall postings |

Arm A is the reference. Arm B is judged. Arm C is measured only if arm B fails a row (`ARMS="A B C"`), and judged
when present. There is no SQLite arm: the ADR rejects it unmeasured.

## Run

```sh
export CARGO_TARGET_DIR=<target dir>
cargo build --release --features documents --example retrieval_bench

work=$(mktemp -d)
BENCH_BIN=$CARGO_TARGET_DIR/release/examples/retrieval_bench \
  benchmarks/retrieval/run.sh "$work" "$work/results.ndjson" 50000 250000
python3 benchmarks/retrieval/decide.py "$work/results.ndjson" --markdown table.md
```

`run.sh` needs an existing empty work directory and deletes each corpus and store once it is measured (`KEEP=1` keeps
them for the probes). Disk needed at the peak: about 4 GB for 250 000 chunks and about 16 GB for 1 000 000. `B_KNOBS`
passes arm B flags (`--ann`, `--nprobes`, `--refine-factor`, `--ef`, `--exact-max-rows`), `ARMS` selects the arms.

`decide.py` accepts several files; a later file replaces an earlier line for the same measurement, which is how a
tuning run or a deliberately bad run is judged without editing the committed results. It exits 0 when arm B passes
every tier present, 1 when arm B fails a check and 2 when a judged tier lacks a measurement.

## What is measured

Each step is one process, so its peak RSS (from `/usr/bin/time`) and its cold open are its own.

- **Corpus** (`gen <n> <dim> <seed> <dir>`): `n / 1000` unit-norm cluster centres; a chunk vector is
  `normalize(centre + 0.35 * g)` and a query is `normalize(row + 0.25 * g)`, where `g` is an isotropic Gaussian of
  expected norm 1. Twenty consecutive chunks form a document from one cluster. Chunk text is 120 tokens, 70% from the
  cluster's 30-word vocabulary and 30% Zipf-distributed over 50 000 words. `email` is 1% of documents and `pdf` 10%, which
  gives the 1% and 10% selectivities. 200 queries. The corpus is written as Arrow IPC batches of 4096 rows.
- **Truth** (`truth <dir> base|appended`): brute-force L2 and brute-force BM25 over the corpus files, filter first, top 10
  per query. It shares no code path with a backend.
- **Build** (`build <arm> ingest|index|append|optimize`): wall time, on-disk bytes, and for arm B the time per index and
  the rows each index covers.
- **Query** (`query <arm> --selectivity 1|10|100 --live 100|90|50 --state base|appended|optimized`): the cold open and
  first query of the fresh process, then 20 untimed and 200 timed queries per lane; p50 and p95 are nearest-rank. Each
  lane fetches `k = 200` candidates, as the pipeline does, and recall@10 and nDCG@10 are scored on the first ten by
  `basemind::eval::score`.
- **Liveness.** `--live 90` marks 90% of documents live. Their keys become a `doc_key IN (...)` predicate beside the
  metadata filter while the list holds at most 20 000 keys; a longer list is applied after ranking by over-fetching
  (start at `max(3k, k + 64)`, double, cap 4096) on the vector lane, and the run reports how many queries stayed
  short at the cap. `--key-list <count>` instead filters on a list of exactly that many keys (the corpus's first
  documents, padded with absent keys) and is timed only: it prices the list itself.

Lexical recall is reported against the brute-force BM25 truth but is not judged: the rule only bounds lexical latency.

## Decision rule

LanceDB stays if arm B meets every row below at both deciding tiers, at 1%, 10% and 100% filter selectivity crossed
with 100%, 90% and 50% live documents. Latency is warm p95.

| Metric | 50 000 | 250 000 |
|---|---|---|
| recall@10 and nDCG@10 against exact | >= 0.99 | >= 0.97 |
| Vector lane p95 | <= 50 ms | <= 100 ms |
| Lexical lane p95 | <= 30 ms | <= 60 ms |
| Liveness list of 2 000 / 20 000 keys, added p95 | <= 10 ms | <= 25 ms |
| Cold open + first vector query | <= 1.0 s | <= 1.5 s |
| Index build | <= 30 s | <= 3 min |
| On-disk size / raw f32 vector bytes | <= 1.6 | <= 1.6 |
| After appending 1% of rows, before upkeep | same recall, p95 <= 1.5x | same |

The 1 000 000 tier is measured when disk allows and published as the documented limit; it does not decide. If arm B
fails a row, arm C is built and measured; if C passes where B failed, a new ADR replaces the backend clause. Otherwise
LanceDB stays and the largest passing tier is the documented limit.

How `decide.py` reads the rows the ADR leaves open:

- **Liveness list.** p95 of an unfiltered query under a list of that many keys, minus p95 of the same query without
  one, per lane. Both list sizes are held to the tier's limit.
- **Index build.** Arm B: the `index` step. Arm C writes postings and SQ8 codes while ingesting, so its figure is its
  `index` step plus whatever its ingest takes beyond arm A's.
- **Same recall after an append.** Each state is scored against its own truth; a loss of at most 0.005 passes.
- **1 000 000.** Shown against the design report's proposed limits (0.95, 250 ms, 120 ms, 2.5 s, 15 min).

## Results

`results/` holds the committed run:

| File | Content |
|---|---|
| `results.ndjson` | the matrix: arms A, B and C at 50 000 and 250 000, arm B at 1 000 000 (100% live only) |
| `table.md` | what `decide.py results.ndjson` printed |
| `ann.ndjson` | arm B with `--ann` at 250 000 and 1 000 000: index build, and queries with and without refinement |
| `degraded.ndjson` | arm B at 250 000 forced onto the unrefined vector index, which `decide.py` must reject |
| `probes.ndjson`, `doc_probes.ndjson` | raw output behind `PROBES.md` |

`PROBES.md` answers the questions the design could not settle by reading.

Reference machine: Apple M4 Pro, 14 cores, 48 GB, macOS, internal SSD. Cold figures are a fresh process with a warm
page cache; the page cache was not purged. **The machine was not idle.** Other builds ran throughout; the one-minute
load average per step is in the `rss` lines (median 10 at 50 000, 18 at 250 000, 43 at 1 000 000, on 14 cores). Each
query step therefore times its 200 queries three times and reports the pass with the lowest p95 (`pass_p95_ms` has all
three). Latencies near a limit should be re-measured on an idle machine before they decide anything; recall, sizes and
the large effects below do not depend on it.

What the run shows:

- **`decide.py` prints: arm B fails a row that arm C passes at the same size.** Exit status 1.
- Arm B is exact everywhere, so recall@10 and nDCG@10 are 1.000 in all 18 cells per tier. With no liveness list its
  vector lane p95 is 12 ms at 50 000 and 61 ms at 250 000 (limits 50 and 100), and its lexical lane stays under 6 ms.
  Index build is 1.4 s and 4.9 s, size 1.17x and 1.16x raw vectors, cold open plus first query 17 ms and 60 ms.
- Arm B fails three kinds of row:
  - **A 20 000-key liveness list** adds about 65 ms to the vector lane's p95 at both tiers (limits 10 and 25) and 18 ms
    to the lexical lane's at 50 000 (limit 10). A 2 000-key list adds under 5 ms. The 20 000-key list holds more keys
    than either corpus has documents; the cost is the list, not the rows it keeps.
  - **After a 1% append and before upkeep** the lexical lane's p95 is 5.3x what it was at both tiers: the FTS index
    does not cover the new rows and they are scanned. In absolute terms that is 4 ms at 50 000 and 17 ms at 250 000,
    inside the lexical limit; it is the ratio that fails. The vector lane's ratio is 1.55 at 250 000, against 1.5.
  - **90% live at 250 000** (an 11 258-key predicate): vector p95 113 ms against 100. This one is within the noise of
    the loaded machine: the other eight liveness cells of the tier pass, at 39 to 97 ms.
- Arm C passes every row arm B fails, and fails two of its own: on-disk size (1.91x raw vectors at 50 000, 1.67x at
  250 000, because it keeps f32 and SQ8 copies beside the text and postings) and index build at 50 000 (its ingest
  took 33 s there and 36 s at 250 000; a first run at 50 000 took 130 s). Its ingest is the unoptimized part of a
  prototype, so that row says little; the size row is structural.
- At 1 000 000 chunks (published, does not decide) arm B's exact vector scan has a p95 of 431 ms against a proposed
  250 ms: that size needs the vector index. `ann.ndjson` has it: recall@10 0.9995 at 7 ms p95 with
  `refine_factor(5)`, for 223 s of index build and 4.8 GB of peak memory.
- **The harness can fail.** `decide.py results/results.ndjson results/degraded.ndjson` turns the three recall rows of
  the 250 000 tier from `1.000 ok` to `0.967`, `0.956` and `0.948 FAIL`. Arm A, which is exact search, scores recall
  1.000 against the brute-force truth in every cell; anything else would be a harness bug.

## Document eval baselines (not run)

The stage gates of the redesign compare `basemind admin eval --mode docs` against a committed baseline on three task
sets: `benchmarks/eval/gen_docs.py` (a verbatim sentence), `gen_docs_headings.py` (a heading trail, which is not
verbatim) and `gen_docs_filtered.py` (a sentence plus the gold file's `mime_type`). The baselines are not committed:
producing them needs a `documents` build of the `basemind` binary, a scan and the embedding model. To produce them,
on each of two repositories, with a data home that is not yours:

```sh
export BASEMIND_DATA_HOME=$(mktemp -d) BASEMIND_COMMS_DIR=$(mktemp -d)
E=/path/to/basemind/benchmarks/eval
cd /path/to/repo && basemind scan
python3 $E/gen_docs.py          --repo . --seed 1 --n 60 --out docs.jsonl
python3 $E/gen_docs_headings.py --repo . --seed 1 --n 60 --out docs_headings.jsonl
python3 $E/gen_docs_filtered.py --repo . --seed 1 --n 60 --out docs_filtered.jsonl
for set in docs docs_headings docs_filtered; do
  basemind admin eval --tasks $set.jsonl --mode docs --warmup --report baseline.$set.json
done
```

Later stages gate with `--baseline baseline.<set>.json --tolerance 0.02 --cost-tolerance 0.25`. `gen_docs_filtered.py`
prints the task count per MIME type; a repository with one document type makes every filter keep the whole corpus.

## Limits of the harness

- The corpus is isotropic inside a cluster: the thousand members of a cluster are almost equidistant from a query. Real
  embeddings have lower intrinsic dimension, so ANN recall here is a pessimistic bound and exact-scan cost is unaffected.
- Arm C is a measurement prototype. It has no concurrency control, no delete path and no crash recovery across its flat
  files; those would be owned code if it were chosen.
- Arm A's and arm B's Lance client runs on a multi-thread tokio runtime. `LanceStore` in `src/lance/mod.rs` uses a
  current-thread runtime; pass `--current-thread` to reproduce that.
