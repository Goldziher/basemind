use super::*;

fn doc_rows(scope: &str, path: &str, n: usize, tag: &str) -> Vec<DocumentRow> {
    (0..n)
        .map(|c| DocumentRow {
            scope: scope.into(),
            path: path.into(),
            chunk_idx: c as u32,
            mime_type: "text/plain".into(),
            text: format!("{tag} {path} {c}"),
            byte_start: 0,
            byte_end: 1,
            embedding: vec![0.0; 4],
        })
        .collect()
}

fn count(store: &LanceStore, table: &str, predicate: &str) -> usize {
    store.inner.rt().block_on(async {
        let t = store.inner.connection.open_table(table).execute().await.unwrap();
        t.count_rows(Some(predicate.to_string())).await.unwrap()
    })
}

fn versions(store: &LanceStore, table: &str) -> usize {
    store
        .version_counts()
        .unwrap()
        .into_iter()
        .find(|(n, _)| n == table)
        .map_or(0, |(_, v)| v)
}

#[test]
fn replace_predicate_groups_by_scope_and_escapes() {
    let items = vec![
        ("a".to_string(), "x'y".to_string(), Vec::<u8>::new()),
        ("b".to_string(), "q".to_string(), Vec::new()),
        ("a".to_string(), "z".to_string(), Vec::new()),
    ];
    assert_eq!(
        replace_predicate("path", &items),
        "(scope = 'a' AND path IN ('x''y', 'z')) OR (scope = 'b' AND path IN ('q'))"
    );
}

#[test]
fn pending_batch_fills_on_doc_count_and_row_count() {
    let mut b: PendingBatch<u8> = PendingBatch::default();
    assert!(b.is_empty() && !b.is_full());
    b.push("s", "big", vec![0; BATCH_MAX_ROWS]);
    assert!(b.is_full());
    assert_eq!(b.take().len(), 1);
    for i in 0..BATCH_MAX_DOCS {
        assert!(!b.is_full());
        b.push("s", &format!("p{i}"), vec![0]);
    }
    assert!(b.is_full());
}

/// #76: N documents must cost O(1) table versions, not 2N, and a rewrite must replace rows.
#[test]
fn replace_documents_many_commits_once_per_batch_and_replaces_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    let base = versions(&store, DOCUMENTS_TABLE);
    let items = (0..50)
        .map(|i| {
            (
                "s".to_string(),
                format!("d{i}.md"),
                doc_rows("s", &format!("d{i}.md"), 3, "v1"),
            )
        })
        .collect();
    store.replace_documents_many(items).unwrap();
    // First batch on an empty table: append only (the delete is skipped).
    assert_eq!(versions(&store, DOCUMENTS_TABLE), base + 1);
    assert_eq!(count(&store, DOCUMENTS_TABLE, "scope = 's'"), 150);

    // Rewrite 10 docs with 1 chunk each, and drop one doc (no rows): one delete + one append.
    let mut items: Vec<_> = (0..10)
        .map(|i| {
            (
                "s".to_string(),
                format!("d{i}.md"),
                doc_rows("s", &format!("d{i}.md"), 1, "v2"),
            )
        })
        .collect();
    items.push(("s".to_string(), "d49.md".to_string(), Vec::new()));
    store.replace_documents_many(items).unwrap();
    assert_eq!(versions(&store, DOCUMENTS_TABLE), base + 3);
    assert_eq!(count(&store, DOCUMENTS_TABLE, "path = 'd3.md'"), 1);
    assert_eq!(count(&store, DOCUMENTS_TABLE, "path = 'd49.md'"), 0);
    assert_eq!(count(&store, DOCUMENTS_TABLE, "path = 'd20.md'"), 3);
    assert_eq!(count(&store, DOCUMENTS_TABLE, "scope = 's'"), 10 + 39 * 3);
}

/// #76: the post-scan pass collapses accumulated versions; the data survives it.
#[test]
fn maintain_prunes_old_versions_and_keeps_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    for i in 0..12 {
        store
            .replace_documents_many(vec![(
                "s".into(),
                format!("d{i}.md"),
                doc_rows("s", &format!("d{i}.md"), 2, "v"),
            )])
            .unwrap();
    }
    assert!(versions(&store, DOCUMENTS_TABLE) >= 12);
    store.maintain(Duration::ZERO).unwrap();
    assert!(versions(&store, DOCUMENTS_TABLE) <= 2, "{:?}", store.version_counts());
    assert_eq!(count(&store, DOCUMENTS_TABLE, "scope = 's'"), 24);
}

/// The watch path writes one document at a time; auto-maintenance must bound the versions.
#[test]
fn incremental_writes_keep_versions_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = LanceStore::open(dir.path(), 4, "test").unwrap();
    for i in 0..(AUTO_PRUNE_EVERY as usize * 3) {
        store
            .replace_documents_many(vec![(
                "s".into(),
                "d.md".into(),
                doc_rows("s", "d.md", 1, &format!("g{i}")),
            )])
            .unwrap();
    }
    assert_eq!(count(&store, DOCUMENTS_TABLE, "path = 'd.md'"), 1);
    let v = versions(&store, DOCUMENTS_TABLE);
    // 96 writes (one delete + one append each, minus the skipped first delete) would be ~191 versions.
    assert!(v < 2 * AUTO_PRUNE_EVERY as usize, "{v} versions");
}

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
#[test]
#[ignore = "benchmark: prints commits, versions and disk size"]
fn bench_per_document_vs_batched() {
    const DOCS: usize = 3_000;
    let all_rows = |d: usize| doc_rows("s", &format!("d{d:05}.md"), 4, "bench");
    for batched in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = LanceStore::open(dir.path(), 4, "test").unwrap();
        let t = std::time::Instant::now();
        if batched {
            let mut pending: PendingBatch<DocumentRow> = PendingBatch::default();
            for d in 0..DOCS {
                pending.push("s", &format!("d{d:05}.md"), all_rows(d));
                if pending.is_full() {
                    store.replace_documents_many(pending.take()).unwrap();
                }
            }
            if !pending.is_empty() {
                store.replace_documents_many(pending.take()).unwrap();
            }
            store.maintain(Duration::ZERO).unwrap();
        } else {
            for d in 0..DOCS {
                store
                    .replace_document("s", &format!("d{d:05}.md"), all_rows(d))
                    .unwrap();
            }
        }
        println!(
            "batched={batched} docs={DOCS} secs={:.1} versions={:?} bytes={}",
            t.elapsed().as_secs_f64(),
            store.version_counts().unwrap(),
            dir_bytes(dir.path())
        );
        assert_eq!(count(&store, DOCUMENTS_TABLE, "scope = 's'"), DOCS * 4);
    }
}
