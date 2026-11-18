use super::*;
use crate::index::view::IndexView;

fn no_lock() -> Result<GroupLock, IndexError> {
    Ok(Box::new(()))
}

fn open(dir: &Path) -> Arc<GroupDb> {
    GroupDb::open_shared(dir, no_lock).expect("open group")
}

fn view(group: &Arc<GroupDb>, workspace: &str, view: &str, memory: &Path) -> IndexDb {
    let id = group.allocate_view_id(workspace, view).expect("allocate");
    group.open_view(id, memory, false).expect("open view")
}

#[test]
fn view_ids_are_stable_distinct_and_listed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let group = open(dir.path());
    let a = group.allocate_view_id("ws-a", "working").expect("allocate");
    let b = group.allocate_view_id("ws-b", "working").expect("allocate");
    let a_staged = group.allocate_view_id("ws-a", "staged").expect("allocate");

    assert_eq!(a, FIRST_VIEW_ID);
    assert_eq!(
        group.allocate_view_id("ws-a", "working").expect("again"),
        a,
        "an id is stable"
    );
    assert_eq!(
        [a, b, a_staged].iter().collect::<std::collections::BTreeSet<_>>().len(),
        3
    );
    let mut views = group.views().expect("views");
    views.sort();
    assert_eq!(
        views,
        vec![
            ("ws-a".into(), "staged".into(), a_staged),
            ("ws-a".into(), "working".into(), a),
            ("ws-b".into(), "working".into(), b),
        ]
    );
}

#[test]
fn one_process_holds_one_handle_per_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = open(dir.path());
    let second = GroupDb::open_shared(dir.path(), || panic!("a live handle needs no new lock")).expect("shared");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(GroupDb::shared(dir.path()).is_some());
    drop((first, second));
    assert!(
        GroupDb::shared(dir.path()).is_none(),
        "the registry holds no strong handle"
    );
}

#[test]
fn views_keep_their_rows_seal_generations_and_rebuilds_apart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = open(&dir.path().join("group"));
    let (a, b) = (
        view(&group, "ws-a", "working", &memory),
        view(&group, "ws-b", "working", &memory),
    );

    a.keyspace(Part::RefsByPath).insert(b"same-key", "a").expect("insert");
    b.keyspace(Part::RefsByPath).insert(b"same-key", "b").expect("insert");
    a.keyspace(Part::GrepBloom).insert(b"hash", "bloom").expect("insert");
    assert_eq!(a.seal_scan(0).expect("seal"), 1);
    assert_eq!(a.seal_scan(1).expect("seal"), 2);
    assert_eq!(b.scan_generation().expect("read"), 0, "a seal is the view's own");

    assert_eq!(
        &*b.view().get(Part::RefsByPath, b"same-key").expect("get").expect("row"),
        b"b"
    );
    assert!(
        b.view().get(Part::GrepBloom, b"hash").expect("get").is_some(),
        "content rows are shared by the group"
    );

    a.clear_for_rebuild().expect("clear");
    assert!(a.keyspace(Part::RefsByPath).is_empty().expect("empty"));
    assert_eq!(a.scan_generation().expect("read"), 0);
    assert_eq!(
        &*b.view().get(Part::RefsByPath, b"same-key").expect("get").expect("row"),
        b"b"
    );
    assert!(
        b.view().get(Part::GrepBloom, b"hash").expect("get").is_some(),
        "a view's rebuild keeps the rows the group shares"
    );
    assert_eq!(
        b.sibling_views().expect("siblings"),
        vec![("ws-a".to_string(), "working".to_string())]
    );
}

#[test]
fn the_incarnation_survives_a_reopen_and_changes_with_a_new_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let group_dir = dir.path().join("group");
    let first = open(&group_dir).incarnation();
    assert_ne!(first, 0);
    assert_eq!(open(&group_dir).incarnation(), first, "a reopen keeps it");
    std::fs::remove_dir_all(&group_dir).expect("lose the database");
    assert_ne!(open(&group_dir).incarnation(), first, "a new database gets a new one");
}

/// Handles on one view share its live keys; a view with no open handle keeps none, so a long-lived
/// process does not hold the keys of every view it ever opened.
#[test]
fn live_keys_are_shared_by_a_view_s_handles_and_dropped_with_the_last() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = open(&dir.path().join("group"));
    let a = view(&group, "ws-a", "working", &memory);
    let b = view(&group, "ws-b", "working", &memory);
    let a_id = group.allocate_view_id("ws-a", "working").expect("id");
    let b_id = group.allocate_view_id("ws-b", "working").expect("id");
    let again = group.open_view(a_id, &memory, true).expect("a second handle on a");
    let key = crate::index::keys::content_key("h", "rust", 1);
    a.publish_live_keys(crate::index::live_keys::LiveKeys::from_entries([(
        crate::path::RelPath::from("x.rs"),
        key,
    )]));
    assert_eq!(
        again.live_keys().key_of(&crate::path::RelPath::from("x.rs")),
        Some(&key),
        "one view's handles share its keys"
    );
    assert!(b.live_keys().is_empty(), "another view's are its own");

    drop(b);
    let mut held = group.live_key_views();
    held.sort_unstable();
    assert_eq!(held, [a_id], "the closed view's cell is gone");
    assert!(!held.contains(&b_id));
    drop((a, again));
    assert!(group.live_key_views().is_empty());
}

/// A view's BM25 cache lives as long as a handle on the view: the daemon keeps the group open for
/// its hosted view, and must not keep the cache of every other view a query ever opened.
#[test]
fn a_view_s_bm25_cache_is_dropped_with_its_last_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = open(&dir.path().join("group"));
    let a = view(&group, "ws-a", "working", &memory);
    let b = view(&group, "ws-b", "working", &memory);
    let a_id = group.allocate_view_id("ws-a", "working").expect("id");
    let again = group.open_view(a_id, &memory, true).expect("a second handle on a");
    assert!(
        Arc::ptr_eq(&a.bm25_live, &again.bm25_live),
        "one view's handles share its cache"
    );
    assert!(!Arc::ptr_eq(&a.bm25_live, &b.bm25_live), "another view's is its own");

    drop(b);
    assert_eq!(group.bm25_cache_views(), [a_id], "the closed view's cache is gone");
    drop((a, again));
    assert!(group.bm25_cache_views().is_empty());
}

/// A group database of another format is discarded whole, and so is one holding rows without an
/// incarnation (an older build died while emptying it in place): no row of the old layout survives
/// under a view id the new database allocates again.
#[test]
fn rows_of_another_layout_never_survive_the_rebuild() {
    let dir = tempfile::tempdir().expect("tempdir");
    let group_dir = dir.path().join("group");
    let memory = dir.path().join("memory.fjall");
    for crashed_mid_clear in [false, true] {
        let first = {
            let group = open(&group_dir);
            let a = view(&group, "ws-a", "working", &memory);
            a.keyspace(Part::RefsByPath).insert(b"old", "row").expect("insert");
            a.keyspace(Part::GrepBloom).insert(b"hash", "bloom").expect("insert");
            let meta = group.meta();
            if crashed_mid_clear {
                meta.remove(META_INCARNATION).expect("lose the incarnation");
                meta.remove(META_FORMAT).expect("lose the format");
                meta.remove(META_NEXT_VIEW).expect("lose the view counter");
                meta.remove(view_record("ws-a", "working")).expect("lose the view record");
            } else {
                meta.insert(META_FORMAT, (GROUP_FORMAT - 1).to_be_bytes()).expect("older format");
            }
            group.incarnation()
        };

        let group = open(&group_dir);
        assert_ne!(group.incarnation(), first, "crashed mid-clear: {crashed_mid_clear}");
        let a = view(&group, "ws-a", "working", &memory);
        assert!(a.keyspace(Part::RefsByPath).is_empty().expect("read"), "{crashed_mid_clear}");
        assert!(a.keyspace(Part::GrepBloom).is_empty().expect("read"), "{crashed_mid_clear}");
    }
}
