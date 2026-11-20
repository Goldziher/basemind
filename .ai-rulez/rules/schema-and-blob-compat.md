---
type: Decision
title: Schema And Blob Compat
x-ai-rulez:
  kind: rule
  id: schema-and-blob-compat
  metadata:
    priority: high
---

# Schema + Blob Compatibility

basemind persists state in two serialized forms: msgpack blobs under `<data dir>/cache/blobs/v<schema>/<hash>.{fm,chunk,rref,doc}.msgpack` (content-addressed, one namespace per blob schema) and the Fjall keyspace under `<data dir>/cache/groups/<group>/index.fjall/`, one per repository group, shared by every worktree and view of it (ADR-0028, ADR-0036). Both have explicit schema versions. An older index is wiped and rebuilt, a newer one is refused; blobs of another schema live in another namespace and are never read. The policy is ADR-0025.

- Single source of truth: `RELEASE_MINOR` in `src/version.rs`. Both `INDEX_SCHEMA_VER` (`src/index/mod.rs`) and the blob `SCHEMA_VER` (`src/extract/mod.rs`) read from it. Bump = bump the constant in one file.
- Bump cadence is bound to release versions, not commits:
  - `0.X.y` → `RELEASE_MINOR = X` (e.g. `0.1.0` → `1`, `0.2.0` → `2`).
  - `M.X.y` once `1.0` ships → `RELEASE_MINOR = M * 100 + X`.
  - Patch releases MUST be blob-and-index-compatible — never bump from a patch commit.
- Wipe-on-mismatch is the explicit migration story: the next `basemind scan` rebuilds from source. Never silently accept a mismatched version.
- Backward-compat shims (`#[serde(default)]`, optional fields) are fine inside a minor version — e.g. adding `start_row` to `Call` did not require a bump because older blobs still deserialize.
- New variants on stable enums (e.g. `SymbolKind`) extend the tail; do not reorder existing variants because `symbol_kind_byte()` in `src/index/keys.rs` maps them to fixed `u8` ordinals.
- Schema bumps are pre-approved as part of a minor release cut. Mention the bump in the release notes (CHANGELOG `## [0.X.0]`).
