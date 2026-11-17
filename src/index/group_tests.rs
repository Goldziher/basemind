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

    a.keyspace(Part::SymbolsByPath)
        .insert(b"same-key", "a")
        .expect("insert");
    b.keyspace(Part::SymbolsByPath)
        .insert(b"same-key", "b")
        .expect("insert");
    a.keyspace(Part::GrepBloom).insert(b"hash", "bloom").expect("insert");
    assert_eq!(a.seal_scan(0).expect("seal"), 1);
    assert_eq!(a.seal_scan(1).expect("seal"), 2);
    assert_eq!(b.scan_generation().expect("read"), 0, "a seal is the view's own");

    assert_eq!(
        &*b.view()
            .get(Part::SymbolsByPath, b"same-key")
            .expect("get")
            .expect("row"),
        b"b"
    );
    assert!(
        b.view().get(Part::GrepBloom, b"hash").expect("get").is_some(),
        "content rows are shared by the group"
    );

    a.clear_for_rebuild().expect("clear");
    assert!(a.keyspace(Part::SymbolsByPath).is_empty().expect("empty"));
    assert_eq!(a.scan_generation().expect("read"), 0);
    assert_eq!(
        &*b.view()
            .get(Part::SymbolsByPath, b"same-key")
            .expect("get")
            .expect("row"),
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
