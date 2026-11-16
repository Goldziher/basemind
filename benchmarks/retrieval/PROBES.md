# Probes

Answers, by experiment, to what the retrieval design could not settle by reading source. Versions: lancedb 0.41.0
(lance 14.0.2), xberg 1.4.0 at the revision pinned in `Cargo.toml`. `$BENCH` is the release `retrieval_bench` example,
`$MEASURE` the release `doc_measure` example. Raw output of the Lance probes is in `results/probes.ndjson`.

The repository tracks no PDF, Office, HTML or notebook file, so those probes ran on xberg's own test documents from
the pinned checkout (`$XBERG` = `~/.cargo/git/checkouts/xberg-*/7510f9d`). The Markdown probes ran on this repository's
`docs/`, `README.md` and `CHANGELOG.md`.

## Lance

### Does a vector search tolerate null embeddings?

**Yes. Rows with a null embedding are skipped, with or without a vector index.**

`$BENCH probe nulls <scratch-dir>`: 4000 rows of dimension 16, every odd row with a null `embedding`.

| Search | Result |
|---|---|
| Flat, `k = 10` | 10 hits, none from a null row |
| Flat, `k = 4000` | 2000 hits, none from a null row |
| Flat, `k = 4000`, `embedding IS NOT NULL` | the same 2000 hits |
| `create_index` IVF_HNSW_SQ on the column | succeeds |
| Indexed, `k = 10` and `k = 4000` | 10 and 2000 hits, none from a null row |

The predicate `embedding IS NOT NULL` is not needed, and neither is a second table for documents that are not
embedded. `count_rows("embedding IS NULL")` returns 2000, so the count of rows still owed a vector is one query.

### What does `full_text_search` do without an FTS index?

**It fails.** `$BENCH probe fts-no-index <scratch-dir>` on 20 000 rows:

```text
Invalid input, Cannot perform full text search unless an INVERTED index has been created on at least one column
```

After `create_index(&["text"], Index::FTS(..))` the same query returns hits. The pipeline must therefore know whether
the index exists before it runs the lexical lane (`Table::list_indices`), and report the lane as unavailable until the
first index build; it cannot fall back to a scan by issuing the same query.

Rows appended after the index was built are still found: Lance scans the unindexed fragments. That scan is what the
`after appending 1% of rows` row of the benchmark measures, and it is the lexical lane's cost until
`OptimizeAction::Index` runs.

### What does a `doc_key IN (...)` list of several thousand keys cost on a BTree column?

**Counting the rows it keeps costs 10 to 20 ms at 2 000 keys and 70 to 80 ms at 20 000, growing with the list; a
20 000-key list adds more to a query than the rule allows at both tiers.**

`$BENCH probe inlist <corpus-dir> <store-dir> --keys <n>` on the 250 000-chunk stores (252 500 rows, after the
append), median of 15 runs, exact vector search with `k = 200`. The keys are spread evenly over the corpus, 20 chunks
each. The machine was under other load (one-minute load average about 26 on 14 cores), so read the columns against
each other, not as absolute figures.

| Store | Keys | Rows kept | `count_rows` ms | Search, SQL list ms | Search, `InList` expression ms |
|---|---|---|---|---|---|
| A, no index | 2 000 | 40 000 | 10.0 | 71.6 | 70.9 |
| A, no index | 5 000 | 100 000 | 25.3 | 95.7 | 115.8 |
| A, no index | 20 000 | 250 000 | 81.1 | 316.3 | 319.3 |
| B, BTree on `doc_key` | 2 000 | 40 000 | 19.0 | 64.3 | 29.7 |
| B, BTree on `doc_key` | 5 000 | 100 000 | 19.7 | 97.2 | 70.4 |
| B, BTree on `doc_key` | 20 000 | 250 000 | 72.8 | 188.6 | 142.5 |

- `explain_plan` shows the BTree in use on arm B (`ScalarIndexQuery: query=[doc_key IN [...]]` under
  `KNNVectorDistance`) and a `FilterExec: doc_key IN (SET)` scan on arm A.
- `count_rows` costs about the same with and without the BTree. It only takes SQL text, 68 bytes per key (1.36 MB
  for 20 000 keys). A rule that picks exact or approximate search from the pre-filtered row count pays this on every
  query unless the count is cached per live-key map.
- With the BTree, passing the list as a DataFusion `InList` through `only_if_expr` instead of SQL is 25 to 55%
  cheaper. Without it the two are the same.
- The benchmark's judged rows agree: a 20 000-key list adds about 65 ms to the vector lane's p95 at both tiers and
  18 to 23 ms to the lexical lane's (`results/table.md`). A 2 000-key list adds under 5 ms.

### ANN recall under a selective pre-filter

**IVF_HNSW_SQ at Lance's defaults loses about 5% recall with or without a filter; `refine_factor(5)` restores exact
results; the filter does not hurt recall, it costs latency.**

`$BENCH build B index <corpus> <store> --ann` on 250 000 chunks, then
`$BENCH query B <corpus> <store> --selectivity <s> --exact-max-rows 0 [--refine-factor 5]`, which forces the vector
index even where the rule would search exactly. `k = 200`, recall@10 against the brute-force truth
(`results/ann.ndjson`).

| Filter keeps | Rows | Default: recall@10 / p95 ms | `refine_factor(5)`: recall@10 / p95 ms |
|---|---|---|---|
| 100% | 250 000 | 0.948 / 2.6 | 1.000 / 8.4 |
| 10% | 25 000 | 0.956 / 13.1 | 1.000 / 42.0 |
| 1% | 2 500 | 0.967 / 8.1 | 1.000 / 9.5 |

At 1 000 000 chunks, under the rule (exact at or below 50 000 pre-filtered rows) with `refine_factor(5)`: recall@10
0.9995 at 7.0 ms p95 unfiltered and 1.000 at 48.7 ms with the 10% filter (100 000 rows); unrefined, 0.944 at 2.5 ms.
The exact scan it replaces took 431 ms p95 on the same machine.

- The pre-filter is honoured: every hit satisfied it, and a selective filter raised recall slightly.
- A filtered ANN search is slower than an unfiltered one (the 10% filter is the worst case measured). Refined to
  full recall, it is slower at 1% and 10% than the exact scan of the same rows (6 ms and 20 ms p95 in
  `results/table.md`), so searching exactly below the threshold is right on latency as well as on recall.
- `nprobes(1)` gave the same recall as the default (`results/degraded.ndjson`), so at this size the default already
  probes minimally and the recall loss is quantization, which only refinement recovers. Without `refine_factor`,
  the ANN path fails the 0.97 recall row; `decide.py` rejects that run.
- The corpus is the pessimistic case for quantization (see the README): a thousand near-equidistant neighbours.

### Memory during index build

`/usr/bin/time -l` around the `index` step, one process per step.

| Chunks | Indexes built | Time | Peak RSS |
|---|---|---|---|
| 50 000 | BTree, 2 Bitmap, LabelList, FTS | 1.4 s (FTS 0.8 s) | 156 MiB |
| 250 000 | BTree, 2 Bitmap, LabelList, FTS | 4.9 s (FTS 4.2 s) | 505 MiB |
| 250 000 | the same plus IVF_HNSW_SQ | 86 s (vector index 80 s) | 1 761 MiB |
| 1 000 000 | BTree, 2 Bitmap, LabelList, FTS | 7.4 s (FTS 6.7 s) | 1 119 MiB |
| 1 000 000 | the same plus IVF_HNSW_SQ | 230 s (vector index 223 s) | 4 757 MiB |

The vector index is what costs memory and time: about 4.8 GB and four minutes at a million chunks, with the machine
under other load. With it the store is 1.47x the raw vector bytes at both sizes (1.16x without).

Ingesting holds more than indexing: 584 MiB at 50 000 and 2 180 MiB at 250 000, because the harness hands Lance
64 batches (about 800 MB of vectors) per commit. `OptimizeAction::Index` after a 1% append took 1.8 s at both sizes
and 114 to 227 MiB.

## xberg

Commands: `$MEASURE probe <path> <mime> <plain|markdown> <pages|nopages>`, which calls xberg with the chunking settings
`DocConfig::to_xberg` passes (Markdown chunker, `balanced` preset, OCR off), and
`$MEASURE run <path> <mime> <xberg|linear> none`, which goes through basemind's `extract_doc`.

### Do PDF chunks carry page numbers without `extract_pages`?

**Yes.** Every chunk of both PDFs has `first_page`, `last_page` and `page_spans`, identically with `pages` and
`nopages`, in both output formats.

| File | Format | Chunks | With pages | Example |
|---|---|---|---|---|
| `$XBERG/tools/benchmark-harness/fixtures/split/two_papers.pdf` | plain | 16 | 16 | 1 to 1 |
| same | markdown | 19 | 19 | 1 to 1 |
| `$XBERG/tools/benchmark-harness/fixtures/split/memo_paper.pdf` | plain | 8 | 8 | 1 to 2, 2 to 2 |
| same | markdown | 8 | 8 | 1 to 2, 2 to 2 |

Setting `pages: Some(PageConfig { extract_pages: true, .. })` changes nothing for PDF chunks. A DOCX table
(`merged_cells.docx`) and an XLSX sheet (`merged_header.xlsx`) each report page 1; a notebook (`rank.ipynb`) reports
page 2 for its only chunk, so for a notebook the "page" is not a page and should not be cited as one.

### What does `output_format: Markdown` do to content and chunk offsets?

**The content changes; the offsets stay exact.** In all 26 runs, for every chunk,
`content[byte_start..byte_end] == chunk.content`, so offsets index the content xberg returns in that format. They
do not index the source file, in either format.

| File | Plain: bytes / chunks / heading lines / table lines | Markdown: bytes / chunks / heading lines / table lines |
|---|---|---|
| `two_papers.pdf` | 11 116 / 16 / 0 / 0 | 10 864 / 19 / 2 / 0 |
| `memo_paper.pdf` | 5 794 / 8 / 0 / 0 | 5 539 / 8 / 0 / 0 |
| `docs/ARCHITECTURE.md` | 70 503 / 87 / 0 / 0 | 72 790 / 105 / 30 / 57 |
| `docs/UPGRADING.md` | 19 987 / 27 / 0 / 0 | 21 346 / 34 / 14 / 79 |
| `CHANGELOG.md` | 266 898 / 342 / 0 / 0 | 283 077 / 474 / 247 / 0 |
| `$XBERG/test_documents/html/hip_13044_b.html` | 21 216 / 27 / 0 / 0 | 36 746 / 51 / 5 / 51 |
| `$XBERG/test_documents/html/international_emergency_medicine.html` | 44 544 / 55 / 0 / 0 | 65 846 / 104 / 21 / 9 |

- Plain output drops heading markers and table pipes even from a Markdown source; Markdown output keeps them, and the
  splitter then produces `Heading` and `TableLike` chunks (20 to 90% more chunks).
- `heading_path` is populated in plain format for Markdown, MDX and HTML (87 of 87, 27 of 27, 54 of 55 chunks) but for
  no PDF chunk. With Markdown output, 11 of 19 chunks of `two_papers.pdf` get one; `memo_paper.pdf`, which has no
  detectable headings, gets none.
- So `output_format: Markdown` is what gives PDF chunks a section, and it changes every document's `content`, hence
  every byte offset and every chunk ordinal: it belongs with the extraction digest change of stage 1.
- `README.md` in plain format still shows 111 lines starting with `#`: shell comments inside code blocks, not headings.

### Does notebook cell or spreadsheet sheet identity reach `heading_path` or `node_ids`?

**The sheet name reaches `heading_path`; nothing reaches `node_ids`.** `merged_header.xlsx` has one sheet named
`Fuses` (checked in `xl/workbook.xml`) and its chunk has `heading_path = ["Fuses"]`. `rank.ipynb` has an empty
`heading_path`. `node_ids` is empty for every chunk of every file probed, in both formats.

### Where do HTML entities in chunks come from?

**From the source file, verbatim. xberg does not introduce them and does not decode them.**

- Of 13 files, one has entities in chunk text outside code: `README.md`, in 4 of 85 chunks (plain) and 3 of 96
  (Markdown). Every occurrence is `&amp;`, and every one is present byte for byte in `README.md`, which writes
  `&amp;` in its prose and inline HTML.
- No entity found in any chunk was absent from its source file.
- The three HTML files yield no entity in any chunk: the HTML extractor decodes them.

Decoding once in basemind, outside code, for Markdown-family sources is therefore the right place; no extractor
setting is involved, and HTML sources need nothing.

### What share of chunks start mid-word?

**None on the xberg splitter; more than half on basemind's linear chunker.**

| File | xberg splitter | linear chunker |
|---|---|---|
| `docs/ARCHITECTURE.md` | 0 of 87 | 55.4% of 101 |
| `CHANGELOG.md` | 0 of 342 | 60.3% of 411 |
| `README.md` | 0 of 85 | 61.4% of 101 |
| `llms.txt` (`text/plain`) | 0 of 17 | 84.6% of 13 |

The probe counts a chunk whose first character and the character before it in the content are both alphanumeric.
The xberg splitter does start chunks mid-line (0 to 29% of chunks, `start_midline_pct`), which is its overlap window
landing on a word boundary inside a line. The linear chunker is what large Markdown, plain text and structured data
go through, so the fix belongs in `src/extract/doc_guard.rs` as the design says.
