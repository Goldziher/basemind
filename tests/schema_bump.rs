//! Verifies that opening a Store against a stale-schema index auto-wipes the cache.
//!
//! We synthesize a stale-schema-shaped index by serializing a struct with
//! `schema_ver = 99` (a value distinct from any current or near-future
//! `RELEASE_MINOR`) and the legacy `FileEntry` layout. `Store::open` should detect the
//! mismatch, remove `index.msgpack` and `blobs/`, and return an empty in-memory index.

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

/// Iter-6 additive `FileMapDoc.keywords` + `FileMapDoc.entities` must not break
/// pre-iter-6 blobs. We construct an "old-shape" blob — every iter-2..iter-5
/// field present, no tail keyword/entity fields — and assert it round-trips
/// through msgpack into the current `FileMapDoc` with empty vecs courtesy of
/// `#[serde(default)]`. This is the explicit contract behind the no-bump
/// decision in the iter-6 plan.
#[cfg(feature = "documents")]
#[test]
fn pre_iter6_doc_blob_deserialises_into_new_filemap_doc() {
    use basemind::extract::doc::FileMapDoc;
    use serde::Serialize;

    // SCHEMA_VER is a `pub(crate)` constant inside `extract/mod.rs`; we
    // construct the blob with the current ver explicitly so `read_doc_by_hex`'s
    // schema check would pass — but here we deserialise straight to the struct,
    // so any version is fine. Use 0 to make the "old" intent obvious.
    #[derive(Serialize)]
    struct OldShape {
        schema_ver: u16,
        mime_type: String,
        content: String,
        metadata: Vec<(String, String)>,
        detected_languages: Vec<String>,
        chunks: Vec<OldChunk>,
        embedding_model: String,
        embedding_dim: u16,
        // NOTE: no `keywords`, no `entities` — pre-iter-6 layout.
    }
    #[derive(Serialize)]
    struct OldChunk {
        byte_start: u32,
        byte_end: u32,
        text: String,
        embedding: Vec<f32>,
    }

    let old = OldShape {
        schema_ver: 0,
        mime_type: "text/plain".to_string(),
        content: "hello world".to_string(),
        metadata: vec![("title".to_string(), "Test".to_string())],
        detected_languages: vec!["eng".to_string()],
        chunks: vec![OldChunk {
            byte_start: 0,
            byte_end: 11,
            text: "hello world".to_string(),
            embedding: vec![],
        }],
        embedding_model: String::new(),
        embedding_dim: 0,
    };
    let bytes = rmp_serde::to_vec_named(&old).expect("serialize old shape");
    let new_doc: FileMapDoc =
        rmp_serde::from_slice(&bytes).expect("old shape must deserialise via serde(default)");

    assert_eq!(new_doc.mime_type, "text/plain");
    assert_eq!(new_doc.chunks.len(), 1);
    assert!(
        new_doc.keywords.is_empty(),
        "iter-6 `keywords` must default to empty on pre-iter-6 blobs"
    );
    assert!(
        new_doc.entities.is_empty(),
        "iter-6 `entities` must default to empty on pre-iter-6 blobs"
    );
}

#[test]
fn opening_against_stale_schema_index_wipes_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let basemind_dir = root.join(".basemind");
    let blobs_dir = basemind_dir.join("blobs");
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
        schema_ver: 99,
        files,
    };
    let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
    fs::write(basemind_dir.join("index.msgpack"), bytes).unwrap();

    // Opening the store must detect the mismatch and wipe.
    let store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING)
        .expect("open should succeed via auto-wipe");
    assert!(
        store.index.files.is_empty(),
        "in-memory index should be empty after wipe"
    );
    assert!(!blob_path.exists(), "stale blob should have been removed");
    // The blobs directory still exists (re-created after wipe), ready for new writes.
    assert!(blobs_dir.exists());
}
