# ADR-0012: Per-file trigram bloom prefilter for `code grep`

- **Status:** Accepted
- **Date:** 2026-10-08
- **Deciders:** Na'aman Hirschfeld (implemented and measured on the armis checkout)
- **Related:** ADR-0008

## Context

`code grep` (`src/mcp/helpers_grep.rs`) is a full-corpus sweep: every indexed file that passes the
`language` / `path_contains` filters is opened and read from disk for every query, then a second
read serves the (at most `limit`) files that contribute hits. On the armis checkout (74k files,
~350 MB) the p50 was ~1.4 s. Results must stay exact: `total_matches`, `total_files_matched`, hit
order, `pack_cursor(file_idx, hit_ordinal)` cursors, context lines and `regex`-crate semantics.

The sweep itself was tightened (reused per-thread read buffer, raw-byte memmem rejection before
UTF-8 validation; see the CHANGELOG). That removes allocator churn but cannot remove the
`open + read + close` of 74k files, which is the remaining floor. Getting to low-hundreds of ms
worst case, and milliseconds for selective literals, needs a prefilter that avoids touching most
files.

### Existing machinery does not fit

The code-search BM25 postings (`index_batch.stage_bm25`) are word/identifier-token postings over
chunks. A regex or literal matches arbitrary substrings (`ocessing` inside `post_processing`,
punctuation, `\(`), so a token index would produce false NEGATIVES, which is unacceptable for an
exact-results tool.

### Measurements (armis checkout, `*.py`, 38,265 files, 216 MB, the 15 `t_grep` task patterns)

| structure                                        | size vs corpus | note                                    |
| ------------------------------------------------ | -------------- | --------------------------------------- |
| exact trigram -> file-id postings (4 B ids)      | 68 %           | 36.6 M entries, avg 957 distinct trigrams/file; far over the 15 % budget even with delta/roaring compression |
| per-file bloom, `size/2` bits (= size/16 bytes), k=2 | 6.25 %     | within budget                           |

Candidate files that survive the trigram test (exact set = lower bound; bloom = what the design would
actually read):

| pattern                       | exact | bloom (6.25 %, k=2) |
| ----------------------------- | ----- | ------------------- |
| `eq9fsw`                      | 3     | 4459                |
| `def to_insert_item`          | 96    | 1205                |
| `class KeyLeadingValue`       | 1     | 557                 |
| `create_mock_response_with_code` | 6  | 225                 |
| `def test_filter_field_hybrid_partial_empty` | 1 | 109      |
| `post_processing`             | 551   | 2502                |
| `class SNMPv3`                | 10    | 2102                |

i.e. 8x to 350x fewer files read than the 38,265-file sweep. Short needles are weakest; raising to
size/8 bytes (12.5 %) or k=3 tightens them.

## Decision

Add a per-file trigram bloom filter as a pure candidate PREFILTER. The real `regex` still runs on
every candidate, so a bloom false positive costs a read, never a wrong result. Implemented in
`src/index/grep_bloom.rs`; as built:

1. **Required literals.** `regex_syntax::hir::literal::Extractor` runs twice on the parsed `Hir`,
   once as `Prefix` (every match starts with one of the literals) and once as `Suffix` (every match
   ends with one). There is no separate "inner" kind in `regex-syntax`; the two necessary
   conditions are combined with AND instead. A set is used only when it is finite, non-empty and every member
   is at least 3 bytes; otherwise that condition is dropped, and with neither usable the grep is
   the unchanged full sweep (`.*`, `\w+`, `ab`, `(?i)k`, `^`). Case-insensitive flags and classes
   are expanded by the extractor itself (or make the set infinite), so they need no special case.
   A file passes when, for every usable set, some member has all its trigrams in the bloom.
2. **Storage.** New fjall keyspace `grep_bloom`, key = repo-relative path, value = `version(1) |
   size(8) | mtime_ns(8) | bits`. The scanner writes the row from the bytes it already holds
   (`WorkerIndexBatch::stage_grep_bloom`, working-tree scans only, no second read), staged through
   the same byte-budgeted batch as BM25 postings so the existing flush bounds and ledger apply.
   Bits = `clamp(size / 8, 64 B, 256 KiB)`, 2 hash positions per trigram (see Sizing).
3. **Query.** For every file in the grep window, one point lookup fetches the row, tests the
   needle's trigram hashes and drops the row; nothing is held between files. The result is one
   `skip` flag per window slot, and `count_all` returns 0 for skipped slots without opening them,
   so window order, `file_idx`, `total_*` and every packed cursor are exactly the full sweep's.
   The lookup runs where the index lives: in-process for a writer session, through
   `HostBackend::host_index_read` for a daemon-hosted connection, and over the socket
   (`IndexReadQuery::GrepBloom`, at most 16,384 paths per request, reply is a bool per path) for a
   `daemon_writer` serve. A forward that fails or an older daemon that does not know the variant
   falls back to the full sweep.
4. **Staleness.** The sweep reads live disk; the bloom reflects scan time. A file is skipped only
   when its row rejects the needle AND a `stat` still shows the exact `(size, mtime_ns)` the row was
   built under. This needs no watcher or daemon protocol and is correct for edits the watcher has not
   yet processed. The `stat` is paid only for files the
   bloom rejects (the cheap majority-case rejection), never a read. A missing, older-version or
   malformed row is a candidate.
5. **Lifecycle and migration.** A rescan replaces a row when a file's bytes are read; a file whose
   content hash is unchanged but whose stamp moved is re-stamped from the bytes in hand; an
   `Unchanged` file with no current row is read once and backfilled; a removed file's row is deleted
   in the same batch as its other index rows (`IndexWriter::remove_file`). The keyspace is new, so
   `INDEX_SCHEMA_VER` is NOT bumped (per the index-keyspace-evolution guidance for a brand-new
   partition): an existing index opens with an empty `grep_bloom`, greps correctly (every file a
   candidate) and fills in on the next scan, with no wipe. The row's own version byte lets a future
   layout change repeat that.
6. **Memory.** Nothing becomes resident. The keyspace joins the cold memtable tier (4 MiB), adding
   20 MiB to the worst-case memtable ceiling (still under the 1 GiB bound the ceiling test
   enforces); the scan's staged-byte ledger covers the rows.
7. **Kill switch.** `BASEMIND_GREP_BLOOM=0` forces the full sweep, for A/B timing and as an escape
   hatch.

### Sizing

Candidate files per pattern on the 38,265 Python files / 216 MB of the armis checkout (the 15
`t_grep` patterns; `exact` is the lower bound where every needle trigram occurs in the file):

| pattern                                      | exact | size/16 k=2 | size/8 k=2 | size/8 k=3 | size/4 k=2 |
| -------------------------------------------- | ----- | ----------- | ---------- | ---------- | ---------- |
| `eq9fsw`                                     | 3     | 3265        | 391        | 521        | 21         |
| `def to_insert_item`                         | 96    | 264         | 115        | 115        | 99         |
| `class KeyLeadingValue`                      | 1     | 38          | 1          | 2          | 1          |
| `def copy_file`                              | 51    | 700         | 103        | 116        | 57         |
| `post_processing`                            | 551   | 1761        | 711        | 744        | 579        |
| `class SNMPv3`                               | 10    | 363         | 23         | 67         | 11         |
| `some_device_name_view(`                     | 12    | 207         | 21         | 25         | 12         |
| index size, % of corpus                      |       | 6.7 %       | 12.8 %     | 12.8 %     | 25.3 %     |

size/8 with k=2 is the choice: it brings the candidate set to within 1-2x of the exact answer for
every pattern but the shortest-needle outliers, at 12.8 % of corpus bytes (40 MB on disk for the
whole 83k-file armis index, ~1 % of the 4.3 GB index). size/16 leaves selective needles 8-10x above
exact; size/4 doubles the index for little extra on realistic patterns; k=3 is worse than k=2 at
this fill.

## Results

armis checkout, 15 `t_grep` tasks (`admin eval --warmup`, release build, machine at load ~50 from
unrelated processes, so wall times are inflated; the on/off runs were interleaved):

| run            | p50       | p95      | P / R / returned results |
| -------------- | --------- | -------- | ------------------------ |
| prefilter off  | 3.6-7.8 s | 4.4-9.9 s | 0.906 / 1.000            |
| prefilter on   | 125-134 ms | 190-268 ms | 0.906 / 1.000          |

Every task's ranked list, returned count and score is identical between all five runs. Whole-process
CPU of the eval run (dominated by cache warm-up, not the 15 greps) fell from ~28 s user / 355-372 s
sys to ~27 s user / ~224 s sys, the saved system time being the avoided `open + read` of the swept
files.

Scan cost, same corpus, same binary lineage (base = this branch without the prefilter):

| scan                                   | user+sys CPU | peak RSS   |
| -------------------------------------- | ------------ | ---------- |
| base, full rebuild (schema migration)  | 1951 + 326 s | 4.96 GB    |
| bloom, full rebuild                    | 2032 + 323 s | 3.68 GB    |
| base, unchanged rescan                 | 196 + 124 s  | 1.15 GB    |
| bloom, unchanged rescan                | 182-223 + 117-135 s | 1.06-1.14 GB |
| bloom, first scan of a base-built index (backfill, no wipe) | 183 + 118 s | 1.09 GB |

Medium corpus (this repository, code only via `--documents-enabled false`, fresh index, two runs each):
peak RSS 497 / 445 MB without the prefilter vs 457 / 468 MB with it, user CPU 6.5 / 5.5 s vs 5.2 / 6.4 s,
index 72.7 MB vs 74.1 MB (+1.9 %).

The rebuild and rescan differences are within the run-to-run noise at this load; peak RSS does not
regress (the rebuild's RSS is dominated by the git-history build and varies run to run). The
backfill of an index that predates the keyspace is as cheap as an ordinary unchanged rescan.

## Consequences

- Selective literals drop from a full sweep to reading tens or hundreds of files.
- Index grows ~12.8 % of the indexed code bytes (about 1 % of an armis-sized index). No schema bump:
  a missing row means "candidate", so grep is correct before the next scan backfills it.
- Patterns with no required 3-byte literal (`.*`, `\w+`, `(?i)` of short strings) keep the full
  sweep; their floor is the tightened sweep.
- A new write path in the scanner and a staleness protocol are the risk surface. They are covered by
  equivalence tests against the unfiltered sweep over a generated corpus with edits after the scan,
  deletions, new files, non-UTF-8 files, long lines and case-insensitive patterns
  (`helpers_grep::tests`), scan/refresh/remove/backfill tests (`tests/grep_bloom_scan.rs`), a
  staged-byte-ledger test, and route-parity tests (`mcp::index_route_tests`).

## Alternatives considered

- **Exact trigram postings** — 68 % of corpus before compression, and per-file posting rewrites on
  every edit; fails the size budget.
- **BM25 / token postings as prefilter** — token granularity cannot answer substring queries;
  false negatives.
- **In-memory trigram or bloom table** — 6 % of corpus (~20 MB on armis) resident per workspace is
  exactly the daemon-memory regression the footprint gate exists to stop.
- **mmap the files instead of read** — not measured here; it still touches every file, so it only
  lowers the per-file constant and cannot reach the target.
