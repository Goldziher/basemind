use super::*;

const MIB: u64 = 1 << 20;

fn estimate(index_bytes: u64, source_bytes: u64, vectors: bool) -> SpaceEstimate {
    SpaceEstimate {
        index_bytes,
        source_bytes,
        vectors,
        headroom_bytes: 512 * MIB,
    }
}

#[test]
fn a_rescan_with_no_new_source_needs_one_and_a_half_times_the_index_plus_headroom() {
    assert_eq!(estimate(100 * MIB, 0, true).required_bytes(), 150 * MIB + 512 * MIB);
    assert_eq!(estimate(0, 0, false).required_bytes(), 512 * MIB);
}

#[test]
fn a_first_scan_is_charged_for_the_index_rows_blobs_and_vectors_of_its_source() {
    let first = estimate(0, 200 * MIB, true);
    assert_eq!(first.index_growth(), 700 * MIB);
    assert_eq!(first.blob_growth(), 500 * MIB);
    assert_eq!(first.vector_growth(), 300 * MIB);
    assert_eq!(first.required_bytes(), 700 * MIB + 500 * MIB + 300 * MIB + 512 * MIB);
}

#[test]
fn no_vectors_are_charged_when_code_is_not_embedded() {
    let first = estimate(0, 200 * MIB, false);
    assert_eq!(first.vector_growth(), 0);
    assert_eq!(first.required_bytes(), 700 * MIB + 500 * MIB + 512 * MIB);
}

#[test]
fn the_existing_index_and_the_new_source_both_grow_the_index() {
    assert_eq!(
        estimate(100 * MIB, 20 * MIB, false).index_growth(),
        150 * MIB + 70 * MIB
    );
}

#[test]
fn ratios_render_as_multipliers() {
    assert_eq!(INDEX_REWRITE.to_string(), "1.5x");
    assert_eq!(INDEX_PER_SOURCE.to_string(), "3.5x");
    assert_eq!(BLOBS_PER_SOURCE.to_string(), "2.5x");
    assert_eq!(VECTORS_PER_SOURCE.to_string(), "1.5x");
}

#[test]
fn a_refusal_names_free_needed_and_every_part_of_the_estimate() {
    let err = evaluate(
        Path::new("/idx"),
        estimate(10 * MIB, 20 * MIB, true),
        600 * MIB,
        Vec::new,
    )
    .expect_err("600 MiB < 677 MiB");
    assert_eq!(err.free_bytes, 600 * MIB);
    assert_eq!(
        err.required_bytes,
        15 * MIB + 70 * MIB + 50 * MIB + 30 * MIB + 512 * MIB
    );
    assert_eq!(
        err.to_string(),
        "not enough free disk space for the index at /idx: 600 MiB free, about 677 MiB needed (85 MiB \
         index: 1.5x the 10 MiB index plus 3.5x the 20 MiB of new source; 50 MiB blobs: 2.5x the new \
         source; 30 MiB vectors: 1.5x the new source when code is embedded; 512 MiB headroom, set with \
         BASEMIND_MIN_FREE_MB=<MiB>). Free space on that volume and scan again"
    );
}

#[test]
fn a_refusal_says_what_an_older_release_left_and_how_to_remove_it() {
    let leftovers = || {
        vec![Reclaimable {
            what: "blobs of an older release".to_string(),
            path: PathBuf::from("/cache/blobs/v33"),
            bytes: 3 * MIB,
            remedy: "`basemind cache gc` removes them",
        }]
    };
    let err = evaluate(Path::new("/idx"), estimate(0, 0, false), 0, leftovers).expect_err("0 free");
    assert_eq!(err.reclaimable, leftovers());
    assert!(
        err.to_string().ends_with(
            ". To free space: 3 MiB of blobs of an older release at /cache/blobs/v33 (`basemind cache gc` \
             removes them);"
        ),
        "{err}"
    );
}

#[test]
fn what_an_older_release_left_is_gathered_only_on_a_refusal() {
    let gathered = std::cell::Cell::new(false);
    let fits = evaluate(Path::new("/idx"), estimate(0, 0, false), 512 * MIB, || {
        gathered.set(true);
        Vec::new()
    });
    assert!(fits.is_ok());
    assert!(!gathered.get());
}

#[test]
fn env_override_replaces_the_default_headroom() {
    assert_eq!(headroom_bytes(None), DEFAULT_HEADROOM_MB * MIB);
    assert_eq!(headroom_bytes(Some(" 64 ")), 64 * MIB);
    assert_eq!(headroom_bytes(Some("0")), 0);
    assert_eq!(headroom_bytes(Some("lots")), DEFAULT_HEADROOM_MB * MIB);
}

#[test]
fn dir_bytes_sums_nested_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
    std::fs::write(dir.path().join("a"), [0u8; 10]).expect("write");
    std::fs::write(dir.path().join("sub").join("b"), [0u8; 5]).expect("write");
    assert_eq!(dir_bytes(dir.path()), 15);
    assert_eq!(dir_bytes(&dir.path().join("missing")), 0);
}

#[test]
fn new_source_bytes_skips_files_over_the_cap_and_paths_that_are_gone() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("small.rs"), [0u8; 10]).expect("write");
    std::fs::write(dir.path().join("large.rs"), [0u8; 100]).expect("write");
    std::fs::create_dir(dir.path().join("dir.rs")).expect("mkdir");
    let rels = ["small.rs", "large.rs", "gone.rs", "dir.rs"];
    assert_eq!(new_source_bytes(dir.path(), rels, 50), 10);
    assert_eq!(new_source_bytes(dir.path(), rels, 100), 110);
}

fn plant(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, vec![0u8; bytes]).expect("write");
}

#[test]
fn reclaimable_finds_older_blob_namespaces_and_per_view_indexes() {
    let cache = tempfile::tempdir().expect("tempdir");
    let blobs = cache.path().join("blobs");
    plant(&blobs.join("v6").join("a.fm.msgpack"), 30);
    plant(&blobs.join("v5").join("b.fm.msgpack"), 20);
    plant(&blobs.join("v7").join("c.fm.msgpack"), 1000);
    plant(&blobs.join("v8").join("d.fm.msgpack"), 1000);
    std::fs::create_dir_all(blobs.join("v4")).expect("an empty namespace");
    let workspaces = cache.path().join("workspaces");
    plant(&workspaces.join("w1/views/working/index.fjall/journal"), 7);
    plant(&workspaces.join("w2/views/rev-abc/index.fjall/journal"), 5);
    plant(&workspaces.join("w2/views/working/index.msgpack"), 1000);

    let found = reclaimable(cache.path(), 7);

    assert_eq!(
        found,
        vec![
            Reclaimable {
                what: "blobs of an older release".to_string(),
                path: blobs.join("v5"),
                bytes: 20,
                remedy: "`basemind cache gc` removes them",
            },
            Reclaimable {
                what: "blobs of an older release".to_string(),
                path: blobs.join("v6"),
                bytes: 30,
                remedy: "`basemind cache gc` removes them",
            },
            Reclaimable {
                what: "per-view indexes of an older release in 2 workspaces".to_string(),
                path: workspaces,
                bytes: 12,
                remedy: "each goes when basemind next opens its worktree for writing",
            },
        ]
    );
}

#[test]
fn a_cache_without_leftovers_has_nothing_to_reclaim() {
    let cache = tempfile::tempdir().expect("tempdir");
    plant(&cache.path().join("blobs/v7/a.fm.msgpack"), 10);
    plant(&cache.path().join("workspaces/w1/views/working/index.msgpack"), 10);
    assert_eq!(reclaimable(cache.path(), 7), Vec::new());
}

#[test]
fn a_faked_free_figure_replaces_the_volume_on_this_thread_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_free_bytes(Some(42));
    assert_eq!(available_bytes(dir.path()), Some(42));
    let other = std::thread::spawn({
        let path = dir.path().to_path_buf();
        move || available_bytes(&path)
    })
    .join()
    .expect("join");
    assert_ne!(other, Some(42));
    fake_free_bytes(None);
    assert_ne!(available_bytes(dir.path()), Some(42));
}

#[test]
fn a_not_yet_created_view_dir_resolves_to_its_existing_ancestor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("a").join("b");
    assert_eq!(nearest_existing(&missing), dir.path());
}
