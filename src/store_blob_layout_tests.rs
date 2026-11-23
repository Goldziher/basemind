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

#[test]
fn locate_moves_a_flat_blob_into_its_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = flat_blob_path(tmp.path(), HASH, FM);
    touch(&flat);

    let found = locate_blob(tmp.path(), HASH, FM);

    assert_eq!(found, sharded_blob_path(tmp.path(), HASH, FM));
    assert!(found.is_file());
    assert!(!flat.exists());
}

#[test]
fn locate_prefers_a_bucketed_blob_and_leaves_a_flat_twin_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = flat_blob_path(tmp.path(), HASH, FM);
    let sharded = sharded_blob_path(tmp.path(), HASH, FM);
    touch(&flat);
    touch(&sharded);

    assert_eq!(locate_blob(tmp.path(), HASH, FM), sharded);
    assert!(flat.exists(), "only a sweep reaps the twin");
}

#[test]
fn migrating_never_replaces_a_blob_already_in_the_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = flat_blob_path(tmp.path(), HASH, FM);
    let sharded = sharded_blob_path(tmp.path(), HASH, FM);
    touch(&flat);
    std::fs::write(&flat, b"old").unwrap();
    touch(&sharded);
    std::fs::write(&sharded, b"new").unwrap();

    assert!(!migrate_blob(&flat, &sharded));

    assert_eq!(std::fs::read(&sharded).unwrap(), b"new");
    assert_eq!(std::fs::read(&flat).unwrap(), b"old");
}

#[test]
fn locate_of_an_absent_blob_is_the_sharded_path_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();

    assert_eq!(
        locate_blob(tmp.path(), HASH, FM),
        sharded_blob_path(tmp.path(), HASH, FM)
    );
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
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
