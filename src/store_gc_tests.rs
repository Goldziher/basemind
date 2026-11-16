use super::*;
use crate::store::{BLOBS_DIR, FileEntry, INDEX_FILE, Index};
use crate::store_cache_admin::{BlobStorePaths, TELEMETRY_FILENAME, cache_stats_in, clear_component_in};
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
    /// Per-fixture blob namespace (`basemind_dir/blobs/v<schema>`), passed to the `_in` seams.
    pub(super) blobs_dir: PathBuf,
    referenced_stem: String,
    orphan_stem: String,
    orphan_len: u64,
}

impl Fixture {
    /// The fixture's blob store: `blobs_dir` is its current namespace, `gc-state.json` sits beside
    /// the store's root as it does in the cache.
    pub(super) fn blobs(&self) -> BlobStorePaths {
        BlobStorePaths {
            root: self.basemind_dir.join(BLOBS_DIR),
            schema: crate::extract::SCHEMA_VER,
            gc_state: self.basemind_dir.join(crate::store_gc_budget::GC_STATE_FILE),
        }
    }
}

pub(super) fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let basemind_dir = tmp.path().join(".basemind");
    let blobs = basemind_dir
        .join(BLOBS_DIR)
        .join(format!("v{}", crate::extract::SCHEMA_VER));
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

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs()).expect("cache_stats");

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
        + stats.memory_vectors_bytes
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

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs()).expect("cache_stats must not hard-fail");
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

    let blobs = fx.blobs_dir.clone();
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

    let blobs = fx.blobs_dir.clone();
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
    let blobs = fx.blobs_dir.clone();
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

    let before = cache_stats_in(&fx.basemind_dir, &fx.blobs()).expect("stats before");
    assert_eq!(before.blob_count, 2, "two blob files on disk");
    assert_eq!(before.orphan_blob_count, 1, "one orphan before GC");
    assert_eq!(
        before.per_view_file_count,
        vec![("working".to_string(), 1)],
        "single working view with one indexed file"
    );

    let referenced = collect_referenced_hashes(&fx.basemind_dir).expect("collect");
    gc_blobs_in(&fx.blobs_dir, &referenced, Duration::ZERO).expect("gc");

    let after = cache_stats_in(&fx.basemind_dir, &fx.blobs()).expect("stats after");
    assert_eq!(after.blob_count, 1, "orphan reaped");
    assert_eq!(after.orphan_blob_count, 0, "no orphans remain");
}

#[test]
fn should_clear_only_blobs_component() {
    let fx = build_fixture();
    fs::write(fx.basemind_dir.join(TELEMETRY_FILENAME), b"{}\n").expect("telemetry");

    clear_component_in(&fx.basemind_dir, CacheComponent::Blobs, &fx.blobs()).expect("clear blobs");

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
        stale_workspace.join(crate::store_layout::AGENT_ID_FILE),
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
        fs::read_to_string(stale_workspace.join(crate::store_layout::AGENT_ID_FILE)).unwrap(),
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

/// A view a newer basemind wrote is neither healed away nor a reason to stop: it stays as it is,
/// and it has no vote, since the blobs it names are in its own namespace. Even a newer map this
/// binary cannot decode is read as newer, not as a damaged one that may be removed.
#[test]
fn global_gc_passes_over_a_view_written_by_a_newer_basemind() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let newer_view = workspaces.join("key-newer").join(VIEWS_DIR).join("working");
    fs::create_dir_all(&newer_view).expect("mk newer view");
    let mut newer = Index::empty();
    newer.schema_ver = crate::extract::SCHEMA_VER + 1;
    newer.rebuild_pins = vec!["a".repeat(64)];
    let undecodable = workspaces.join("key-newer-shape").join(VIEWS_DIR).join("working");
    fs::create_dir_all(&undecodable).expect("mk view");
    #[derive(serde::Serialize)]
    struct AnotherShape {
        schema_ver: u16,
        files: &'static str,
    }
    let another = AnotherShape {
        schema_ver: crate::extract::SCHEMA_VER + 1,
        files: "not the map this binary expects",
    };
    fs::write(
        newer_view.join(INDEX_FILE),
        rmp_serde::to_vec_named(&newer).expect("encode newer index"),
    )
    .expect("write newer index");
    fs::write(
        undecodable.join(INDEX_FILE),
        rmp_serde::to_vec_named(&another).expect("encode"),
    )
    .expect("write");

    let referenced = collect_referenced_hashes_global_in(&workspaces).expect("a newer view does not stop the sweep");

    assert!(referenced.is_empty(), "and names nothing in this namespace");
    assert!(
        newer_view.join(INDEX_FILE).exists() && undecodable.join(INDEX_FILE).exists(),
        "the newer basemind's views survive"
    );
    assert!(matches!(read_index(&undecodable), Err(StoreError::NewerSchema { .. })));
}

/// A file map from before blobs were namespaced is stale cache like one of an older schema: the
/// sweep removes the view instead of failing on it.
#[test]
fn global_gc_heals_a_view_of_an_earlier_blob_layout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let stale_view = workspaces.join("key-flat").join(VIEWS_DIR).join("working");
    fs::create_dir_all(&stale_view).expect("mk stale view");
    let mut stale = Index::empty();
    stale.blob_layout = 0;
    fs::write(
        stale_view.join(INDEX_FILE),
        rmp_serde::to_vec_named(&stale).expect("encode stale index"),
    )
    .expect("write stale index");

    let referenced = collect_referenced_hashes_global_in(&workspaces).expect("heal");

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

/// The record of the last sweep is read from where the sweep writes it, beside the blob store's
/// root, not from inside the store.
#[test]
fn cache_stats_reports_the_last_sweep() {
    let fx = build_fixture();
    assert!(cache_stats_in(&fx.basemind_dir, &fx.blobs()).unwrap().last_gc.is_none());
    let report = GcReport {
        removed: 3,
        ..GcReport::default()
    };

    crate::store_gc_budget::persist_gc_state_at(&fx.blobs().gc_state, &report);

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs()).unwrap();
    assert_eq!(stats.last_gc.expect("the persisted sweep is reported").removed, 3);
}

/// The global record and the global store agree on where the record is: one function names it.
#[test]
fn the_global_blob_store_reads_the_record_the_sweep_persists() {
    crate::store::init_isolated_cache();
    let paths = BlobStorePaths::global();
    assert_eq!(paths.gc_state, crate::store_gc_budget::gc_state_path());
    assert_eq!(paths.current(), crate::store::global_blobs_dir());
    assert_eq!(
        paths.gc_state.parent(),
        paths.root.parent(),
        "beside blobs/, in the cache directory"
    );
}

fn plant_blob(dir: &std::path::Path, stem: char) -> PathBuf {
    fs::create_dir_all(dir).expect("mk namespace");
    let path = dir.join(format!("{}.fm.msgpack", stem.to_string().repeat(64)));
    fs::write(&path, vec![b'x'; 4096]).expect("write blob");
    path
}

/// The footprint is the whole blob store, every namespace and the un-namespaced files of older
/// builds; the current namespace is reported beside it, and blob counts are the current one's.
#[test]
fn cache_stats_measures_every_blob_namespace() {
    let fx = build_fixture();
    let root = fx.blobs().root;
    let only_current = cache_stats_in(&fx.basemind_dir, &fx.blobs()).unwrap();
    plant_blob(&root, 'c');
    plant_blob(&root.join("v1"), 'd');
    plant_blob(&root.join("v65000"), 'e');

    let stats = cache_stats_in(&fx.basemind_dir, &fx.blobs()).unwrap();

    assert_eq!(stats.blobs_current_bytes, only_current.blobs_current_bytes);
    assert_eq!(
        stats.blob_count, only_current.blob_count,
        "counted in the current namespace"
    );
    assert!(
        stats.blobs_bytes >= stats.blobs_current_bytes + 3 * 4096,
        "the other namespaces and the flat blob are measured: {} vs {}",
        stats.blobs_bytes,
        stats.blobs_current_bytes
    );
}

/// `cache clear blobs` takes what this binary and older ones wrote, and leaves a newer
/// basemind's namespace alone, naming it.
#[test]
fn clearing_blobs_takes_older_namespaces_and_flat_blobs_and_spares_a_newer_one() {
    let fx = build_fixture();
    let root = fx.blobs().root;
    let flat = plant_blob(&root, 'c');
    let older = plant_blob(&root.join("v1"), 'd');
    let newer = plant_blob(&root.join("v65000"), 'e');
    let stray = root.join("notes.txt");
    fs::write(&stray, b"not a blob").expect("write stray");

    let report = clear_component_in(&fx.basemind_dir, CacheComponent::Blobs, &fx.blobs()).expect("clear blobs");

    assert_eq!(report.spared_newer_blob_namespaces, ["v65000"]);
    assert!(newer.exists(), "a newer basemind's blobs are not ours to clear");
    assert!(!flat.exists(), "un-namespaced blobs go");
    assert!(!older.exists() && !root.join("v1").exists(), "an older namespace goes");
    assert!(stray.exists(), "a file that is not a blob is left alone");
    assert!(fx.blobs_dir.exists() && fs::read_dir(&fx.blobs_dir).unwrap().next().is_none());
}

/// The sweep of the blob store is both sweeps: the current namespace's unreferenced blobs, and
/// what older builds left beside it. A namespace of an older schema would otherwise never be
/// collected by anything.
#[test]
fn the_blob_store_sweep_reaps_the_current_namespace_and_the_superseded_ones() {
    let fx = build_fixture();
    let paths = fx.blobs();
    let workspaces = fx.basemind_dir.join("no-workspaces");
    let orphan = fx.blobs_dir.join(format!("{}.fm.msgpack", fx.orphan_stem));
    let older = plant_blob(&paths.root.join("v1"), 'd');
    let flat = plant_blob(&paths.root, 'c');
    let newer = plant_blob(&paths.root.join("v65000"), 'e');
    assert!(orphan.exists());

    let report = gc_blob_store_in(&workspaces, &paths.root, paths.schema, Duration::ZERO).expect("sweep");

    assert!(
        !orphan.exists(),
        "an unreferenced blob of the current namespace is reaped"
    );
    assert!(!older.exists() && !flat.exists(), "and so is what older builds left");
    assert!(newer.exists(), "a newer namespace is never touched");
    assert!(report.removed >= 3, "{report:?}");
}

/// The sweep the daemon and the CLI run over the machine-global store is the whole-store sweep:
/// an aged blob of an older namespace and an aged un-namespaced one go, a newer namespace stays.
#[test]
fn the_machine_global_sweep_reaps_what_older_builds_left() {
    crate::store::init_isolated_cache();
    let root = crate::store_layout::global_blobs_root();
    let aged = |path: &PathBuf| {
        let week = Duration::from_secs(7 * 24 * 60 * 60);
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now() - week))
            .expect("age the blob");
    };
    let older = plant_blob(&root.join("v1"), 'f');
    let flat = plant_blob(&root, 'g');
    let newer = plant_blob(&root.join("v65001"), 'h');
    aged(&older);
    aged(&flat);
    aged(&newer);

    // A sweep another test is running holds the machine-wide lock and makes this call a no-op.
    let mut attempts = 0;
    while (older.exists() || flat.exists()) && attempts < 100 {
        gc_global_blobs().expect("sweep");
        attempts += 1;
        if older.exists() || flat.exists() {
            std::thread::yield_now();
        }
    }

    assert!(!older.exists(), "an older namespace is swept by the global sweep");
    assert!(!flat.exists(), "so is an un-namespaced blob");
    assert!(newer.exists(), "a newer namespace is not touched");
}

/// Reaping what older builds left needs no live set. A workspace whose references cannot be read
/// fails the sweep of the current namespace, and must not also keep the old namespaces on disk.
#[test]
fn superseded_namespaces_are_reaped_although_the_live_set_cannot_be_read() {
    let fx = build_fixture();
    let paths = fx.blobs();
    let workspaces = fx.basemind_dir.join("workspaces");
    let unreadable = workspaces.join("key").join(VIEWS_DIR).join("working").join(INDEX_FILE);
    fs::create_dir_all(&unreadable).expect("a directory where the file map belongs");
    let older = plant_blob(&paths.root.join("v1"), 'd');
    let flat = plant_blob(&paths.root, 'c');
    let orphan = fx.blobs_dir.join(format!("{}.fm.msgpack", fx.orphan_stem));

    let swept = gc_blob_store_in(&workspaces, &paths.root, paths.schema, Duration::ZERO);

    assert!(
        swept.is_err(),
        "the current namespace cannot be swept without its live set"
    );
    assert!(orphan.exists(), "and nothing of it was");
    assert!(
        !older.exists() && !flat.exists(),
        "what older builds left is reaped all the same"
    );
}

/// Blobs are namespaced by schema, so a view a newer basemind wrote names nothing in this binary's
/// namespace: it neither votes in the sweep nor stops it, and it is left as it is.
#[test]
fn a_view_of_a_newer_basemind_neither_stops_the_sweep_nor_is_touched_by_it() {
    let fx = build_fixture();
    let paths = fx.blobs();
    let workspaces = fx.basemind_dir.join("workspaces");
    seed_workspace(&workspaces, "key-newer", &[&fx.orphan_stem]);
    let newer_map = workspaces
        .join("key-newer")
        .join(VIEWS_DIR)
        .join("working")
        .join(INDEX_FILE);
    let mut index: Index = rmp_serde::from_slice(&fs::read(&newer_map).expect("read")).expect("decode");
    index.schema_ver = crate::extract::SCHEMA_VER + 1;
    let bytes = rmp_serde::to_vec_named(&index).expect("encode");
    fs::write(&newer_map, &bytes).expect("write");
    let orphan = fx.blobs_dir.join(format!("{}.fm.msgpack", fx.orphan_stem));

    let report = gc_blob_store_in(&workspaces, &paths.root, paths.schema, Duration::ZERO).expect("sweep");

    assert!(report.removed >= 1, "{report:?}");
    assert!(
        !orphan.exists(),
        "a hash only the newer view names is not kept in this namespace"
    );
    assert_eq!(
        fs::read(&newer_map).expect("still there"),
        bytes,
        "the newer view is untouched"
    );
}

/// The report counts the files of the whole blob store, not of this binary's namespace alone.
#[test]
fn the_report_counts_the_blobs_of_every_namespace() {
    let root = tempfile::tempdir().expect("tempdir");
    plant_blob(root.path(), 'a');
    plant_blob(&root.path().join("v1"), 'b');
    plant_blob(&root.path().join("v2"), 'c');
    plant_blob(&root.path().join("v2"), 'd');

    assert_eq!(count_blob_files(root.path()).expect("count"), 4);
    assert_eq!(count_blob_files(&root.path().join("absent")).expect("count"), 0);
}

/// A workspace can hold a stale view beside one a newer basemind wrote. Healing the first must
/// neither remove the second nor stop over it.
#[test]
fn global_gc_heals_a_stale_view_and_keeps_the_newer_view_beside_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspaces = tmp.path().join("workspaces");
    let views = workspaces.join("key").join(VIEWS_DIR);
    let write = |view: &str, schema_ver: u16| {
        let dir = views.join(view);
        fs::create_dir_all(&dir).expect("mk view");
        let mut index = Index::empty();
        index.schema_ver = schema_ver;
        fs::write(dir.join(INDEX_FILE), rmp_serde::to_vec_named(&index).expect("encode")).expect("write");
        dir
    };
    let stale = write("working", crate::extract::SCHEMA_VER.saturating_sub(1));
    let newer = write("staged", crate::extract::SCHEMA_VER + 1);

    collect_referenced_hashes_global_in(&workspaces).expect("the sweep goes on");

    assert!(!stale.exists(), "the stale view is removed for lazy rebuild");
    assert!(
        newer.join(INDEX_FILE).exists(),
        "a newer basemind's view is never removed by GC"
    );
}
