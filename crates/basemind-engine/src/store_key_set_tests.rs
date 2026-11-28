use super::*;

#[test]
fn the_sorted_key_set_follows_every_store_mutation() {
    init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(tmp.path(), VIEW_WORKING).expect("open");
    let file = || FileEntry {
        hash_hex: "aa".to_string(),
        language: "rust".to_string(),
        size_bytes: 0,
        mtime: 0,
        extract_epoch: 0,
        content_key: None,
        lines: 0,
    };
    let doc = || DocEntry {
        hash_hex: "bb".to_string(),
        embedding_preset: String::new(),
        size_bytes: 0,
        mtime: 0,
        embedded: false,
        embed_attempted: false,
        config_digest: String::new(),
    };
    let tombstone = || crate::store_doc_tombstone::DocTombstone {
        hash_hex: "cc".to_string(),
        guard_digest: String::new(),
        timeout_secs: 1,
    };
    let under = |store: &Store, dir: &str| -> Vec<String> {
        store
            .key_order()
            .under(dir)
            .map(|key| key.to_str_lossy().into_owned())
            .collect()
    };

    store.upsert("d/a.rs", file());
    store.upsert("d/a.rs", file());
    store.upsert_doc("d/b.pdf", doc());
    store.upsert_doc_tombstone("d/c.pdf", tombstone());
    store.upsert_doc_tombstone("d/a.rs", tombstone());
    assert!(store.key_order().matches(&store.index));
    assert_eq!(under(&store, "d"), ["d/a.rs", "d/b.pdf", "d/c.pdf"]);

    store.remove("d/a.rs");
    assert_eq!(
        under(&store, "d"),
        ["d/a.rs", "d/b.pdf", "d/c.pdf"],
        "the tombstone still holds it"
    );
    assert!(store.remove_doc_tombstone("d/a.rs"));
    store.remove_doc("d/b.pdf");
    assert!(store.key_order().matches(&store.index));
    assert_eq!(under(&store, "d"), ["d/c.pdf"]);

    store.retain_doc_tombstones(|_| false);
    assert!(store.key_order().matches(&store.index));
    assert!(under(&store, "d").is_empty());
}
