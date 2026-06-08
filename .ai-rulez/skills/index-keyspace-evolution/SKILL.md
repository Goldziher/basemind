---
priority: medium
description: "Adding Fjall partitions and key encodings safely"
---

# Index Keyspace Evolution

Use this when adding a new Fjall keyspace, extending an existing key encoding, or changing the value shape of an indexed record.

## Decide if you need a schema bump

Bumping `INDEX_SCHEMA_VER` triggers auto-wipe of `.basemind/views/<view>/index.fjall/` on the user's next scan. The next scan rebuilds from the msgpack blobs — no data loss, but a one-time scan cost. Bump when:

- The byte layout of an existing key changes.
- A `SymbolKind` ordinal would shift (you reordered the enum — don't).
- A value's msgpack shape lost a field or renamed one.

Do **not** bump when:

- Adding a brand-new partition (no existing entries to migrate).
- Adding a `#[serde(default)]`-defaulted field to an indexed value.
- Adding a new `SymbolKind` variant at the tail (the `u8` mapping is stable).

## Steps

1. **Encoder + decoder + tests in `src/index/keys.rs`**
   - Add `<partition>_key(...) -> Vec<u8>` and `<partition>_prefix(...) -> Vec<u8>`.
   - Add `parse_<partition>_key(&[u8]) -> Result<(...), …>`.
   - Add unit tests for round-trip + prefix isolation: insert two keys with a shared prefix that differ only in the suffix, scan the prefix, confirm the wrong sibling is excluded.

2. **Open the partition in `src/index/mod.rs`**
   - Add the field to `IndexDb`.
   - Open via `db.keyspace("<name>", PartitionCreateOptions::default())` next to the others.
   - If used only for reads, mark `#[allow(dead_code)]` until a writer / reader exists.

3. **Writer in `src/index/writer.rs`**
   - Extend `upsert_file` to derive secondary keys from the L1/L2 input and stage inserts.
   - Extend the read-before-write deletion to enumerate the partition under the file's primary key and stage deletes for those secondary keys.
   - Add integration tests covering upsert-then-remove leaves the partition empty.

4. **Reader in `src/mcp/helpers.rs` (and/or `query.rs`)**
   - Add a `scan_<partition>_by_<key>` helper that range-scans the prefix and decodes values.
   - Apply `scan_cap = limit * 8`.

5. **Bump `INDEX_SCHEMA_VER`** (if needed) and verify the auto-wipe path by:
   - Pointing `basemind scan` at an existing `.basemind/` from a prior version.
   - Confirming the wipe + rebuild completes without panic.

## Verification

- Unit tests in `src/index/keys.rs` and `src/index/writer.rs` cover round-trip + prefix isolation + upsert/remove symmetry.
- `tests/schema_bump.rs` covers the wipe-on-mismatch path; extend it if the bump is non-trivial.
- Harden harness still 8/8 green.
