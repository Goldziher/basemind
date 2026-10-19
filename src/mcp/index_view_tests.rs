//! Proof that the read paths reach the fjall index only through [`IndexView`](crate::index::view):
//! each test hands the code under test a [`RecordingView`] and asserts on what was read. A layered
//! implementation can therefore replace the view without touching a call site.

use super::helpers_calls_scan::{calls_in_file_fjall, scan_calls_by_name};
use super::helpers_impls::scan_impls_fjall;
use super::index_route::IndexRoute;
use crate::config::ConfigV1;
use crate::index::grep_bloom::{GrepEntry, Needle};
use crate::index::view::{IndexView, ReadLog, RecordingView};
use crate::path::RelPath;
use crate::scanner::{EmbedMode, ScanSource, scan};
use crate::store::{Store, VIEW_WORKING};

const FILES: usize = 6;

fn scanned_store(root: &std::path::Path) -> Store {
    for k in 0..FILES {
        std::fs::write(
            root.join(format!("m{k}.rs")),
            format!(
                "pub trait Tr{k} {{ fn run(&self); }}\n\
                 pub struct S{k};\n\
                 impl Tr{k} for S{k} {{ fn run(&self) {{ shared_helper(); }} }}\n\
                 pub fn driver_{k}() {{ shared_helper(); local_{k}(); }}\n"
            ),
        )
        .expect("write corpus file");
    }
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    scan(
        root,
        &mut store,
        &ConfigV1::with_defaults(),
        ScanSource::WorkingTree,
        EmbedMode::Inline,
    )
    .expect("scan");
    store
}

fn recording(store: &Store) -> (RecordingView, ReadLog) {
    RecordingView::new(store.index_db.clone().expect("index"))
}

fn grep_entries(store: &Store) -> Vec<GrepEntry> {
    (0..FILES)
        .map(|k| {
            let rel = RelPath::from(format!("m{k}.rs").as_bytes());
            let entry = store.lookup(&rel).expect("indexed");
            GrepEntry {
                rel,
                hash_hex: entry.hash_hex.clone(),
                size_bytes: entry.size_bytes,
                mtime_ns: entry.mtime,
            }
        })
        .collect()
}

#[test]
fn call_and_implementation_scans_read_through_the_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let db = store.index_db.clone().expect("index");
    let (view, log) = recording(&store);

    let direct = scan_calls_by_name(&db.view(), "shared_helper", 50, None).expect("direct scan");
    let routed = scan_calls_by_name(&view, "shared_helper", 50, None).expect("routed scan");
    assert_eq!(routed.total, 2 * FILES as u32, "two call sites per file");
    assert_eq!((routed.total, &routed.hit_keys), (direct.total, &direct.hit_keys));
    assert!(log.saw("callee_keys_matching"), "name scan: {:?}", log.entries());
    assert!(log.saw("get:calls_by_path"), "line/column resolve: {:?}", log.entries());

    log.clear();
    let calls = calls_in_file_fjall(&view, &RelPath::from("m0.rs")).expect("calls");
    assert!(!calls.is_empty());
    assert!(log.saw("range:calls_by_path"), "per-file calls: {:?}", log.entries());

    log.clear();
    let impls = scan_impls_fjall(&view, "Tr", None, 50, None, |_, _| true, |_, _| (0, 0)).expect("impls");
    assert_eq!(impls.total, FILES);
    assert!(log.saw("trait_keys_matching"), "implementations: {:?}", log.entries());
}

#[test]
fn probe_and_grep_prefilter_reads_go_through_the_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let (view, log) = recording(&store);

    let probed = crate::intel::resolve_pass::probed_only(&view, vec!["nowhere.rs".to_owned()]);
    assert!(probed.is_empty());
    assert!(log.saw("is_probed"), "watcher probe filter: {:?}", log.entries());

    log.clear();
    let entries = grep_entries(&store);
    let needle = Needle::compile("zzz_definitely_absent").expect("needle");
    let verdicts = view.grep_skip_verdicts(dir.path(), &entries, &needle);
    assert_eq!(verdicts.len(), entries.len());
    assert!(log.saw("get:grep_bloom"), "grep prefilter: {:?}", log.entries());
}

#[cfg(feature = "code-search")]
#[test]
fn both_search_lanes_read_through_the_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let (view, log) = recording(&store);

    let exact = crate::search::exact::exact_lane_chunk_ids(&store, &view, "driver_1", 10);
    assert!(!exact.is_empty(), "the exact lane resolves a defined function");
    assert!(log.saw("symbols_by_name_lookup"), "exact lane: {:?}", log.entries());

    log.clear();
    let keyword = crate::search::bm25::bm25_search(&view, "driver", 10);
    assert!(!keyword.is_empty(), "the keyword lane finds the chunk");
    assert!(log.saw("bm25_stats"), "keyword lane stats: {:?}", log.entries());
    assert!(
        log.saw("prefix:code_bm25_postings"),
        "keyword lane postings: {:?}",
        log.entries()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_local_route_answers_every_read_from_its_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = scanned_store(dir.path());
    let cache = super::MapCache::build(&store, 0);
    let (view, log) = recording(&store);
    let route = IndexRoute::Local(Box::new(view));

    let page = route.scan_calls(&cache, "shared_helper", 50, None).await.expect("scan");
    assert_eq!(page.total, 2 * FILES as u32);
    assert!(
        log.saw("callee_keys_matching"),
        "references/callers: {:?}",
        log.entries()
    );

    log.clear();
    let paths = vec![RelPath::from("m1.rs")];
    let per_file = route.calls_in_files(&cache, &paths).await.expect("calls");
    assert!(!per_file[0].is_empty());
    assert!(log.saw("range:calls_by_path"), "resolved callers: {:?}", log.entries());

    log.clear();
    let impls = route.scan_impls(&cache, "Tr", None, 50, None).await.expect("impls");
    assert_eq!(impls.total, FILES);
    assert!(log.saw("trait_keys_matching"), "implementations: {:?}", log.entries());

    log.clear();
    let entries = grep_entries(&store);
    let verdicts = route
        .grep_skip(dir.path(), "zzz_definitely_absent", &entries)
        .await
        .expect("verdicts");
    assert_eq!(verdicts.len(), entries.len());
    assert!(log.saw("get:grep_bloom"), "grep prefilter: {:?}", log.entries());
}
