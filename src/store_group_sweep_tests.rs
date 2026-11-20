use std::path::{Path, PathBuf};

use crate::extract::{FileMapL1, Symbol, SymbolKind};
use crate::index::keys::{self, ContentKey};
use crate::path::RelPath;
use crate::store::{INDEX_FILE, Store, VIEW_STAGED, VIEW_WORKING};

fn git(args: &[&str], cwd: &Path) {
    let out = crate::testing::git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository whose group holds two views of one workspace: `working`, opened by the returned
/// store, and `staged`, opened once and closed. Returns the store and the staged view's directory.
fn group_of_two(tmp: &Path) -> (Store, PathBuf) {
    crate::store::init_isolated_cache();
    let root = tmp.join("repo");
    std::fs::create_dir_all(&root).expect("repo");
    git(&["init", "-q", "-b", "main"], &root);
    let staged_dir = Store::open(&root, VIEW_STAGED).expect("open staged").view_dir.clone();
    (Store::open(&root, VIEW_WORKING).expect("open working"), staged_dir)
}

fn key(hash: &str) -> ContentKey {
    keys::content_key(hash, "rust", crate::extract::EXTRACT_EPOCH)
}

/// Commit a content no file map names, then publish the store's file map (which does not name it).
fn write_dead_content(store: &Store, hash: &str) {
    let db = store.index_db.as_ref().expect("a writer holds its index");
    let l1 = FileMapL1 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 0,
        had_errors: false,
        error_count: 0,
        symbols: vec![Symbol {
            name: format!("fn_{hash}"),
            kind: SymbolKind::Function,
            start_byte: 0,
            end_byte: 1,
            start_row: 0,
            start_col: 0,
            signature: None,
            decorators: Vec::new(),
        }],
        imports: Vec::new(),
        implementations: Vec::new(),
        rationale: Vec::new(),
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    };
    let mut writer = db.writer();
    writer
        .upsert_file(&RelPath::from("dropped.rs"), &key(hash), &l1, None)
        .expect("upsert");
    writer.commit().expect("commit");
    store.publish_live_keys();
}

fn holds_content(store: &Store, hash: &str) -> bool {
    let db = store.index_db.as_ref().expect("index");
    db.holds_content_rows_for_test(&key(hash))
}

fn view_id(store: &Store, view: &str) -> Option<u32> {
    let db = store.index_db.as_ref().expect("index");
    let own = db.group.as_ref().expect("a group index");
    let workspace = crate::store_layout::workspace_key(&store.root);
    own.group.view_id(&workspace, view).expect("read the record")
}

#[test]
fn an_unreadable_sibling_file_map_deletes_no_content_and_keeps_the_debt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, staged_dir) = group_of_two(tmp.path());
    std::fs::write(staged_dir.join(INDEX_FILE), b"\xc1 not msgpack").expect("damage the map");
    write_dead_content(&store, "dead");

    let report = store.sweep_group().expect("sweep").expect("a group sweep");

    assert_eq!(report.contents_removed, 0, "liveness is unknown");
    assert!(holds_content(&store, "dead"));
    let db = store.index_db.as_ref().expect("index");
    assert!(db.content_sweep_owed_for_test(), "the debt waits for a readable map");

    std::fs::remove_file(staged_dir.join(INDEX_FILE)).expect("drop the damaged map");
    let report = store.sweep_group().expect("sweep").expect("a group sweep");
    assert_eq!(report.contents_removed, 1, "with every map read, the dead content goes");
    assert!(!holds_content(&store, "dead"));
}

#[test]
fn a_view_is_retired_only_once_its_directory_is_known_to_be_gone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (store, staged_dir) = group_of_two(tmp.path());
    let staged = view_id(&store, VIEW_STAGED).expect("the staged view has a record");

    let report = store.sweep_group().expect("sweep").expect("a group sweep");
    assert_eq!(report.views_retired, 0, "its directory is there");
    assert_eq!(view_id(&store, VIEW_STAGED), Some(staged));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let views = staged_dir.parent().expect("views dir").to_path_buf();
        std::fs::set_permissions(&views, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let unknown = store.sweep_group();
        std::fs::set_permissions(&views, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
        assert_eq!(
            unknown.expect("sweep").expect("a group sweep").views_retired,
            0,
            "existence unknown"
        );
        assert_eq!(view_id(&store, VIEW_STAGED), Some(staged));
    }

    std::fs::remove_dir_all(&staged_dir).expect("reap the view");
    let report = store.sweep_group().expect("sweep").expect("a group sweep");
    assert_eq!(report.views_retired, 1);
    assert_eq!(view_id(&store, VIEW_STAGED), None);
    assert!(view_id(&store, VIEW_WORKING).is_some(), "the sweeping view stays");
}
