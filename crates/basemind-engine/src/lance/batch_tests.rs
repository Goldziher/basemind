use super::*;

/// `n` code-chunk rows of `content`; the batching and maintenance under test do not depend on the
/// table.
#[cfg(feature = "code-search")]
fn doc_rows(content: &str, n: usize, tag: &str) -> Vec<super::super::CodeRow> {
    (0..n)
        .map(|c| super::super::CodeRow {
            content_hash: content.into(),
            chunk_id: format!("{content}:{c}"),
            symbol: String::new(),
            kind: String::new(),
            lang: "rust".into(),
            line_start: 1,
            line_end: 1,
            byte_start: 0,
            byte_end: 1,
            text: format!("{tag} {content} {c}"),
            embedding: vec![0.0; 4],
        })
        .collect()
}

#[cfg(feature = "code-search")]
const TABLE: &str = super::super::schema::CODE_CHUNKS_TABLE;

#[cfg(feature = "code-search")]
fn count(store: &LanceStore, table: &str, predicate: &str) -> usize {
    store.inner.rt().block_on(async {
        let t = store.inner.connection.open_table(table).execute().await.unwrap();
        t.count_rows(Some(predicate.to_string())).await.unwrap()
    })
}

#[cfg(feature = "code-search")]
fn versions(store: &LanceStore, table: &str) -> usize {
    store
        .version_counts()
        .unwrap()
        .into_iter()
        .find(|(n, _)| n == table)
        .map_or(0, |(_, v)| v)
}

#[cfg(feature = "code-search")]
#[test]
fn content_predicate_lists_every_hash_and_escapes() {
    let hashes = ["x'y".to_string(), "z".to_string()];
    assert_eq!(super::super::content_in(&hashes), "content_hash IN ('x''y', 'z')");
}

#[test]
fn pending_batch_fills_on_doc_count_and_row_count() {
    let mut b: PendingBatch<u8> = PendingBatch::default();
    assert!(b.is_empty() && !b.is_full());
    b.push("big", vec![0; BATCH_MAX_ROWS]);
    assert!(b.is_full());
    assert_eq!(b.take().len(), 1);
    for i in 0..BATCH_MAX_DOCS {
        assert!(!b.is_full());
        b.push(&format!("p{i}"), vec![0]);
    }
    assert!(b.is_full());
}

/// #76: N documents must cost O(1) table versions, not 2N, and a rewrite must replace rows.
#[cfg(feature = "code-search")]
#[test]
fn replace_many_commits_once_per_batch_and_replaces_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let base = versions(&store, TABLE);
    let items = (0..50)
        .map(|i| (format!("c{i}"), doc_rows(&format!("c{i}"), 3, "v1")))
        .collect();
    store.replace_code_chunks_many(items).unwrap();
    // First batch on an empty table: append only (the delete is skipped).
    assert_eq!(versions(&store, TABLE), base + 1);
    assert_eq!(count(&store, TABLE, "content_hash IS NOT NULL"), 150);

    // Rewrite 10 contents with 1 chunk each, and drop one (no rows): one delete + one append.
    let mut items: Vec<_> = (0..10)
        .map(|i| (format!("c{i}"), doc_rows(&format!("c{i}"), 1, "v2")))
        .collect();
    items.push(("c49".to_string(), Vec::new()));
    store.replace_code_chunks_many(items).unwrap();
    assert_eq!(versions(&store, TABLE), base + 3);
    assert_eq!(count(&store, TABLE, "content_hash = 'c3'"), 1);
    assert_eq!(count(&store, TABLE, "content_hash = 'c49'"), 0);
    assert_eq!(count(&store, TABLE, "content_hash = 'c20'"), 3);
    assert_eq!(count(&store, TABLE, "content_hash IS NOT NULL"), 10 + 39 * 3);
}

/// #76: the post-scan pass collapses accumulated versions; the data survives it.
#[cfg(feature = "code-search")]
#[test]
fn maintain_prunes_old_versions_and_keeps_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    for i in 0..12 {
        store
            .replace_code_chunks_many(vec![(format!("c{i}"), doc_rows(&format!("c{i}"), 2, "v"))])
            .unwrap();
    }
    assert!(versions(&store, TABLE) >= 12);
    store.maintain(Duration::ZERO).unwrap();
    assert!(versions(&store, TABLE) <= 2, "{:?}", store.version_counts());
    assert_eq!(count(&store, TABLE, "content_hash IS NOT NULL"), 24);
}

/// The watch path writes one document at a time; auto-maintenance must bound the versions.
#[cfg(feature = "code-search")]
#[test]
fn incremental_writes_keep_versions_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    for i in 0..(AUTO_PRUNE_EVERY as usize * 3) {
        store
            .replace_code_chunks_many(vec![("c".into(), doc_rows("c", 1, &format!("g{i}")))])
            .unwrap();
    }
    assert_eq!(count(&store, TABLE, "content_hash = 'c'"), 1);
    let v = versions(&store, TABLE);
    // 96 writes (one delete + one append each, minus the skipped first delete) would be ~191 versions.
    assert!(v < 2 * AUTO_PRUNE_EVERY as usize, "{v} versions");
}

#[cfg(feature = "code-search")]
fn dir_bytes(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p).map_or(0, |rd| {
        rd.flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => dir_bytes(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    })
}

/// Synthetic #76 benchmark: per-document commits versus batched commits plus maintenance.
/// Run with `cargo test --features documents --release lance::batch::tests::bench -- --ignored --nocapture`.
#[cfg(feature = "code-search")]
#[test]
#[ignore = "benchmark: prints commits, versions and disk size"]
fn bench_per_document_vs_batched() {
    const DOCS: usize = 3_000;
    let key = |d: usize| format!("c{d:05}");
    let all_rows = |d: usize| doc_rows(&key(d), 4, "bench");
    for batched in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = LanceStore::open(dir.path(), 4, "test").unwrap();
        let t = std::time::Instant::now();
        if batched {
            let mut pending: PendingBatch<super::super::CodeRow> = PendingBatch::default();
            for d in 0..DOCS {
                pending.push(&key(d), all_rows(d));
                if pending.is_full() {
                    store.replace_code_chunks_many(pending.take()).unwrap();
                }
            }
            if !pending.is_empty() {
                store.replace_code_chunks_many(pending.take()).unwrap();
            }
            store.maintain(Duration::ZERO).unwrap();
        } else {
            for d in 0..DOCS {
                store.replace_code_chunks_many(vec![(key(d), all_rows(d))]).unwrap();
            }
        }
        println!(
            "batched={batched} docs={DOCS} secs={:.1} versions={:?} bytes={}",
            t.elapsed().as_secs_f64(),
            store.version_counts().unwrap(),
            dir_bytes(dir.path())
        );
        assert_eq!(count(&store, TABLE, "content_hash IS NOT NULL"), DOCS * 4);
    }
}
