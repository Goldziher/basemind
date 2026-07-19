//! Content fingerprint of the indexed file set — the guard that lets a refresh SKIP a whole-corpus
//! [`MapCache`](super::MapCache) rebuild.
//!
//! Both refresh paths on a `daemon_writer` serve — the view watcher (`index.msgpack` was rewritten)
//! and a forwarded `rescan` — used to rebuild the entire map unconditionally. The daemon rewrites
//! `index.msgpack` after EVERY scan, including a scan that changed nothing (`updated: 0,
//! removed: 0`, the common case under editor / gitignored churn), so serve reconstructed the whole
//! corpus — re-reading every L1 and L2 blob — for zero actual change. Worse, the OLD map stays
//! resident in `state.cache` while the new one is built beside it, so each rebuild transiently
//! DOUBLES resident memory. That is the sawtooth in serve's RSS.
//!
//! The fingerprint closes it: if every `(path, content-hash)` pair is unchanged, the L1/L2 blobs
//! behind the map are byte-identical, so any map built from the new store would be identical to the
//! one already in hand. Reusing it is not an approximation — it is the same map.

use crate::store::Store;

/// Non-zero so the fingerprint of an EMPTY index never collides with the `0` carried by the
/// [`MapCache::empty`](super::MapCache::empty) boot placeholder — a serve still warming its map must
/// never mistake the placeholder for a current one and skip the build.
const FINGERPRINT_SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// Fixed hasher seeds: the fingerprint must be reproducible for identical input, so it cannot use
/// `ahash`'s per-process random state.
const HASHER_SEEDS: [u64; 4] = [
    0x243f_6a88_85a3_08d3,
    0x1319_8a2e_0370_7344,
    0xa409_3822_299f_31d0,
    0x082e_fa98_ec4e_6c89,
];

/// Fingerprint the indexed file set: a hash over every `(path, content-hash)` pair.
///
/// Two stores with the same fingerprint index byte-identical content under byte-identical paths.
/// The blob store is content-addressed, so their L1/L2 blobs — and therefore any `MapCache` built
/// from either — are identical. That is what makes reusing an existing map sound rather than merely
/// cheap.
///
/// Order-independent (an XOR fold over per-entry hashes) because `index.files` is an `AHashMap`
/// whose iteration order is not stable. Keys are unique, so no entry can cancel another; the file
/// count is mixed in so that add/remove pairs cannot coincidentally fold back to the same value.
pub(crate) fn index_fingerprint(store: &Store) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};

    let state = ahash::RandomState::with_seeds(HASHER_SEEDS[0], HASHER_SEEDS[1], HASHER_SEEDS[2], HASHER_SEEDS[3]);
    let folded = store
        .index
        .files
        .iter()
        .map(|(path, entry)| {
            let mut hasher = state.build_hasher();
            path.hash(&mut hasher);
            entry.hash_hex.hash(&mut hasher);
            hasher.finish()
        })
        .fold(FINGERPRINT_SEED, |acc, entry_hash| acc ^ entry_hash);

    let mut hasher = state.build_hasher();
    folded.hash(&mut hasher);
    store.index.files.len().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scan `root` fresh and hand back the store, so each assertion sees a store reopened from disk
    /// exactly the way a refresh path reopens it.
    fn scan(root: &std::path::Path) -> Store {
        let cfg = crate::config::ConfigV1::with_defaults();
        let mut store = Store::open(root, crate::store::VIEW_WORKING).unwrap();
        crate::scanner::scan(
            root,
            &mut store,
            &cfg,
            crate::scanner::ScanSource::WorkingTree,
            crate::scanner::EmbedMode::Inline,
        )
        .unwrap();
        store
    }

    /// The whole point: the daemon rewriting `index.msgpack` without changing a single indexed file
    /// must NOT change the fingerprint, so the refresh paths can reuse the map they already hold.
    #[test]
    fn fingerprint_is_stable_when_no_indexed_file_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
        fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();

        let store = scan(root);
        let first = index_fingerprint(&store);
        drop(store);

        // Re-scan and reopen with zero source changes — exactly what a no-op daemon scan produces.
        let store = scan(root);
        let second = index_fingerprint(&store);

        assert_eq!(first, second, "a no-op rescan must not change the fingerprint");
        assert_ne!(
            first, 0,
            "a populated index must never fingerprint to the empty-cache sentinel"
        );
    }

    #[test]
    fn fingerprint_changes_when_a_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();

        let store = scan(root);
        let before = index_fingerprint(&store);
        drop(store);

        fs::write(root.join("a.rs"), b"pub fn alpha_renamed() {}\n").unwrap();
        let store = scan(root);
        let after = index_fingerprint(&store);

        assert_ne!(before, after, "changed content must change the fingerprint");
    }

    #[test]
    fn fingerprint_changes_when_a_file_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
        fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();

        let store = scan(root);
        let before = index_fingerprint(&store);
        drop(store);

        fs::remove_file(root.join("b.rs")).unwrap();
        let store = scan(root);
        let after = index_fingerprint(&store);

        assert_ne!(before, after, "a removed file must change the fingerprint");
    }

    /// Guards the soundness claim directly: same fingerprint => the map built from either store is
    /// identical. If this ever fails, reusing the map would serve stale results.
    #[test]
    fn equal_fingerprint_implies_equal_map_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.rs"), b"pub fn alpha() {}\n").unwrap();
        fs::write(root.join("b.rs"), b"pub fn beta() {}\n").unwrap();

        let store = scan(root);
        let map_a = super::super::MapCache::build(&store);
        drop(store);

        let store = scan(root);
        let map_b = super::super::MapCache::build(&store);

        assert_eq!(
            map_a.fingerprint, map_b.fingerprint,
            "no source change => same fingerprint"
        );
        assert_eq!(
            map_a.by_path.keys().collect::<Vec<_>>(),
            map_b.by_path.keys().collect::<Vec<_>>(),
            "same fingerprint => same indexed path set"
        );
        for (path, l1) in &map_a.by_path {
            let other = map_b.by_path.get(path).expect("path present in both maps");
            let names = |m: &crate::extract::FileMapL1| m.symbols.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
            assert_eq!(
                names(l1),
                names(other),
                "same fingerprint => identical symbols for {path:?}"
            );
        }
    }
}
