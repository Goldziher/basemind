//! Single source-of-truth for release-version-derived constants.
//!
//! `RELEASE_MINOR` is the only place the persisted-schema version is declared. The blob
//! format (`crate::extract::SCHEMA_VER`) and the inverted-index format
//! (`crate::index::INDEX_SCHEMA_VER`) both read from it, so a minor-release bump wipes
//! `.basemind/blobs/` and `.basemind/views/<view>/index.fjall/` together on next scan.
//!
//! Bump cadence — bound to release versions, not to commits:
//! - `0.1.x` → `RELEASE_MINOR = 1`
//! - `0.2.x` → `RELEASE_MINOR = 2`
//! - `1.0.x` → `RELEASE_MINOR = 100` (decimal `major * 100 + minor` keeps the value
//!   monotonic across the 0.x → 1.x boundary without forcing patch-level wipes).
//!
//! Patch releases (`0.1.0` → `0.1.1`) MUST be blob-and-index-compatible — never bump
//! `RELEASE_MINOR` from a patch commit; if a serialized shape change is required, it
//! gates the next minor.

/// Persisted-schema version. Synced to the release minor: `0.X.y` → `X` (and
/// `M.X.y` → `M * 100 + X` once `1.0` ships).
pub const RELEASE_MINOR: u16 = 0;
