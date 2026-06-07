---
priority: high
---

# Schema + Blob Compatibility

Gitmind persists state in two serialized forms: msgpack blobs under `.gitmind/blobs/<hash>.{l1,l2}.msgpack` (content-addressed) and the Fjall keyspace under `.gitmind/views/<view>/index.fjall/`. Both have explicit schema versions; both auto-wipe and rebuild on version mismatch.

- Bump the relevant schema constant whenever the serialized shape changes:
  - `INDEX_SCHEMA_VER` in `src/index/mod.rs` for any Fjall partition layout / key encoding change.
  - The blob format version in `src/store.rs` for msgpack value changes.
- Wipe-on-mismatch is the explicit migration story: the next `gitmind scan` rebuilds from source. Never silently accept a mismatched version.
- Backward-compat shims (`#[serde(default)]`, optional fields) are fine inside a major version — e.g. adding `start_row` to `Call` did not require a bump because older blobs still deserialize.
- New variants on stable enums (e.g. `SymbolKind`) extend the tail; do not reorder existing variants because `symbol_kind_byte()` in `src/index/keys.rs` maps them to fixed `u8` ordinals.
- Schema bumps are pre-approved by the project owner; do not block on permission, but mention the bump in the commit body.
