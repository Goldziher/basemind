use super::*;

fn stamp_schema_ver(basemind_dir: &Path, ver: u32) {
    let index = GitHistoryIndex::open(basemind_dir).expect("open index");
    let db = index.local();
    db.meta.insert(keys::META_ROOT_SHA, vec![7u8; 20]).expect("write a row");
    db.meta
        .insert(keys::META_SCHEMA_VER, ver.to_be_bytes())
        .expect("stamp schema version");
}

fn raw_meta(basemind_dir: &Path, key: &[u8]) -> Option<Vec<u8>> {
    let db = Database::builder(basemind_dir.join(GIT_HISTORY_DIR))
        .worker_threads(1)
        .open()
        .expect("open raw index");
    let meta = db.keyspace("gh_meta", keyspace_options).expect("meta keyspace");
    meta.get(key).expect("read meta").map(|value| value.to_vec())
}

/// `max_memtable_size` has no getter and fjall persists it at keyspace creation, so this
/// observes it after a reopen: writing past the budget, but far below fjall's 64 MiB default,
/// must rotate and flush the memtable.
#[test]
fn a_reopened_keyspace_flushes_at_its_memtable_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    drop(GitHistoryIndex::open(dir.path()).expect("create index"));
    let index = GitHistoryIndex::open(dir.path()).expect("reopen index");
    let keyspace = &index.local().commit_text_by_ord;
    let value = vec![0u8; 4_096];
    let rows = 12 * 1024 * 1024 / value.len();
    for row in 0..rows {
        keyspace.insert(row.to_be_bytes(), value.as_slice()).expect("insert");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while keyspace.disk_space() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        keyspace.disk_space() > 0,
        "no flush after {} bytes: fjall kept its 64 MiB default memtable",
        rows * value.len()
    );
}

#[test]
fn open_refuses_an_index_stamped_by_a_newer_basemind_and_leaves_it_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let newer = GIT_HISTORY_SCHEMA + 1;
    stamp_schema_ver(dir.path(), newer);

    let Err(error) = GitHistoryIndex::open(dir.path()) else {
        panic!("a newer git-history index must not open");
    };

    assert!(
        matches!(
            error,
            GitHistoryError::NewerSchema { stored, current } if stored == newer && current == GIT_HISTORY_SCHEMA
        ),
        "expected NewerSchema, got: {error}"
    );
    assert!(
        error.to_string().contains("upgrade basemind"),
        "the error must be actionable, got: {error}"
    );
    assert_eq!(
        raw_meta(dir.path(), keys::META_SCHEMA_VER),
        Some(newer.to_be_bytes().to_vec()),
        "the newer index keeps its stamp: it was neither wiped nor restamped"
    );
    assert_eq!(
        raw_meta(dir.path(), keys::META_ROOT_SHA),
        Some(vec![7u8; 20]),
        "the newer index keeps its rows"
    );
}

#[test]
fn open_sweeps_a_directory_an_interrupted_wipe_set_aside() {
    let dir = tempfile::tempdir().expect("tempdir");
    let aside = dir.path().join(format!("{DISCARD_PREFIX}1-0"));
    std::fs::create_dir_all(aside.join("journals")).expect("a half-deleted leftover");

    drop(GitHistoryIndex::open(dir.path()).expect("open"));

    assert!(!aside.exists(), "the leftover is deleted on the next open");
}

#[test]
fn clear_drops_the_head_with_every_row_and_keeps_the_stamp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = GitHistoryIndex::open(dir.path()).expect("open");
    let sha = "11".repeat(20);
    let sha20 = keys::sha_hex_to_raw(&sha).expect("a valid sha");
    let mut writer = index.writer().expect("writer");
    writer.begin().expect("mark the build");
    let meta = CommitMeta {
        sha,
        summary: "c1".to_owned(),
        author: "t".to_owned(),
        author_email: "t@e.x".to_owned(),
        author_time_unix: 0,
        files: Vec::new(),
    };
    writer.put_commit_meta(0, &meta).expect("commit row");
    writer.put_ord_for_sha(&sha20, 0).expect("sha row");
    writer
        .finish_meta(&sha20, &reach::Reach::from_ords([0]), &sha20, 1, 0, 1)
        .expect("stamp the head");
    assert_eq!(index.last_indexed_head(), Some(sha20));

    index.clear().expect("clear");

    assert_eq!(index.last_indexed_head(), None);
    assert!(!index.build_interrupted());
    assert_eq!(index.ord_for_sha(&sha20), None);
    assert_eq!(index.commit_count(), 0);
    drop(index);
    assert_eq!(
        raw_meta(dir.path(), keys::META_SCHEMA_VER),
        Some(GIT_HISTORY_SCHEMA.to_be_bytes().to_vec())
    );
}

/// An older patch binary of the same minor shares this index and knows no in-progress mark:
/// had the head survived a cut-off append, it would append past the rows the cut left.
#[test]
fn a_build_in_progress_leaves_no_head_on_disk_for_a_binary_that_ignores_the_mark() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = GitHistoryIndex::open(dir.path()).expect("open");
    let sha20 = [0x22u8; 20];
    index
        .writer()
        .expect("writer")
        .finish_meta(&sha20, &reach::Reach::default(), &sha20, 0, 0, 0)
        .expect("stamp a head");
    index.writer().expect("writer").begin().expect("mark an append");
    drop(index);

    assert_eq!(raw_meta(dir.path(), keys::META_BUILD_IN_PROGRESS), Some(vec![1u8]));
    assert_eq!(
        raw_meta(dir.path(), keys::META_LAST_HEAD),
        None,
        "no head to append from"
    );
}

#[test]
fn open_wipes_an_index_stamped_by_an_older_basemind() {
    let dir = tempfile::tempdir().expect("tempdir");
    stamp_schema_ver(dir.path(), GIT_HISTORY_SCHEMA - 1);

    drop(GitHistoryIndex::open(dir.path()).expect("an older index is wiped and reopened"));

    assert_eq!(
        raw_meta(dir.path(), keys::META_SCHEMA_VER),
        Some(GIT_HISTORY_SCHEMA.to_be_bytes().to_vec()),
        "the rebuilt index carries the current stamp"
    );
    assert_eq!(
        raw_meta(dir.path(), keys::META_ROOT_SHA),
        None,
        "rows written under the older schema are gone"
    );
    let leftovers = std::fs::read_dir(dir.path())
        .expect("read the workspace dir")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(DISCARD_PREFIX))
        .count();
    assert_eq!(leftovers, 0, "the wiped database was set aside and deleted");
}
#[test]
fn the_tips_over_the_cap_are_the_ones_synced_least_recently_and_never_the_one_being_written() {
    let tips: Vec<([u8; 20], u64)> = (1u8..=4).map(|n| ([n; 20], u64::from(n) * 10)).collect();

    assert_eq!(
        evicted_tips(tips.clone(), &[9; 20], 5),
        Vec::<[u8; 20]>::new(),
        "room for one more"
    );
    assert_eq!(evicted_tips(tips.clone(), &[9; 20], 4), vec![[1; 20]]);
    assert_eq!(evicted_tips(tips.clone(), &[9; 20], 2), vec![[1; 20], [2; 20], [3; 20]]);
    assert_eq!(
        evicted_tips(tips, &[1; 20], 4),
        Vec::<[u8; 20]>::new(),
        "the tip being rewritten is not counted twice, nor dropped"
    );
}

#[test]
fn every_stamped_tip_keeps_its_own_reachability_and_the_oldest_beyond_the_cap_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = GitHistoryIndex::open(dir.path()).expect("open");
    let root = [0xeeu8; 20];
    let stamp = |n: usize| {
        let mut head = [0u8; 20];
        head[..8].copy_from_slice(&(n as u64).to_be_bytes());
        head
    };
    for n in 0..MAX_TIPS + 3 {
        let head = stamp(n);
        // Distinct stamps: the tip written first is the least recently synced.
        index
            .local()
            .meta
            .insert(
                keys::tip_key(&head),
                [
                    (n as u64).to_be_bytes().as_slice(),
                    reach::Reach::from_ords([n as u32]).as_bytes(),
                ]
                .concat(),
            )
            .expect("seed a tip");
    }
    let newest = stamp(MAX_TIPS + 3);
    index
        .writer()
        .expect("writer")
        .finish_meta(&newest, &reach::Reach::from_ords([5, 6]), &root, 7, 0, 7)
        .expect("stamp a tip");

    assert_eq!(index.tips().len(), MAX_TIPS, "the tips are capped");
    assert!(index.has_tip(&keys::sha_raw_to_hex(&newest)));
    assert!(
        !index.has_tip(&keys::sha_raw_to_hex(&stamp(0))),
        "the oldest is dropped"
    );
    assert!(!index.has_tip(&keys::sha_raw_to_hex(&stamp(3))), "and the next oldest");
    assert!(index.has_tip(&keys::sha_raw_to_hex(&stamp(4))));
    assert_eq!(index.reach_of(&newest), Some(reach::Reach::from_ords([5, 6])));
    assert_eq!(index.reach_of(&stamp(10)), Some(reach::Reach::from_ords([10])));
    assert_eq!(
        index.tips()[0],
        keys::sha_raw_to_hex(&newest),
        "most recently synced first"
    );
}

#[test]
fn a_tip_is_ordered_only_when_its_stamp_says_so_and_loses_it_when_restamped_unordered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = GitHistoryIndex::open(dir.path()).expect("open");
    let head = [0x33u8; 20];
    let stamp = |ordered: bool| {
        let mut writer = index.writer().expect("writer");
        writer.set_ordered(ordered);
        writer
            .finish_meta(&head, &reach::Reach::from_ords([0]), &head, 1, 0, 1)
            .expect("stamp the tip");
    };

    assert!(!index.tip_is_ordered(&head), "a tip nobody stamped is not ordered");
    stamp(false);
    assert!(!index.tip_is_ordered(&head), "stamped unordered");
    stamp(true);
    assert!(index.tip_is_ordered(&head));
    assert!(
        index.at(&keys::sha_raw_to_hex(&head)).is_some(),
        "an ordered tip is served"
    );
    stamp(false);
    assert!(!index.tip_is_ordered(&head), "a restamp replaces the mark");
    assert!(index.has_tip(&keys::sha_raw_to_hex(&head)));
    assert!(
        index.at(&keys::sha_raw_to_hex(&head)).is_none(),
        "an unordered tip is read by the live walk"
    );
}
