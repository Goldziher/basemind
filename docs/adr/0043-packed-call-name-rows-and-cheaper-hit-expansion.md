---
status: proposed
date: 2026-11-18
amends: ["0041"]
---

# ADR-0043: Packed call name rows and cheaper hit expansion

- **Deciders:** the repository owner
- **Related:** amends ADR-0041 (group format); ADR-0028 rules 2 and 7 (content-keyed rows, hit
  keys), ADR-0036

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
5. **Group format 6 and index revision 7.** A group database of format 5 is emptied on open and
   rebuilt by the next scan (ADR-0028's rebuild path). The restart interval is persisted when a
   keyspace is created, so `INDEX_PARTITION_REVISION` moves to 7 (as `+5` and `+6` did for other
   create-time options): an index of the earlier revision is dropped and rebuilt.

## Consequences

- Fewer, wider rows: a call-dense name (`get`) reads about a quarter of the rows it read, and the
  rows of other views' contents shrink by the same factor.
- A callee whose calls change in a content rewrites one row instead of one per call; the
  reconcile's "identical extraction writes nothing" still holds, since the name rows follow from
  the content rows.
- A packed row is read whole even when a page stops in its middle; a content with thousands of
  calls of one callee costs one larger value instead of thousands of keys.
- The four scan-read keyspaces lose prefix truncation inside their data blocks; LZ4 recovers most
  of it on disk, and the block cache holds somewhat larger blocks.
- The bench gate result at 1x, 3x and 10x is recorded in the commit that lands this ADR.

## Alternatives considered

- **Keep one row per site and only cheapen the expansion.** Measured: copying each hit key into one
  allocation instead of byte by byte left `references/exact/hot` at +45 % to +50 % at one version,
  and the extra rows of other views' contents remain at three.
- **Per-view name rows (the pre-group layout).** Removes the dead rows but writes every name row
  once per worktree, which content keying exists to avoid.
- **Seek past a dead content's rows.** A seek costs more than the handful of rows it skips.
- **A new structured-hit read API instead of hit keys.** Avoids composing keys entirely, but
  changes the `IndexView` contract the reference scans and their cursors are built on; the buffers
  reach the gate without it.
