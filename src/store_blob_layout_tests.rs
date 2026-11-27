use super::*;

const HASH: &str = "ab0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";
const FM: &str = ".fm.msgpack";

fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

#[test]
fn the_sharded_path_is_the_bucket_of_the_first_two_digits() {
    let dir = Path::new("/blobs");
    assert_eq!(
        sharded_blob_path(dir, HASH, FM),
        Path::new("/blobs/ab").join(format!("{HASH}.fm.msgpack"))
    );
    assert_eq!(
        flat_blob_path(dir, HASH, FM),
        Path::new("/blobs").join(format!("{HASH}.fm.msgpack"))
    );
}

#[test]
fn a_stem_shorter_than_a_bucket_name_is_its_own_bucket() {
    assert_eq!(
        sharded_blob_path(Path::new("/b"), "a", ".fm.msgpack"),
        Path::new("/b/a/a.fm.msgpack")
    );
}

const LAYOUTS: [Layout; 2] = [Layout::Flat, Layout::Sharded];

/// A release of 0.34 before the sharded layout reads flat names only, and every 0.34 release shares
/// the `v34` namespace: it stays flat. The next namespace is the first a flat-only release never reads.
#[test]
fn the_namespaces_up_to_the_last_flat_schema_stay_flat_and_later_ones_are_sharded() {
    assert_eq!(LAST_FLAT_BLOB_SCHEMA, 34);
    assert_eq!(layout_of(LAST_FLAT_BLOB_SCHEMA), Layout::Flat);
    assert_eq!(layout_of(LAST_FLAT_BLOB_SCHEMA + 1), Layout::Sharded);
}

/// A later 0.34 patch writes and keeps every blob of the `v34` namespace where an earlier 0.34
/// release looks for it: a read moves nothing into a bucket.
#[test]
fn a_flat_namespace_keeps_its_blobs_flat() {
    let layout = layout_of(LAST_FLAT_BLOB_SCHEMA);
    let tmp = tempfile::tempdir().unwrap();
    let flat = flat_blob_path(tmp.path(), HASH, FM);
    assert_eq!(
        locate_in(layout, tmp.path(), HASH, FM),
        flat,
        "a writer creates it flat"
    );
    touch(&flat);

    assert_eq!(locate_in(layout, tmp.path(), HASH, FM), flat);
    assert!(
        flat.is_file(),
        "a read leaves the flat blob where a flat-only release reads it"
    );
    assert!(!tmp.path().join("ab").exists(), "no bucket is created");
}

#[test]
fn locate_moves_a_blob_of_the_other_layout_into_place() {
    for layout in LAYOUTS {
        let tmp = tempfile::tempdir().unwrap();
        let displaced = layout.other().path(tmp.path(), HASH, FM);
        touch(&displaced);

        let found = locate_in(layout, tmp.path(), HASH, FM);

        assert_eq!(found, layout.path(tmp.path(), HASH, FM), "{layout:?}");
        assert!(found.is_file(), "{layout:?}");
        assert!(!displaced.exists(), "{layout:?}");
    }
}

#[test]
fn locate_prefers_the_blob_in_place_and_leaves_its_twin_alone() {
    for layout in LAYOUTS {
        let tmp = tempfile::tempdir().unwrap();
        let placed = layout.path(tmp.path(), HASH, FM);
        let twin = layout.other().path(tmp.path(), HASH, FM);
        touch(&placed);
        touch(&twin);

        assert_eq!(locate_in(layout, tmp.path(), HASH, FM), placed, "{layout:?}");
        assert!(twin.exists(), "{layout:?}: only a sweep reaps the twin");
    }
}

#[test]
fn moving_never_replaces_a_blob_already_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = flat_blob_path(tmp.path(), HASH, FM);
    let sharded = sharded_blob_path(tmp.path(), HASH, FM);
    touch(&flat);
    std::fs::write(&flat, b"old").unwrap();
    touch(&sharded);
    std::fs::write(&sharded, b"new").unwrap();

    assert!(!move_blob(&flat, &sharded));

    assert_eq!(std::fs::read(&sharded).unwrap(), b"new");
    assert_eq!(std::fs::read(&flat).unwrap(), b"old");
}

#[test]
fn locate_of_an_absent_blob_is_the_path_in_place_and_creates_nothing() {
    for layout in LAYOUTS {
        let tmp = tempfile::tempdir().unwrap();

        assert_eq!(
            locate_in(layout, tmp.path(), HASH, FM),
            layout.path(tmp.path(), HASH, FM),
            "{layout:?}"
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "{layout:?}");
    }
}

/// Only the copy outside the layout is a twin, and only while the copy in place exists.
#[test]
fn a_displaced_twin_is_the_copy_outside_the_layout_of_a_blob_in_place() {
    for layout in LAYOUTS {
        let tmp = tempfile::tempdir().unwrap();
        let placed = layout.path(tmp.path(), HASH, FM);
        let twin = layout.other().path(tmp.path(), HASH, FM);
        touch(&twin);
        assert!(
            !is_displaced_twin_in(layout, tmp.path(), &twin, HASH, FM),
            "{layout:?}: a blob with no copy in place is the only copy"
        );

        touch(&placed);

        assert!(is_displaced_twin_in(layout, tmp.path(), &twin, HASH, FM), "{layout:?}");
        assert!(
            !is_displaced_twin_in(layout, tmp.path(), &placed, HASH, FM),
            "{layout:?}"
        );
    }
}

#[test]
fn blob_files_lists_flat_and_bucketed_files_but_not_other_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = tmp.path().join("flat.fm.msgpack");
    let bucketed = sharded_blob_path(tmp.path(), HASH, FM);
    let not_a_bucket = tmp.path().join("notes").join("kept.fm.msgpack");
    let uppercase = tmp.path().join("CD").join("upper.fm.msgpack");
    for path in [&flat, &bucketed, &not_a_bucket, &uppercase] {
        touch(path);
    }

    let mut found = blob_files(tmp.path()).unwrap();
    found.sort();

    let mut expected = vec![flat, bucketed];
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn blob_files_of_a_missing_directory_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(blob_files(&tmp.path().join("absent")).unwrap().is_empty());
}
