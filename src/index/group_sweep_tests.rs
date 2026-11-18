use std::path::Path;
use std::sync::Arc;

use super::group::{GroupDb, GroupLock};
use super::keys::{self, ContentKey};
use super::live_keys::LiveKeys;
use super::view::Part;
use super::{IndexDb, IndexError};
use crate::extract::{Call, FileMapL1, FileMapL2, Symbol, SymbolKind};
use crate::path::RelPath;

fn no_lock() -> Result<GroupLock, IndexError> {
    Ok(Box::new(()))
}

fn open_view(group: &Arc<GroupDb>, workspace: &str, memory: &Path) -> IndexDb {
    let id = group.allocate_view_id(workspace, "working").expect("allocate");
    group.open_view(id, memory, true).expect("open view")
}

fn key(hash: &str) -> ContentKey {
    keys::content_key(hash, "rust", crate::extract::EXTRACT_EPOCH)
}

fn l1(name: &str) -> FileMapL1 {
    FileMapL1 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 0,
        had_errors: false,
        error_count: 0,
        symbols: vec![Symbol {
            name: name.to_string(),
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
    }
}

fn l2(callee: &str) -> FileMapL2 {
    FileMapL2 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        calls: vec![Call {
            callee: callee.to_string(),
            start_byte: 4,
            end_byte: 5,
            start_row: 0,
            start_col: 0,
        }],
        docs: Vec::new(),
    }
}

/// Write `path` as the content `hash`, defining `hash`-named symbol and calling it.
fn write(db: &IndexDb, path: &str, hash: &str) {
    let mut writer = db.writer();
    let name = format!("fn_{hash}");
    writer
        .upsert_file(&RelPath::from(path), &key(hash), &l1(&name), Some(&l2(&name)))
        .expect("upsert");
    writer.commit().expect("commit");
}

/// Publish `entries` as the view's file map, as a flush does once the map is written.
fn publish(db: &IndexDb, entries: &[(&str, &str)]) {
    db.publish_live_keys(LiveKeys::from_entries(
        entries.iter().map(|(path, hash)| (RelPath::from(*path), key(hash))),
    ));
}

/// The distinct content keys of the shared symbol and call rows, and the names of their name rows.
fn stored(db: &IndexDb) -> (Vec<ContentKey>, Vec<String>) {
    let mut contents = Vec::new();
    let mut names = Vec::new();
    for rows in [&db.symbols_by_path, &db.calls_by_path] {
        for row in rows.iter() {
            let (row, _) = row.expect("row");
            contents.push(row[..keys::CONTENT_KEY_LEN].try_into().expect("content key"));
        }
    }
    for row in db.symbols_by_name.iter().chain(db.calls_by_callee.iter()) {
        let (row, _) = row.expect("row");
        let mut at = 0;
        let name = keys::read_len_prefixed_ref(&row, &mut at).expect("name");
        names.push(String::from_utf8(name.to_vec()).expect("utf-8"));
    }
    contents.sort_unstable();
    contents.dedup();
    names.sort_unstable();
    names.dedup();
    (contents, names)
}

fn sweep(db: &IndexDb, live: &[&str]) -> super::group_sweep::GroupSweepReport {
    let live: Vec<ContentKey> = live.iter().map(|hash| key(hash)).collect();
    let mut sweep = db.begin_group_sweep().expect("begin").expect("no other sweep");
    sweep.sweep_contents(|content| live.contains(content)).expect("sweep");
    sweep.finish(true).expect("finish")
}

#[test]
fn a_sweep_deletes_every_content_no_view_names_with_its_name_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    write(&a, "x.rs", "shared");
    write(&a, "y.rs", "gone");
    write(&b, "x.rs", "shared");
    publish(&a, &[("x.rs", "shared")]);
    publish(&b, &[("x.rs", "shared")]);
    assert!(a.content_sweep_owed_for_test(), "a path write owes the group a sweep");
    assert_eq!(stored(&a).0.len(), 2);

    let report = sweep(&a, &["shared"]);

    assert_eq!(report.contents_removed, 1);
    assert_eq!(stored(&a), (vec![key("shared")], vec!["fn_shared".to_string()]));
    assert!(!a.content_sweep_owed_for_test(), "the paid debt is cleared");
}

#[test]
fn a_content_a_writer_staged_is_kept_until_its_file_map_is_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    publish(&a, &[]);
    write(&b, "new.rs", "in-flight");

    let report = sweep(&a, &[]);

    assert_eq!(report.contents_removed, 0, "b's pass has not written its file map yet");
    assert_eq!(stored(&a).0, vec![key("in-flight")]);
    assert!(
        a.content_sweep_owed_for_test(),
        "the pass may still drop a content; the debt stays"
    );

    publish(&b, &[]);
    assert_eq!(sweep(&a, &[]).contents_removed, 1, "b's map dropped it");
    assert!(stored(&a).0.is_empty());
}

#[test]
fn a_file_map_written_during_a_sweep_keeps_its_contents_for_that_sweep() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    publish(&a, &[]);

    let mut open = a.begin_group_sweep().expect("begin").expect("no other sweep");
    assert!(a.begin_group_sweep().expect("begin").is_none(), "one sweep at a time");
    // The sweep read b's map before b wrote this content and its new map. ~keep
    write(&b, "new.rs", "late");
    publish(&b, &[("new.rs", "late")]);
    open.sweep_contents(|_| false).expect("sweep");
    open.finish(true).expect("finish");

    assert_eq!(stored(&a).0, vec![key("late")], "pending until the sweep ended");
    assert_eq!(sweep(&a, &["late"]).contents_removed, 0, "the next sweep reads b's map");
    assert_eq!(sweep(&a, &[]).contents_removed, 1, "and no longer holds it pending");
}

#[test]
fn a_view_whose_handles_are_gone_is_retired_with_its_rows_and_its_id_is_never_reused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let a = open_view(&group, "ws-a", &memory);
    let gone = open_view(&group, "ws-gone", &memory);
    let gone_id = group.allocate_view_id("ws-gone", "working").expect("id");
    gone.keyspace(Part::RefsByPath).insert(b"row", "v").expect("insert");
    gone.meta.insert(b"own-meta", "v").expect("insert");

    let mut open = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(
        !open.retire_view("ws-gone", "working", gone_id).expect("retire"),
        "an open view is not retired"
    );
    drop(gone);
    assert!(open.retire_view("ws-gone", "working", gone_id).expect("retire"));
    assert!(open.owes_contents(), "its contents may be dead now");
    drop(open);

    let scoped = |part: Part| super::scoped::Scoped::view(group.keyspace(part).clone(), gone_id);
    assert!(scoped(Part::RefsByPath).is_empty().expect("read"));
    assert!(scoped(Part::Meta).is_empty().expect("read"));
    assert_eq!(group.view_id("ws-gone", "working").expect("read"), None);
    assert!(
        group.allocate_view_id("ws-gone", "working").expect("again") > gone_id,
        "a retired id is never handed out again"
    );
}

#[test]
fn a_retirement_a_crash_cut_short_is_finished_by_the_next_sweep() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let a = open_view(&group, "ws-a", &memory);
    let half = group.allocate_view_id("ws-half", "working").expect("id");
    super::scoped::Scoped::view(group.keyspace(Part::RefsByPath).clone(), half)
        .insert(b"row", "v")
        .expect("insert");
    let mut tombstone = b"\xffgroup/retired_view/".to_vec();
    tombstone.extend_from_slice(&half.to_be_bytes());
    group.meta().insert(&tombstone, []).expect("tombstone");

    drop(a.begin_group_sweep().expect("begin").expect("sweep"));

    let rows = super::scoped::Scoped::view(group.keyspace(Part::RefsByPath).clone(), half);
    assert!(rows.is_empty().expect("read"));
    assert!(group.meta().get(&tombstone).expect("read").is_none());
}

#[test]
fn a_view_claimed_for_an_open_is_not_retired_before_the_open_takes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let a = open_view(&group, "ws-a", &memory);
    // The worktree was recreated at a reaped path: its view directory is back, its record never left. ~keep
    let claim = group.claim_view("ws-back", "working").expect("claim");
    let id = claim.id();

    let mut open = a.begin_group_sweep().expect("begin").expect("sweep");
    let retired = open.retire_view("ws-back", "working", id).expect("retire");
    drop(open);
    let back = group.open_claimed(claim, &memory, true).expect("open");
    back.keyspace(Part::RefsByPath).insert(b"row", "v").expect("insert");

    assert!(!retired, "an id claimed by an open is in use");
    assert_eq!(group.view_id("ws-back", "working").expect("read"), Some(id));
    drop(back);
    let mut open = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(
        open.retire_view("ws-back", "working", id).expect("retire"),
        "once the open's handle is gone, the view can be retired"
    );
}
