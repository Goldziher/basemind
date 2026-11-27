use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Barrier, mpsc};
use std::time::Duration;

use super::*;
use crate::index::view::IndexView;

/// How long an open that must not wait for another is given before the test calls it blocked.
const UNBLOCKED_OPEN_WAIT: Duration = Duration::from_secs(10);
/// How long the open of a group is held in flight while other opens of the group arrive.
const IN_FLIGHT_OPEN: Duration = Duration::from_millis(50);

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
fn a_slow_open_of_one_group_does_not_block_an_open_of_another() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (slow_dir, fast_dir) = (dir.path().join("slow"), dir.path().join("fast"));
    let (slow_dir, fast_dir) = (slow_dir.as_path(), fast_dir.as_path());
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let slow = scope.spawn(move || {
            GroupDb::open_shared(slow_dir, move || {
                entered_tx.send(()).expect("signal the slow open");
                release_rx.recv().expect("release the slow open");
                no_lock()
            })
            .expect("slow open")
        });
        entered.recv().expect("the slow open is in flight");
        let (opened_tx, opened) = mpsc::channel();
        let fast = scope.spawn(move || {
            opened_tx
                .send(GroupDb::open_shared(fast_dir, no_lock).expect("fast open"))
                .expect("report the fast open");
        });
        let fast_open = opened.recv_timeout(UNBLOCKED_OPEN_WAIT);
        release.send(()).expect("release the slow open");
        let slow = slow.join().expect("slow opener");
        fast.join().expect("fast opener");
        let fast_open = fast_open.expect("the open of another group waited for the slow open");
        assert!(!Arc::ptr_eq(&slow, &fast_open), "each group has a handle of its own");
        assert!(GroupDb::shared(slow_dir).is_some_and(|held| Arc::ptr_eq(&held, &slow)));
        assert!(GroupDb::shared(fast_dir).is_some_and(|held| Arc::ptr_eq(&held, &fast_open)));
    });
}

#[test]
fn concurrent_opens_of_one_group_open_it_once_and_share_the_handle() {
    const OPENERS: usize = 8;
    let dir = tempfile::tempdir().expect("tempdir");
    let (dir, opens, start) = (dir.path(), &AtomicUsize::new(0), &Barrier::new(OPENERS));
    let handles: Vec<Arc<GroupDb>> = std::thread::scope(|scope| {
        let openers: Vec<_> = (0..OPENERS)
            .map(|_| {
                scope.spawn(move || {
                    start.wait();
                    GroupDb::open_shared(dir, move || {
                        opens.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(IN_FLIGHT_OPEN);
                        no_lock()
                    })
                    .expect("open group")
                })
            })
            .collect();
        openers
            .into_iter()
            .map(|opener| opener.join().expect("opener"))
            .collect()
    });
    assert_eq!(opens.load(Ordering::SeqCst), 1, "the group is opened once");
    assert_eq!(handles.len(), OPENERS);
    assert!(handles.iter().all(|handle| Arc::ptr_eq(handle, &handles[0])));
}

#[test]
fn a_panic_during_an_open_leaves_the_group_openable() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Held so no other test's registry prune drops the slot before it is inspected. ~keep
    let slot = slot(dir.path());
    let panicked = std::panic::catch_unwind(|| GroupDb::open_shared(dir.path(), || panic!("the group lock panics")));
    assert!(panicked.is_err());
    assert!(slot.is_poisoned(), "the panic poisoned the group's slot");

    let group = open(dir.path());
    assert!(GroupDb::shared(dir.path()).is_some_and(|held| Arc::ptr_eq(&held, &group)));
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
                meta.remove(view_record("ws-a", "working"))
                    .expect("lose the view record");
            } else {
                meta.insert(META_FORMAT, (GROUP_FORMAT - 1).to_be_bytes())
                    .expect("older format");
            }
            group.incarnation()
        };

        let group = open(&group_dir);
        assert_ne!(group.incarnation(), first, "crashed mid-clear: {crashed_mid_clear}");
        let a = view(&group, "ws-a", "working", &memory);
        assert!(
            a.keyspace(Part::RefsByPath).is_empty().expect("read"),
            "{crashed_mid_clear}"
        );
        assert!(
            a.keyspace(Part::GrepBloom).is_empty().expect("read"),
            "{crashed_mid_clear}"
        );
    }
}

/// A bloom row a sibling's writer staged, which no file map names yet, survives a sweep that judged
/// liveness by the maps; once the sibling writes its map, liveness alone decides.
#[test]
fn a_bloom_sweep_keeps_the_rows_a_sibling_staged_until_its_map_names_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = open(&dir.path().join("group"));
    let a = view(&group, "ws-a", "working", &memory);
    let b = view(&group, "ws-b", "working", &memory);
    let mut writer = b.writer();
    writer.upsert_grep_bloom("fresh", crate::index::grep_bloom::build_row(b"fn fresh() {}"));
    writer.commit().expect("commit");

    {
        let _window = a.grep_bloom_sweep_window().expect("no other sweep runs");
        assert_eq!(a.sweep_grep_bloom(|_| false).expect("sweep"), 0);
    }
    assert!(a.grep_bloom_current("fresh"), "the sibling's staged row stays");
    a.remove_grep_bloom_rows(["fresh"]).expect("remove");
    assert!(
        a.grep_bloom_current("fresh"),
        "and stays for the incremental removal too"
    );

    b.publish_live_keys(crate::index::live_keys::LiveKeys::default());
    assert_eq!(
        a.sweep_grep_bloom(|_| false).expect("sweep"),
        1,
        "settled: the maps decide"
    );
    assert!(!a.grep_bloom_current("fresh"));
}

/// Two release minors share one group directory: each keeps its own database in it, so neither
/// opens, wipes nor refuses the other's. The older release's database keeps the stamp it was left with.
#[test]
fn two_release_minors_keep_their_own_group_database_in_one_group_directory() {
    use crate::index::{INDEX_SCHEMA_VER, peek_index_schema_ver};
    use crate::store_layout::{LAST_UNNAMESPACED_GROUP_MINOR, group_index_namespace};
    const OLDER_RELEASES_SCHEMA: u32 = INDEX_SCHEMA_VER - 1;
    let dir = tempfile::tempdir().expect("tempdir");
    let older = group_index_namespace(dir.path(), LAST_UNNAMESPACED_GROUP_MINOR);
    let newer = group_index_namespace(dir.path(), LAST_UNNAMESPACED_GROUP_MINOR + 1);
    assert_eq!(older, dir.path());
    assert_eq!(newer, dir.path().join("v35"));

    // The older release's database, stamped with a schema this build would wipe on open.
    let older_incarnation = {
        let group = open(&older);
        group
            .meta()
            .insert(crate::index::META_SCHEMA_VER, OLDER_RELEASES_SCHEMA.to_be_bytes())
            .expect("stamp the older release's schema");
        group.allocate_view_id("ws", "working").expect("allocate");
        group.incarnation()
    };
    assert_eq!(
        peek_index_schema_ver(&older).expect("peek"),
        Some(OLDER_RELEASES_SCHEMA)
    );

    let newer_group = open(&newer);
    assert_ne!(
        newer_group.incarnation(),
        older_incarnation,
        "the newer minor starts a database of its own"
    );
    assert_eq!(
        newer_group.views().expect("views"),
        Vec::new(),
        "it holds none of the older rows"
    );
    drop(newer_group);

    assert_eq!(
        peek_index_schema_ver(&older).expect("peek"),
        Some(OLDER_RELEASES_SCHEMA),
        "the newer minor's open left the older database as it was"
    );
    assert_eq!(peek_index_schema_ver(&newer).expect("peek"), Some(INDEX_SCHEMA_VER));
}
