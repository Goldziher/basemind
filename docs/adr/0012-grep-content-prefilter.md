# ADR-0012: Per-file trigram bloom prefilter for `code grep`

- **Status:** Proposed
- **Date:** 2026-10-08
- **Deciders:** pending maintainer review
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
every candidate, so a bloom false positive costs a read, never a wrong result.

1. **Required literals.** Extract necessary substrings with
   `regex_syntax::hir::literal::Extractor` (inner + prefix kinds) after `Hir` translation. Case-
   insensitive classes expand to case variants inside the extractor; if the resulting set is
   infinite, contains an empty literal, or any member is shorter than 3 bytes, skip the prefilter
   and sweep as today. A candidate must contain at least one member of the set (alternation), each
   member contributing the AND of its trigrams.
2. **Storage.** One value per file in a new fjall keyspace (`grep_bloom`, key = `RelPath`, value =
   `content_hash | nbits | bits`), written in the same batch as the file's other index rows from
   `scanner_file.rs` using bytes the scanner already holds, so there is no second read. Deleting or
   re-indexing a file replaces the row in the same transaction. Sized per file (`size/16` bytes,
   min 64 B) so the total stays ~6 % of the corpus.
3. **Query.** A single prefix scan streams the rows (no resident copy), tests the query's trigram
   hashes, and yields candidate indices in window order; `count_all` then runs only on those.
   Candidate order is unchanged, so `file_idx` and therefore every cursor is identical.
4. **Staleness.** The sweep reads live disk content; the bloom reflects scan time. Store the file's
   `(size, mtime_ns)` with the row and have the daemon's watcher invalidate rows on change; a file
   whose recorded stamp differs from a `stat` is always treated as a candidate. Cost is one `stat`
   per candidate only, not per corpus file. Rows missing (not yet scanned) are candidates.
5. **Memory.** Nothing becomes resident: the scan streams rows from the keyspace, per-thread scratch
   is the 1 MiB-capped read buffer. The scan-time write amplification is the 6 % of corpus bytes in
   bloom rows, staged through `index_batch` so the existing byte-budgeted flush bounds it (the
   `stage_bm25` pattern, including the budget-accounting test).

## Consequences

- Selective literals drop from a full sweep to reading tens or hundreds of files.
- Index grows ~6 % of corpus bytes; a keyspace/schema version bump forces one rescan to backfill.
  Backfill can be lazy: a missing row means "candidate", so grep is correct before it completes.
- Patterns with no required 3-byte literal (`.*`, `\w+`, `(?i)` of short strings) keep the full
  sweep; their floor is the tightened sweep.
- A new write path in the scanner and a staleness protocol are the risk surface; both need the
  footprint-gate and equivalence tests described above before this ships.

## Alternatives considered

- **Exact trigram postings** — 68 % of corpus before compression, and per-file posting rewrites on
  every edit; fails the size budget.
- **BM25 / token postings as prefilter** — token granularity cannot answer substring queries;
  false negatives.
- **In-memory trigram or bloom table** — 6 % of corpus (~20 MB on armis) resident per workspace is
  exactly the daemon-memory regression the footprint gate exists to stop.
- **mmap the files instead of read** — not measured here; it still touches every file, so it only
  lowers the per-file constant and cannot reach the target.
