---
priority: high
---

# Schema + Blob Compatibility

Basemind persists state in two serialized forms: msgpack blobs under `.basemind/blobs/<hash>.{l1,l2}.msgpack` (content-addressed) and the Fjall keyspace under `.basemind/views/<view>/index.fjall/`. Both have explicit schema versions; both auto-wipe and rebuild on version mismatch.

- Single source of truth: `RELEASE_MINOR` in `src/version.rs`. Both `INDEX_SCHEMA_VER` (`src/index/mod.rs`) and the blob `SCHEMA_VER` (`src/extract/mod.rs`) read from it. Bump = bump the constant in one file.
- Bump cadence is bound to release versions, not commits:
  - `0.X.y` → `RELEASE_MINOR = X` (e.g. `0.1.0` → `1`, `0.2.0` → `2`).
  - `M.X.y` once `1.0` ships → `RELEASE_MINOR = M * 100 + X`.
  - Patch releases MUST be blob-and-index-compatible — never bump from a patch commit.
- Wipe-on-mismatch is the explicit migration story: the next `basemind scan` rebuilds from source. Never silently accept a mismatched version.
- Backward-compat shims (`#[serde(default)]`, optional fields) are fine inside a minor version — e.g. adding `start_row` to `Call` did not require a bump because older blobs still deserialize.
- New variants on stable enums (e.g. `SymbolKind`) extend the tail; do not reorder existing variants because `symbol_kind_byte()` in `src/index/keys.rs` maps them to fixed `u8` ordinals.
- Schema bumps are pre-approved as part of a minor release cut. Mention the bump in the release notes (CHANGELOG `## [0.X.0]`).
