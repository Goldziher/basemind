---
status: accepted
date: 2026-11-18
amends: ["0036", "0041"]
---

# ADR-0043: Packed call name rows and cheaper hit expansion

- **Deciders:** the repository owner
- **Related:** amends ADR-0041 (group format) and ADR-0036 rule 7 (format rebuilds); ADR-0028
  rules 2, 6 and 7 (content-keyed rows, dropped keyspaces, hit keys)

## Context

The index read gate of ADR-0028 (`benches/index_read`, at most 10 % p95 regression at three
content versions against the per-worktree baseline `ac4b7225`) still failed after ADR-0041 for the
name scans: `references` and `callers` (+44 % to +82 %) and the exact symbol lookup (+45 % to
+75 %), at one content version as much as at three, so the cost was the read path, not dead rows.
A profile of `references/exact/hot` at three versions put it in two places:

- `ExpandHits::next` itself was 20 % of the scan: every hit key (`row ‖ path`) was built byte by
  byte into a fresh `Arc<[u8]>`, and every row's content key was hashed into the live keys' map.
- Rows of contents other views hold are read and skipped: 25 % more rows than the baseline at
  three versions, each a full LSM step, for the same 2 000 hits.

With rules 1 to 5 in place the gate still failed for the exact symbol lookup (`symbol_exact/hot`,
+17 % to +27 % at three versions), and the cost grew with the version count: at ten versions about
half the rows a name scan reads are dead, contents only other views hold. A profile of the lookup at
three versions put 60 % of it in fjall's iterator (merge, MVCC filter, block decode), the same per
row whether the row is live or dead, and 13.5 % in finding each row's content among the live keys: a
bucket bound, then a run, two dependent loads at addresses unrelated to the previous row's.

## Decision

1. **`calls_by_callee` packs its sites (amends ADR-0028 rule 2 for call name rows).** One row per
   callee and content, keyed `u16:len(callee) ‖ callee ‖ content key:16`; its value is the start of
   every call of that callee in the content, `u32_be` each, ascending and distinct. A writer derives
   the rows from the extraction (no decode); a reconcile or a sweep derives the keys to remove from
   the stored `calls_by_path` values, one per callee. `symbols_by_name` keeps one row per symbol: a
   name is defined about once per file, so packing would not shorten its scans.
2. **Hit keys and cursors are unchanged (ADR-0028 rule 7).** A view expands a row into its sites in
   start order and each site into one hit per live holder, `callee ‖ content key ‖ start ‖ path`:
   the same bytes, order and cursors as before. A site no live path holds is still yielded as its
   bare site key, one per site, so the examined-keys cap counts what it counted.
3. **Hit expansion composes keys into reused buffers.** A scan composes each hit key into one of
   two buffers it owns, writing a buffer again once its reader dropped the key; a reader that keeps
   keys gets a fresh buffer. The live keys hold their distinct content keys as a sorted array of
   big-endian integers indexed by their leading bits (content keys are uniform hashes, so a lookup
   reads one bucket bound and about one key), instead of a hash map. The exact symbol lookup reads
   holders straight from the rows without composing hit keys or decoding a row no live path holds.
4. **Scan-read keyspaces store every key whole.** `symbols_by_name`, `calls_by_callee`,
   `implementations_by_trait` and `calls_by_path` are created with a data-block restart interval of
   one. With fjall's default (10 / 16) a key sharing a prefix with the one before it is rebuilt into
   a fresh allocation on every read; whole keys are read as slices of the cached block. Measured on
   the bench's symbol scan, a raw row read drops from about 120 ns to about 100 ns, under the
   per-worktree baseline, which pays for the rows of other views' contents a group index reads.
5. **Group format 6 and index revision 7.** A group database of format 5 is discarded on open and
   rebuilt by the next scan (rule 7). The restart interval is persisted when a keyspace is created,
   so `INDEX_PARTITION_REVISION` moves to 7 (as `+5` and `+6` did for other create-time options): an
   index of the earlier revision is dropped and rebuilt. The same revision drops `imports_by_module`
   and `imports_by_path`, which ADR-0028 rule 6 dropped and which were still written though nothing
   read them.
6. **Live keys are looked up in scan order.** The rows of one name (and symbol kind) arrive in
   ascending content-key order, so a scan looks each row's content up from a cursor over the sorted
   live keys: it steps forward over at most eight runs, on the same or the next cache line, and
   looks a key behind it, or further ahead, up by its bucket. A dead row then costs its LSM step and
   a compare or two; nothing else is read for it. The exact symbol lookup reads keys only.
7. **A group database of another format is discarded whole (amends ADR-0036 rule 7).** The open
   renames `index.fjall` aside and deletes it, as a schema mismatch does, instead of clearing
   `meta` and then each keyspace: a crash between those clears left rows of the old layout under
   view ids the next open allocated again. A database holding rows but no incarnation (what such a
   crash left) is discarded the same way.

## Consequences

- Fewer, wider rows: a call-dense name (`get`) reads about a quarter of the rows it read, and the
  rows of other views' contents shrink by the same factor.
- A callee whose calls change in a content rewrites one row instead of one per call; the
  reconcile's "identical extraction writes nothing" still holds, since the name rows follow from
  the content rows.
- A packed row is read whole even when a page stops in its middle; a content with thousands of
  calls of one callee costs one larger value instead of thousands of keys.
- The four scan-read keyspaces lose prefix truncation inside their data blocks; LZ4 takes it back
  on disk (the bench's three-version group index: 78.2 MiB before, 78.4 MiB after), though the
  block cache holds somewhat larger decompressed blocks.
- **The gate passes at three versions.** Interleaved against `ac4b7225` (four baseline runs, three
  of this layout, load average 6 to 15 from other work), p95 of the slowest run of this layout
  against the slowest baseline run: `symbol_exact/hot` +3 %, `references/exact/warm` +3 %,
  `references/substring/hot` -40 %, `callers/hot` -26 %, `bm25/hot` -16 %; every gated operation
  passes.
- **Dead rows still cost at ten versions** (not gated): `symbol_exact/hot` +59 %,
  `references/exact/warm` +45 %, `bm25/hot` +47 %. The cost follows the share of rows other views
  hold, which no layout that keeps rule 7's order removes (see the alternatives).
- `imports_by_module` and `imports_by_path` are gone: a scan writes two fewer rows per import.
- A view's BM25 caches now live only as long as a handle on the view, as its live keys already did.

## Alternatives considered

- **Keep one row per site and only cheapen the expansion.** Measured: copying each hit key into one
  allocation instead of byte by byte left `references/exact/hot` at +45 % to +50 % at one version,
  and the extra rows of other views' contents remain at three.
- **Per-view name rows (the pre-group layout).** Removes the dead rows but writes every name row
  once per worktree, which content keying exists to avoid.
- **Skip dead rows by seeking to the next live key (a merge join with the live keys).** A seek
  opens a new range: about 1 µs (`symbol_exact/cold`, one row) against about 0.13 µs per row
  stepped, so it pays off only past eight consecutive dead rows. Dead rows come singly: 17 % of rows
  at three versions, and at ten, where half are dead, eight in a row has a chance of about 0.2 %;
  the measured 25-worktree group of ADR-0028 (1,349 contents, about 1,120 per worktree) is at the
  three-version ratio. A `calls_by_callee` scan could not skip them anyway: rule 2 yields each dead
  site so the examined-key cap counts it.
- **Cluster rows by liveness.** Liveness is per view, row order is per content key and shared by
  every view (ADR-0028 rule 7), and pages and cursors must stay byte for byte what they are; no key
  layout puts one view's live contents together for every view at once.
- **A per-view liveness bitmap consulted before a row's value is read.** The exact symbol lookup
  reads no value, and a packed call row's value is its sites, which the cap counts; a bitmap would
  only replace the lookup the scan-order cursor (rule 6) already makes a compare.
- **A new structured-hit read API instead of hit keys.** Avoids composing keys entirely, but
  changes the `IndexView` contract the reference scans and their cursors are built on; the buffers
  reach the gate without it.
