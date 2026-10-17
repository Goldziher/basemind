use super::*;
use crate::store::{BLOBS_DIR, FileEntry, INDEX_FILE, Index};
use crate::store_cache_admin::{TELEMETRY_FILENAME, cache_stats_in, clear_component_in};
use std::fs;
use std::path::PathBuf;

/// A referenced + an orphan blob, with a hand-written `views/working/index.msgpack`
/// pointing only at the referenced stem.
///
/// Since the blob store went machine-global, these tests keep their blobs in a *per-fixture*
/// temp `blobs/` dir under `basemind_dir` and drive GC / stats through the `gc_blobs_in` /
/// `cache_stats_in` seams — never the real global store. That keeps each test hermetic and
/// parallel-safe (colliding hex stems across tests can't clobber one another).
pub(super) struct Fixture {
    _tmp: tempfile::TempDir,
    pub(super) basemind_dir: PathBuf,
    /// Per-fixture blob directory (`basemind_dir/blobs`), passed to the `_in` seams.
    pub(super) blobs_dir: PathBuf,
    referenced_stem: String,
    orphan_stem: String,
    orphan_len: u64,
}

pub(super) fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let basemind_dir = tmp.path().join(".basemind");
    let blobs = basemind_dir.join(BLOBS_DIR);
    let working = basemind_dir.join(VIEWS_DIR).join("working");
    fs::create_dir_all(&blobs).expect("mk blobs");
    fs::create_dir_all(&working).expect("mk view");

    let referenced_stem = "a".repeat(64);
    let orphan_stem = "b".repeat(64);

    fs::write(blobs.join(format!("{referenced_stem}.fm.msgpack")), b"fm").expect("write ref fm");
    let orphan_bytes = b"orphan-blob-bytes";
    let orphan_len = orphan_bytes.len() as u64;
    fs::write(blobs.join(format!("{orphan_stem}.fm.msgpack")), orphan_bytes).expect("write orphan");

    let mut index = Index::empty();
    index.files.insert(
        crate::path::RelPath::from("src/main.rs"),
        FileEntry {
            hash_hex: referenced_stem.clone(),
            language: "rust".to_string(),
            size_bytes: 2,
            mtime: 0,
            extract_epoch: crate::extract::EXTRACT_EPOCH,
        },
    );
    let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
    fs::write(working.join(INDEX_FILE), bytes).expect("write index");

    Fixture {
        _tmp: tmp,
        basemind_dir,
        blobs_dir: blobs,
        referenced_stem,
        orphan_stem,
        orphan_len,
    }
}

#[test]
fn the_blob_grace_override_parses_seconds_and_falls_back_on_garbage() {
    assert_eq!(grace_from(None), BLOB_GC_GRACE);
    assert_eq!(grace_from(Some("0")), Duration::ZERO);
    assert_eq!(grace_from(Some(" 90 ")), Duration::from_secs(90));
    assert_eq!(
        grace_from(Some("soon")),
        BLOB_GC_GRACE,
        "an unparseable override is ignored"
    );
}

#[test]
fn cache_stats_counts_git_history_and_reconciles_total() {
    let fx = build_fixture();

    let gh_dir = fx.basemind_dir.join(crate::git_history::GIT_HISTORY_DIR);
    fs::create_dir_all(&gh_dir).expect("mk git-history");
    let gh_payload = b"git-history-index-bytes-XXXXXXXX";
    fs::write(gh_dir.join("commits.fjall"), gh_payload).expect("write gh blob");

    let stray = b"lockmeta";
    fs::write(fx.basemind_dir.join(".lock.meta"), stray).expect("write stray");

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("cache_stats");

    assert!(
        stats.git_history_bytes >= gh_payload.len() as u64,
        "git-history.fjall/ must be counted (got {})",
        stats.git_history_bytes
    );
    assert!(
        stats.other_bytes >= stray.len() as u64,
        "the unattributed stray file lands in other_bytes (got {})",
        stats.other_bytes
    );

    assert_eq!(
        stats.total_bytes,
        dir_size(&fx.basemind_dir).expect("dir_size") + stats.blobs_bytes,
        "total_bytes is the workspace tree plus the global blob store"
    );
    let component_sum = stats.blobs_bytes
        + stats.views_bytes
        + stats.lance_bytes
        + stats.git_cache_bytes
        + stats.telemetry_bytes
        + stats.git_history_bytes;
    assert_eq!(
        stats.total_bytes,
        component_sum + stats.other_bytes,
        "components + other must reconcile to total"
    );
}

#[test]
fn cache_stats_degrades_when_index_unreadable() {
    let fx = build_fixture();
    let working = fx.basemind_dir.join(VIEWS_DIR).join("working");
    fs::write(working.join(INDEX_FILE), b"\xff\xff not-msgpack \x00").expect("corrupt index");

    assert!(
        collect_referenced_hashes(&fx.basemind_dir).is_err(),
        "an unreadable index must fail the delete-path safety check"
    );

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("cache_stats must not hard-fail");
    assert!(
        !stats.blob_accounting_ok,
        "orphan accounting must be flagged unavailable"
    );
    assert_eq!(
        stats.orphan_blob_count, 0,
        "orphan count is 0 (skipped), not a real zero"
    );
    assert!(stats.blob_count >= 2, "blob files are still counted by size walk");
    assert!(stats.total_bytes > 0, "sizes are still reported");
    assert_eq!(
        stats.total_bytes,
        dir_size(&fx.basemind_dir).expect("dir_size") + stats.blobs_bytes,
        "total still reconciles to the workspace tree plus the blob store"
    );
}

#[test]
fn should_collect_only_referenced_stem() {
    let fx = build_fixture();
    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    assert_eq!(referenced.code.len(), 1, "exactly one live stem");
    assert!(referenced.contains(&fx.referenced_stem), "live stem present");
    assert!(
        !referenced.contains(&fx.orphan_stem),
        "orphan stem must not be referenced"
    );
}

#[test]
fn should_remove_only_orphan_blob() {
    let fx = build_fixture();
    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    let report = gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    assert_eq!(report.scanned, 2, "one ref blob + one orphan inspected");
    assert_eq!(report.removed, 1, "only the orphan removed");
    assert_eq!(
        report.bytes_freed, fx.orphan_len,
        "freed bytes equal the orphan's exact length"
    );

    let blobs = fx.basemind_dir.join(BLOBS_DIR);
    assert!(
        blobs.join(format!("{}.fm.msgpack", fx.referenced_stem)).exists(),
        "referenced filemap survives"
    );
    assert!(
        !blobs.join(format!("{}.fm.msgpack", fx.orphan_stem)).exists(),
        "orphan filemap gone"
    );
}

#[test]
fn gc_grace_keeps_young_orphan_doc_blobs() {
    let fx = build_fixture();
    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");

    let report = gc_blobs_in(&fx.blobs_dir, &referenced, Duration::from_secs(3600)).expect("gc with grace");
    assert_eq!(report.scanned, 2, "both blobs inspected");
    assert_eq!(report.removed, 0, "young orphan survives the grace window");

    let blobs = fx.basemind_dir.join(BLOBS_DIR);
    assert!(
        blobs.join(format!("{}.fm.msgpack", fx.orphan_stem)).exists(),
        "young orphan blob still on disk"
    );

    let report = gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc without grace");
    assert_eq!(report.removed, 1, "orphan reaped once outside the grace window");
}

#[test]
fn gc_sweeps_stray_tmp_files_older_than_the_grace_window_and_keeps_fresh_ones() {
    let fx = build_fixture();
    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    let old = fx.blobs_dir.join("abc.rref.msgpack.1234.ThreadId(2).tmp");
    let fresh = fx.blobs_dir.join("def.rref.msgpack.5678.ThreadId(3).tmp");
    std::fs::write(&old, b"stale").expect("write old tmp");
    std::fs::write(&fresh, b"in flight").expect("write fresh tmp");
    let long_ago = SystemTime::now() - BLOB_GC_GRACE - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .and_then(|f| f.set_modified(long_ago))
        .expect("age the old tmp");

    // Even a zero grace must not reap the fresh, possibly in-flight, tmp file.
    let report = gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");
    assert!(!old.exists(), "stale tmp swept");
    assert!(fresh.exists(), "fresh tmp kept");
    assert_eq!(report.removed, 2, "the orphan blob and the stale tmp");
}

#[test]
fn should_reclaim_legacy_split_tier_blobs_even_when_stem_is_referenced() {
    let fx = build_fixture();
    let blobs = fx.basemind_dir.join(BLOBS_DIR);
    fs::write(blobs.join(format!("{}.l1.msgpack", fx.referenced_stem)), b"legacy-l1").expect("write legacy l1");
    fs::write(blobs.join(format!("{}.l2.msgpack", fx.referenced_stem)), b"legacy-l2").expect("write legacy l2");

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    assert!(
        referenced.contains(&fx.referenced_stem),
        "stem is referenced by the live index"
    );
    let report = gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    assert_eq!(report.removed, 3, "two legacy split blobs + the orphan filemap");
    assert!(
        !blobs.join(format!("{}.l1.msgpack", fx.referenced_stem)).exists(),
        "legacy l1 reclaimed despite a referenced stem"
    );
    assert!(
        !blobs.join(format!("{}.l2.msgpack", fx.referenced_stem)).exists(),
        "legacy l2 reclaimed despite a referenced stem"
    );
    assert!(
        blobs.join(format!("{}.fm.msgpack", fx.referenced_stem)).exists(),
        "the live combined filemap survives"
    );
}

#[test]
fn should_report_one_orphan_before_gc_and_zero_after() {
    let fx = build_fixture();

    let before = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("stats before");
    assert_eq!(before.blob_count, 2, "two blob files on disk");
    assert_eq!(before.orphan_blob_count, 1, "one orphan before GC");
    assert_eq!(
        before.per_view_file_count,
        vec![("working".to_string(), 1)],
        "single working view with one indexed file"
    );

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    let after = cache_stats_in(&fx.basemind_dir, &fx.blobs_dir).expect("stats after");
    assert_eq!(after.blob_count, 1, "orphan reaped");
    assert_eq!(after.orphan_blob_count, 0, "no orphans remain");
}

#[test]
fn should_clear_only_blobs_component() {
    let fx = build_fixture();
    fs::write(fx.basemind_dir.join(TELEMETRY_FILENAME), b"{}\n").expect("telemetry");

    clear_component_in(&fx.basemind_dir, CacheComponent::Blobs, &fx.blobs_dir).expect("clear blobs");

    let blobs = &fx.blobs_dir;
    let remaining: Vec<_> = fs::read_dir(blobs)
        .expect("read blobs")
        .filter_map(Result::ok)
        .collect();
    assert!(remaining.is_empty(), "blobs dir emptied: {remaining:?}");
    assert!(blobs.exists(), "blobs dir itself preserved");

    assert!(
        fx.basemind_dir
            .join(VIEWS_DIR)
            .join("working")
            .join(INDEX_FILE)
            .exists(),
        "view index untouched by Blobs clear"
    );
    assert!(
        fx.basemind_dir.join(TELEMETRY_FILENAME).exists(),
        "telemetry untouched by Blobs clear"
    );
}

/// Build a fixture with two scanned views (`working` + `rev-abc`), each with a real
/// `index.msgpack`, sharing the blob store. Returns the basemind dir.
fn build_two_view_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let basemind_dir = tmp.path().join(".basemind");
    for view in ["working", "rev-abc"] {
        let view_dir = basemind_dir.join(VIEWS_DIR).join(view);
        fs::create_dir_all(&view_dir).expect("mk view");
        let mut index = Index::empty();
        index.files.insert(
            crate::path::RelPath::from("src/main.rs"),
            FileEntry {
                hash_hex: "a".repeat(64),
                language: "rust".to_string(),
                size_bytes: 2,
                mtime: 0,
                extract_epoch: crate::extract::EXTRACT_EPOCH,
            },
        );
        let bytes = rmp_serde::to_vec_named(&index).expect("encode");
        fs::write(view_dir.join(INDEX_FILE), bytes).expect("write index");
    }
    (tmp, basemind_dir)
}

#[test]
fn should_clear_single_view_and_leave_others_intact() {
    let (_tmp, basemind_dir) = build_two_view_fixture();

    clear_single_view(&basemind_dir, "rev-abc").expect("clear one view");

    assert!(
        !basemind_dir.join(VIEWS_DIR).join("rev-abc").exists(),
        "named view removed"
    );
    assert!(
        basemind_dir.join(VIEWS_DIR).join("working").join(INDEX_FILE).exists(),
        "other view survives single-view clear"
    );
}

#[test]
fn clear_single_view_is_idempotent_for_missing_view() {
    let (_tmp, basemind_dir) = build_two_view_fixture();
    clear_single_view(&basemind_dir, "rev-does-not-exist").expect("missing view is a no-op");
    assert!(basemind_dir.join(VIEWS_DIR).join("working").exists());
    assert!(basemind_dir.join(VIEWS_DIR).join("rev-abc").exists());
}

#[test]
fn clear_single_view_rejects_path_traversal() {
    let (_tmp, basemind_dir) = build_two_view_fixture();
    for bad in ["..", "a/b", "../escape", ""] {
        assert!(
            clear_single_view(&basemind_dir, bad).is_err(),
            "invalid view name {bad:?} must be rejected"
        );
    }
    assert!(basemind_dir.join(VIEWS_DIR).join("working").exists());
}

#[test]
fn blob_stem_recovers_stem_for_every_known_suffix() {
    assert_eq!(blob_stem("deadbeef.fm.msgpack"), Some("deadbeef"));
    assert_eq!(blob_stem("deadbeef.doc.msgpack"), Some("deadbeef"));
    assert_eq!(blob_stem("deadbeef.chunk.msgpack"), Some("deadbeef"));
    assert_eq!(blob_stem("deadbeef.rref.msgpack"), Some("deadbeef"));
    assert_eq!(blob_stem("deadbeef.tmp"), None);
}

#[test]
fn should_reclaim_unreferenced_chunk_and_rref_but_keep_referenced() {
    let fx = build_fixture();
    let blobs = &fx.blobs_dir;

    fs::write(
        blobs.join(format!("{}.chunk.msgpack", fx.referenced_stem)),
        b"ref-chunk",
    )
    .expect("ref chunk");
    fs::write(blobs.join(format!("{}.rref.msgpack", fx.referenced_stem)), b"ref-rref").expect("ref rref");
    fs::write(blobs.join(format!("{}.chunk.msgpack", fx.orphan_stem)), b"orphan-chunk").expect("orphan chunk");
    fs::write(blobs.join(format!("{}.rref.msgpack", fx.orphan_stem)), b"orphan-rref").expect("orphan rref");

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    assert!(
        blobs.join(format!("{}.chunk.msgpack", fx.referenced_stem)).exists(),
        "referenced chunk survives"
    );
    assert!(
        blobs.join(format!("{}.rref.msgpack", fx.referenced_stem)).exists(),
        "referenced rref survives"
    );
    assert!(
        !blobs.join(format!("{}.chunk.msgpack", fx.orphan_stem)).exists(),
        "orphan chunk reclaimed"
    );
    assert!(
        !blobs.join(format!("{}.rref.msgpack", fx.orphan_stem)).exists(),
        "orphan rref reclaimed"
    );
}

/// Build a workspace dir (`<workspaces>/<key>/views/working/index.msgpack`) whose index
/// references each stem in `stems`. Mirrors the global cache's per-workspace layout.
fn seed_workspace(workspaces_dir: &Path, key: &str, stems: &[&str]) {
    let working = workspaces_dir.join(key).join(VIEWS_DIR).join("working");
    fs::create_dir_all(&working).expect("mk workspace view");
    let mut index = Index::empty();
    for (i, stem) in stems.iter().enumerate() {
        index.files.insert(
            crate::path::RelPath::from(format!("src/f{i}.rs").as_str()),
            FileEntry {
                hash_hex: (*stem).to_string(),
                language: "rust".to_string(),
                size_bytes: 2,
                mtime: 0,
                extract_epoch: crate::extract::EXTRACT_EPOCH,
            },
        );
    }
    let bytes = rmp_serde::to_vec_named(&index).expect("encode index");
    fs::write(working.join(INDEX_FILE), bytes).expect("write index");
}

#[test]
fn global_gc_keeps_a_blob_referenced_by_any_workspace_and_reaps_the_orphan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");

    let stem_a = "a".repeat(64);
    let stem_b = "b".repeat(64);
    let orphan = "c".repeat(64);
    fs::write(blobs.join(format!("{stem_a}.fm.msgpack")), b"fm-a").expect("blob a");
    fs::write(blobs.join(format!("{stem_b}.fm.msgpack")), b"fm-b").expect("blob b");
    let orphan_bytes = b"orphan-blob-bytes";
    fs::write(blobs.join(format!("{orphan}.fm.msgpack")), orphan_bytes).expect("orphan blob");

    seed_workspace(&workspaces, "key-a", &[&stem_a]);
    seed_workspace(&workspaces, "key-b", &[&stem_b]);

    let referenced = collect_referenced_hashes_global_in(&workspaces).expect("union");
    assert_eq!(referenced.code.len(), 2, "the union spans both workspaces");
    assert!(referenced.contains(&stem_a) && referenced.contains(&stem_b));
    assert!(!referenced.contains(&orphan), "orphan referenced by no workspace");

    let report = gc_global_blobs_in(&workspaces, &blobs, Duration::ZERO).expect("global gc");
    assert_eq!(report.scanned, 3, "all three blobs inspected");
    assert_eq!(report.removed, 1, "only the cross-workspace orphan reaped");
    assert_eq!(report.bytes_freed, orphan_bytes.len() as u64);

    assert!(
        blobs.join(format!("{stem_a}.fm.msgpack")).exists(),
        "blob referenced by workspace A survives"
    );
    assert!(
        blobs.join(format!("{stem_b}.fm.msgpack")).exists(),
        "blob referenced by workspace B survives (union, not per-workspace)"
    );
    assert!(!blobs.join(format!("{orphan}.fm.msgpack")).exists(), "orphan reaped");
}

#[test]
fn global_gc_heals_an_unreadable_workspace_index_without_blocking_other_workspaces() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let blobs = tmp.path().join("blobs");
    fs::create_dir_all(&blobs).expect("mk blobs");
    let live = "a".repeat(64);
    let stale = "b".repeat(64);
    seed_workspace(&workspaces, "key-live", &[&live]);

    let stale_workspace = workspaces.join("key-stale");
    let stale_view = stale_workspace.join(VIEWS_DIR).join("working");
    fs::create_dir_all(&stale_view).expect("mk view");
    fs::write(
        stale_workspace.join(crate::comms::identity::AGENT_ID_FILE),
        b"session-stable",
    )
    .expect("write stable identity");
    fs::write(stale_view.join(INDEX_FILE), b"\xff\xff not-msgpack \x00").expect("corrupt index");
    fs::write(blobs.join(format!("{live}.fm.msgpack")), b"live").expect("live blob");
    fs::write(blobs.join(format!("{stale}.fm.msgpack")), b"stale").expect("stale blob");

    let report = gc_global_blobs_in(&workspaces, &blobs, Duration::ZERO).expect("gc heals stale view");

    assert_eq!(report.removed, 1, "the stale workspace no longer pins its blob");
    assert!(
        blobs.join(format!("{live}.fm.msgpack")).exists(),
        "live reference survives"
    );
    assert!(
        !blobs.join(format!("{stale}.fm.msgpack")).exists(),
        "stale orphan is reclaimed"
    );
    assert!(!stale_view.exists(), "the unreadable rebuildable view is quarantined");
    assert_eq!(
        fs::read_to_string(stale_workspace.join(crate::comms::identity::AGENT_ID_FILE)).unwrap(),
        "session-stable",
        "healing a view must preserve stable identity metadata"
    );
}

#[test]
fn global_gc_heals_a_mixed_release_workspace_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let stale_view = workspaces.join("key-v23").join(VIEWS_DIR).join("working");
    fs::create_dir_all(&stale_view).expect("mk stale view");
    let mut stale = Index::empty();
    stale.schema_ver = crate::extract::SCHEMA_VER.saturating_sub(1);
    fs::write(
        stale_view.join(INDEX_FILE),
        rmp_serde::to_vec_named(&stale).expect("encode stale index"),
    )
    .expect("write stale index");

    let referenced = collect_referenced_hashes_global_in(&workspaces).expect("heal mixed schema");

    assert!(referenced.is_empty());
    assert!(!stale_view.exists(), "the stale view is removed for lazy rebuild");
}

#[test]
fn should_round_trip_component_tokens() {
    for component in [
        CacheComponent::Blobs,
        CacheComponent::Views,
        CacheComponent::Lance,
        CacheComponent::GitCache,
        CacheComponent::Telemetry,
        CacheComponent::All,
    ] {
        let token = component.as_str();
        let parsed: CacheComponent = token.parse().expect("parse token");
        assert_eq!(parsed, component, "round-trip {token}");
    }
    assert!("nonsense".parse::<CacheComponent>().is_err(), "unknown token rejected");
}
