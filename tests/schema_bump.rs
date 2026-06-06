//! Verifies that opening a Store against a stale-schema index auto-wipes the cache.
//!
//! We synthesize a v1-shaped index by serializing a struct with `schema_ver = 1` and
//! the legacy `FileEntry` layout. `Store::open` should detect the mismatch, remove
//! `index.msgpack` and `blobs/`, and return an empty in-memory index.

use std::fs;

use serde::Serialize;

#[derive(Serialize)]
struct LegacyIndex {
    schema_ver: u16,
    files: std::collections::BTreeMap<String, LegacyEntry>,
}

#[derive(Serialize)]
struct LegacyEntry {
    hash_hex: String,
    language: String,
    size_bytes: u64,
    mtime: i64,
}

#[test]
fn opening_against_v1_index_wipes_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let gitmind_dir = root.join(".gitmind");
    let blobs_dir = gitmind_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).unwrap();

    // Drop a fake blob and a v1-shaped index.
    let blob_path = blobs_dir.join("deadbeef.l1.msgpack");
    fs::write(&blob_path, b"not really a blob").unwrap();

    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "a.rs".to_string(),
        LegacyEntry {
            hash_hex: "deadbeef".repeat(8),
            language: "rust".to_string(),
            size_bytes: 42,
            mtime: 0,
        },
    );
    let legacy = LegacyIndex {
        schema_ver: 1,
        files,
    };
    let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
    fs::write(gitmind_dir.join("index.msgpack"), bytes).unwrap();

    // Opening the store must detect the mismatch and wipe.
    let store = gitmind::store::Store::open(root, gitmind::store::VIEW_WORKING)
        .expect("open should succeed via auto-wipe");
    assert!(
        store.index.files.is_empty(),
        "in-memory index should be empty after wipe"
    );
    assert!(!blob_path.exists(), "stale blob should have been removed");
    // The blobs directory still exists (re-created after wipe), ready for new writes.
    assert!(blobs_dir.exists());
}
