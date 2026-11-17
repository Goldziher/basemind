---
status: accepted
date: 2026-11-17
---

# ADR-0029: Document retrieval pipeline and retrieval store

- **Deciders:** the repository owner
- **Related:** ADR-0028 (group directory, row keys, liveness, collection); ADR-0015, ADR-0023,
  ADR-0025, ADR-0027

## Context

`docs_search` searches document-type files: PDF, Office, HTML, Markdown, email, notebooks and plain
text. Code has its own search and is not part of this record. Everything runs locally.

An audit of the branches `fix/storage-audit` and `fix/verb-tool-audit` found:

- **One lane.** The search is a dense nearest-neighbour query. Documents have no lexical lane,
  although the configuration says documents excluded from embedding stay searchable by keyword. If
  the embedder cannot load, the tool fails.
- **Filters after the cut.** `mime_type` is applied before ranking. `keywords` and `entity` are
  applied to the 400 nearest chunks, per document, by reading each document's blob.
- **Rerank drops results.** The reranker returns its `top_k` (default 10) and the rest are
  discarded, so a reranked search cannot be paged past 10. A reranker error fails the call.
- **No index.** The LanceDB tables have no scalar, full-text or vector index. Every query scans
  every vector of the workspace.
- **One store per worktree,** keyed by path, so each worktree holds its own copy of the same rows.
  The embeddings themselves are stored once, in the content-addressed document blobs.
- **Chunks lose their structure.** xberg reports page range, heading path and chunk type; basemind
  keeps byte offsets and text. The fixed-size chunker restarts inside a word.
- **Weak answers.** A hit has no page, section or line and no surrounding context, and repeats its
  whole document's keywords, entities and summary.

The corpus is small by nature. A repository's documents are counted in hundreds or thousands, not
millions of chunks, and the design is sized for that.

## Decision

1. **Scope.** By default only document types are chunked and embedded. JSON, YAML, TOML, CSV and
   logs are left to `code_grep`.
2. **Store.** One LanceDB directory per repository group and embedding model, at
   `cache/groups/<group>/lance/<model>-<dim>-<hash>/` (ADR-0028). It holds `doc_chunks`, one
   immutable row per `(doc_key, chunk_ord)`, and `docs`, one row per `doc_key`; code-chunk rows
   keyed by `cck` live beside them. Rows carry no path and no scope. `doc_key` is the 16-byte
   blake3 of content hash and document extraction digest, recorded in the file map entry by the
   scan that wrote the rows.
3. **Liveness** comes from ADR-0028's live-key map, built from the view's file map. It is a
   pre-filter, `doc_key IN (live keys)`, intersected in memory with the `path` and `keywords`
   selections, while the list holds at most 20,000 keys. Above that it is applied after ranking
   with a bounded widening (3x, doubling, cap 4096), and a search that reaches the cap says so.
4. **Two lanes, fused.** A vector lane and a LanceDB full-text lane (BM25, lower-case, ASCII
   folding, English stemming) run under the same predicate, 200 candidates each. They are fused by
   reciprocal rank, `k = 60`, equal weights, on `(doc_key, chunk_ord)`, and cut at 101 for paging.
   A lexical lane that returns less than a page is rerun once with edit distance 1.
5. **Filters run before ranking.** `mime_type`, `kind`, `entity`, `keywords` and `path` are one
   predicate, backed by scalar indexes. `entity` filters chunks; `keywords` selects documents by
   whole keyword. The `scope` parameter is removed.
6. **Exact first.** The vector lane scans exactly whenever the pre-filtered row count is at most
   50,000. A vector index is built only for a store above that, and only when the benchmark shows
   exact search over its latency budget at that size.
7. **Rerank reorders, it does not cut.** The reranker scores the first 50 fused hits; the rest
   follow in fused order. A reranker failure is reported on a successful response.
8. **Answers carry context and a citation.** After the page is cut, adjacent hits of one document
   are merged, each hit is extended by one chunk on either side while the token budget allows, and
   the same content at several paths is one hit listing the other paths. A hit cites path, page
   range, section path, line range and byte range; a field the format lacks is null.
9. **The response says what ran:** the lanes that ran and those that could not, with a reason;
   exact or approximate; fuzzy or not; reranked or not. No lane able to run is an error. A degraded
   answer is not cached.
10. **Chunking keeps what xberg reports.** Page range, heading path and chunk kind are stored per
    chunk; tables repeat their header; the fixed-size chunker starts at a line or word boundary;
    the heading breadcrumb goes into the embedded text only. HTML entities are decoded once,
    outside code, if the stage-0 probe finds them in chunks and no extractor setting removes them.
    The embedding preset decides chunk size for documents xberg chunks.
11. **Store class (ADR-0025).** Derived, class (a): version is the release minor in `meta.json`
    with model and dimension; older is wiped, newer refused. It is rebuilt from document blobs. A
    scan first asks the store which of its keys are absent and reads blobs only for those. Only
    the group lock holder writes. Dead rows are deleted by ADR-0028's sweep, which hands this
    store the live keys; the store lists no file maps itself.
12. **Backend.** LanceDB stays, with scalar and full-text indexes on, if it meets every row below
    on the reference machine, against the harness's own brute-force result with the filter applied
    first, at 1%, 10% and 100% filter selectivity and with 50% and 90% of rows live:

    | Metric | 50k chunks | 250k chunks |
    |---|---|---|
    | recall@10 and nDCG@10 against exact | >= 0.99 | >= 0.97 |
    | vector lane p95, warm | <= 50 ms | <= 100 ms |
    | lexical lane p95, warm | <= 30 ms | <= 60 ms |
    | liveness list of 2,000 / 20,000 keys, added p95 | <= 10 ms | <= 25 ms |
    | cold open plus first query | <= 1.0 s | <= 1.5 s |
    | index build | <= 30 s | <= 3 min |
    | size on disk / raw vector bytes | <= 1.6x | <= 1.6x |
    | after appending 1% of rows, before upkeep | same recall, p95 <= 1.5x | same |

    A 1M-chunk tier is measured and published as the documented limit; it does not decide. If
    LanceDB fails a row, the Fjall arm below is built and measured; if that passes where LanceDB
    failed, a new ADR replaces this clause. Otherwise LanceDB stays and the largest passing tier
    is the documented limit.
13. **Order and format.** The store needs ADR-0028's group directory, lock and live-key map, so it
    lands after the group database, beside the re-keying of the index, and before the group
    sweep. It has no format bump of its own: its version is the release minor, and the new chunk
    fields ride the 0.34.0 blob directory. ADR-0028's one index revision bump is the only other.

## Consequences

- A second worktree of a clone adds no rows and embeds nothing for unchanged documents.
- A search works without the embedder, on the lexical lane, and says so.
- **Breaking.** `docs_search` output changes shape: `cite`, `chunk`, `also_at` and the lane
  report arrive; per-hit `keywords`, `entities` and `summary` go. `keywords` matches whole
  keywords, `scope` is gone, and data and config files leave the corpus. All of it rides 0.34.0.
- Vectors are stored twice, in the blob and in the store: about 300 MB each per 100,000 chunks of
  768 dimensions. The blob copy is what makes a wiped store cost inserts instead of embedding.
- That holds per blob. A blob is keyed by content hash alone, so two worktrees with different
  document settings or embedding models replace each other's blob, and rebuilding the rows of the
  displaced one re-extracts and re-embeds.
- Lexical scores use the statistics of the whole group table, dead rows and other worktrees'
  rows included. Fusion uses ranks only, so the effect is on order within the lexical lane.
- Index upkeep is added to the end of a scan.
- One English analyzer: documents in other languages are matched unstemmed.
- LanceDB remains about 190 lockfile packages. It is on crates.io and does not block publishing.

## Alternatives considered

- **Fjall rows, an exact scan over quantized vectors and the existing BM25 code** — links nothing
  new and removes about 190 packages. Not chosen now: it means owning filtering, full-text search,
  fuzzy matching and concurrent access. It is built only if LanceDB fails the table above.
- **SQLite with sqlite-vec** — rejected, and not benchmarked: no SQLite is linked (ADR-0025), and
  its approximate indexes exist only in alpha releases.
- **Liveness only after ranking** (the first design) — rejected as the default: with many
  worktrees holding versions of one document, most of a lane's candidates are another worktree's
  rows, and the depth left for this worktree collapses.
- **`doc_key` recomputed from the workspace's current settings** — rejected: a setting edited
  since the scan makes every row look dead, and collection cannot know another worktree's settings.
- **`cache/repos/<group>/retrieval/`** — rejected: one directory, `groups/`, holds everything a
  collector may delete, and `repos/` keeps what it may not.
- **LanceDB's built-in hybrid query** — rejected: one failing lane fails the search, and it does
  not report per-lane ranks.
- **Keep per-worktree stores and publish copies to a shared one** (`src/lance/shared*.rs`) —
  rejected: every row is written twice, behind a snapshot each scan invalidates. Only its widening
  projection survives, for the over-threshold case of rule 3.
- **Filter after ranking with a deeper cut** — rejected: a selective filter still misses matches.
- **A vector index for every store** — rejected: at this corpus size exact search is fast enough
  and has no recall to tune.
- **Query rewriting, sparse or late-interaction vectors, semantic chunking** — not adopted: more
  models to ship for a gain nobody has measured here.
