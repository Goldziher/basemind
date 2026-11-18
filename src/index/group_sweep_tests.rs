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

#[test]
fn a_retirement_owes_the_content_sweep_until_one_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let a = open_view(&group, "ws-a", &memory);
    let gone_id = group.allocate_view_id("ws-gone", "working").expect("id");
    assert!(!a.content_sweep_owed_for_test());

    let mut open = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(open.retire_view("ws-gone", "working", gone_id).expect("retire"));
    // The file maps could not be read, so no content sweep ran. ~keep
    drop(open);
    assert!(
        a.content_sweep_owed_for_test(),
        "the retired view's contents are owed a sweep"
    );

    let open = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(open.owes_contents());
    open.finish(true).expect("finish");
    assert!(!a.content_sweep_owed_for_test(), "a content sweep paid it");
}

#[test]
fn a_retirement_a_crash_cut_short_is_reported_when_finished() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let a = open_view(&group, "ws-a", &memory);
    let half = group.allocate_view_id("ws-half", "working").expect("id");
    let mut tombstone = b"\xffgroup/retired_view/".to_vec();
    tombstone.extend_from_slice(&half.to_be_bytes());
    group.meta().insert(&tombstone, []).expect("tombstone");

    let report = a
        .begin_group_sweep()
        .expect("begin")
        .expect("sweep")
        .finish(false)
        .expect("finish");

    assert_eq!(report.views_retired, 1);
}

/// Measures what a view's retirement costs the group's writers: the retirement's wall time and the
/// longest a concurrent writer waited for the group gate meanwhile. Run with
/// `cargo test --features code-search --lib retirement_gate_hold -- --ignored --nocapture`.
#[cfg(feature = "code-search")]
#[test]
#[ignore = "timing harness"]
fn retirement_gate_hold() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::search::bm25::ChunkPosting;

    let files: usize = std::env::var("RETIRE_FILES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4000);
    let terms: usize = 40;
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let keep = open_view(&group, "ws-keep", &memory);
    let gone_id = group.allocate_view_id("ws-gone", "working").expect("id");
    let gone = group.open_view(gone_id, &memory, true).expect("open");
    let write = |db: &IndexDb, prefix: &str| {
        let mut writer = db.writer();
        for i in 0..files {
            let hash = format!("{prefix}{i:06}");
            let posting = ChunkPosting {
                chunk_id: format!("{hash}:0"),
                doclen: u32::try_from(terms).expect("terms"),
                terms: (0..terms).map(|t| (format!("term{}", (i * 7 + t) % 5000), 1)).collect(),
            };
            writer
                .upsert_bm25_file(
                    &RelPath::from(format!("src/f{i}.rs").as_str()),
                    &keys::bm25_content_key(&hash, "rust", "d"),
                    &[posting],
                )
                .expect("upsert");
            if writer.staged_bytes() > 1 << 20 {
                writer.commit().expect("commit");
                writer = db.writer();
            }
        }
        writer.commit().expect("commit");
    };
    write(&keep, "k");
    write(&gone, "g");
    let postings = keep.code_bm25_postings.iter().count();
    drop(gone);

    let done = AtomicBool::new(false);
    let (retire, longest) = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut longest = Duration::ZERO;
            while !done.load(Ordering::Acquire) {
                let asked = Instant::now();
                drop(keep.bm25_gate.read());
                longest = longest.max(asked.elapsed());
                std::thread::yield_now();
            }
            longest
        });
        let started = Instant::now();
        let mut open = keep.begin_group_sweep().expect("begin").expect("sweep");
        assert!(open.retire_view("ws-gone", "working", gone_id).expect("retire"));
        open.finish(false).expect("finish");
        let retire = started.elapsed();
        done.store(true, Ordering::Release);
        (retire, waiter.join().expect("waiter"))
    });
    let left = keep.code_bm25_postings.iter().count();
    println!(
        "retirement of {files} files x {terms} terms ({postings} posting rows, {left} left): \
         {retire:?} wall, longest writer wait on the gate {longest:?}"
    );
}

#[test]
fn a_rebuilt_view_that_held_rows_owes_the_content_sweep() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    b.clear_for_rebuild().expect("clear an empty view");
    assert!(
        !a.content_sweep_owed_for_test(),
        "a view that held nothing owes nothing"
    );

    write(&a, "x.rs", "only-a");
    publish(&a, &[("x.rs", "only-a")]);
    // The flush that wrote the file map sealed the view. ~keep
    a.seal_scan(0).expect("seal");
    sweep(&a, &["only-a"]);
    assert!(!a.content_sweep_owed_for_test());

    a.clear_for_rebuild().expect("clear");

    assert!(
        a.content_sweep_owed_for_test(),
        "contents only the rebuilt view held are dead"
    );
}

#[test]
fn a_sweep_left_owing_for_an_unsettled_view_is_not_repeated_until_something_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    publish(&a, &[]);
    // b's pass failed before its file map: its handle stays open, its content pending. ~keep
    write(&b, "x.rs", "stuck");
    assert_eq!(sweep(&a, &[]).contents_removed, 0);
    assert!(a.content_sweep_owed_for_test());

    let again = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(
        !again.owes_contents(),
        "nothing changed: the sweep would find what the last one found"
    );
    drop(again);

    write(&a, "y.rs", "fresh");
    publish(&a, &[("y.rs", "fresh")]);
    let after_write = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(after_write.owes_contents(), "a path write owes it again");
    drop(after_write);
    sweep(&a, &["fresh"]);

    publish(&b, &[]);
    let after = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(after.owes_contents(), "b's file map settled its pending content");
    after.finish(false).expect("finish");
    assert_eq!(sweep(&a, &["fresh"]).contents_removed, 1);
    assert!(!a.content_sweep_owed_for_test());
}

#[test]
fn a_pass_that_only_removes_paths_keeps_the_debt_until_its_file_map_is_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    write(&b, "x.rs", "dropped");
    publish(&b, &[("x.rs", "dropped")]);
    publish(&a, &[]);
    let mut writer = b.writer();
    writer.remove_file(&RelPath::from("x.rs")).expect("remove");
    writer.commit().expect("commit");

    // b's map on disk still names the content. ~keep
    assert_eq!(sweep(&a, &["dropped"]).contents_removed, 0);
    assert!(a.content_sweep_owed_for_test(), "b's next file map drops it");

    publish(&b, &[]);
    assert_eq!(sweep(&a, &[]).contents_removed, 1);
    assert!(!a.content_sweep_owed_for_test());
}

#[test]
fn the_pending_contents_of_a_view_no_handle_has_open_are_dropped_and_the_debt_paid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = dir.path().join("memory.fjall");
    let group = GroupDb::open_shared(&dir.path().join("group"), no_lock).expect("group");
    let (a, b) = (open_view(&group, "ws-a", &memory), open_view(&group, "ws-b", &memory));
    publish(&a, &[]);
    // b's pass ends without writing its file map, and its handle closes. ~keep
    write(&b, "x.rs", "orphan");
    assert_eq!(sweep(&a, &[]).contents_removed, 0, "pending while b is open");
    drop(b);

    let open = a.begin_group_sweep().expect("begin").expect("sweep");
    assert!(
        open.owes_contents(),
        "dropping b's pending content changed what a sweep finds"
    );
    drop(open);
    assert_eq!(sweep(&a, &[]).contents_removed, 1);
    assert!(!a.content_sweep_owed_for_test());
}
