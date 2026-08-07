//! Single source-of-truth for release-version-derived constants.
//!
//! `RELEASE_MINOR` is the only place the persisted-schema version is declared. The blob
//! format (`crate::extract::SCHEMA_VER`), the inverted-index format
//! (`crate::index::INDEX_SCHEMA_VER`), and the git cache
//! (`crate::git_cache::GIT_CACHE_SCHEMA`) all read from it, so a minor-release bump
//! invalidates every cache on next scan. The invalidation is durable, not destructive:
//! `Store::open` resets each view's `index.msgpack` and the Fjall index, then the next
//! scan re-extracts every file — overwriting stale-schema blobs in place at their
//! content-hash path. Orphaned blobs are reclaimed by `store_gc::run_gc`, so the expensive
//! content-addressed blob store is never `rm -rf`'d out from under a live cache.
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
pub const RELEASE_MINOR: u16 = 24;

/// Reported when the running executable cannot be located or read.
pub const UNKNOWN_BUILD_ID: &str = "unknown";

/// A short identity for the RUNNING BINARY, distinguishing builds that share a version string.
///
/// `version` alone cannot tell two builds apart. A `cargo install --path .` build and the published
/// release both report the same `X.Y.Z`, so a resident daemon left over from the previous build is
/// indistinguishable from one built out of the current tree — and gets reused, silently answering
/// with stale code while every version check agrees. Diagnosing that means noticing the daemon's
/// start time predates the install, which is not something anyone thinks to check.
///
/// This hashes the executable's own bytes, so two binaries differ here exactly when their content
/// does — including a rebuild of the same commit with a dirty tree, which a git SHA would miss.
///
/// Streamed rather than read into memory (a debug binary is ~150 MB) and computed once, because
/// nothing on a hot path needs it: this exists for the diagnostic surfaces.
pub fn build_id() -> &'static str {
    static BUILD_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD_ID.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else {
            return UNKNOWN_BUILD_ID.to_string();
        };
        let Ok(mut file) = std::fs::File::open(&exe) else {
            return UNKNOWN_BUILD_ID.to_string();
        };
        let mut hasher = blake3::Hasher::new();
        if std::io::copy(&mut file, &mut hasher).is_err() {
            return UNKNOWN_BUILD_ID.to_string();
        }
        hasher.finalize().to_hex()[..12].to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_stable_within_a_process_and_not_the_unknown_sentinel() {
        let first = build_id();
        assert_eq!(first, build_id(), "cached, so repeated calls cannot disagree");
        assert_ne!(
            first, UNKNOWN_BUILD_ID,
            "the test binary is readable, so a real hash must be produced"
        );
        assert_eq!(first.len(), 12, "short hash, stable width for rendering");
    }
}
