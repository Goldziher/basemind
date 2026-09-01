//! The drive loop's memory bound, asserted end to end (issue #62).
//!
//! `scanner_drive` exists so that live `FileResult`s are O(drive chunk) instead of O(corpus): the
//! scan used to materialise every candidate's result — staged `FileEntry`, pending document batch,
//! pending code-chunk batch — into one `Vec` and only then drain it. `ScanObserver::on_batch`
//! reports how many results one chunk held live at once, which is the only externally observable
//! witness of that bound, and nothing in the tree read it until this test.
//!
//! This file holds exactly ONE test on purpose. The scanner's rayon pool is a process-global
//! `OnceLock` whose size is fixed by the first scan in the process, and the baseline chunk is
//! derived from that size — a second test in this binary scanning under a different
//! `[resources] scan_threads` would silently inherit the first one's pool and make the expected
//! chunk non-deterministic.

use std::fs;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, FileResult, ScanCancel, ScanObserver, ScanSource, scan_with_observer};
use basemind::store::Store;

/// Pool size pinned via `[resources] scan_threads`, so the baseline chunk below is a constant
/// rather than a function of whatever machine runs the suite.
const SCAN_THREADS: usize = 2;

/// The baseline drive chunk for [`SCAN_THREADS`] workers, derived the way `scanner_drive`'s
/// `base_chunk_items` derives it: `max(1024, workers * 64)`. Mirrored rather than imported because
/// `scanner_drive` is `pub(crate)`; a change to either constant must be reflected here, which is
/// the point — the bound is what this test pins.
const EXPECTED_CHUNK: usize = 1024;

/// Candidates to scan. Comfortably more than two chunks, so a corpus-sized `Vec` is
/// distinguishable from a chunk-sized one by more than an off-by-one.
const FILES: usize = 2600;

/// Records the largest `live_results` any single drive chunk reported.
#[derive(Default)]
struct PeakBatchObserver {
    peak_live: usize,
    batches: Vec<usize>,
    files: usize,
}

impl ScanObserver for PeakBatchObserver {
    fn on_file(&mut self, _result: FileResult) {
        self.files += 1;
    }

    fn on_batch(&mut self, live_results: usize) {
        self.peak_live = self.peak_live.max(live_results);
        self.batches.push(live_results);
    }
}

#[test]
fn the_drive_loop_never_holds_more_than_one_chunk_of_results_live() {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    for i in 0..FILES {
        // Distinct bodies so no two files share a content hash — a deduplicated corpus would ~keep
        // still exercise the drive loop, but it would stop exercising the per-file work it drives. ~keep
        fs::write(
            src.join(format!("f{i}.rs")),
            format!("pub fn generated_{i}() -> usize {{ {i} }}\n"),
        )
        .expect("write candidate");
    }

    let mut cfg = ConfigV1::with_defaults();
    cfg.resources.scan_threads = SCAN_THREADS;
    // Neither tier participates in the bound under test, and both add nondeterministic work ~keep
    // (model downloads, LanceDB flushes) on a `full` build. ~keep
    cfg.code_search.enabled = false;
    cfg.code_search.embed = false;
    cfg.documents.enabled = false;

    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    let mut observer = PeakBatchObserver::default();
    let report = scan_with_observer(
        root,
        &mut store,
        &cfg,
        ScanSource::WorkingTree,
        EmbedMode::Deferred,
        &ScanCancel::new(),
        &mut observer,
    )
    .expect("scan");

    assert_eq!(report.stats.updated, FILES, "every candidate must have been indexed");
    assert_eq!(observer.files, FILES, "every result must have reached the observer");
    assert_eq!(
        observer.batches.iter().sum::<usize>(),
        FILES,
        "the chunks must partition the corpus, not sample it"
    );
    assert!(
        observer.peak_live <= EXPECTED_CHUNK,
        "the drive loop held {} results live at once — the bound is {EXPECTED_CHUNK} \
         (issue #62: live per-file state must be O(chunk), never O(corpus))",
        observer.peak_live
    );
    assert!(
        observer.batches.len() > 2,
        "with {FILES} candidates and a {EXPECTED_CHUNK}-item chunk the drive must cut several \
         chunks; {} chunk(s) means this test is not observing the bound at all",
        observer.batches.len()
    );
}
