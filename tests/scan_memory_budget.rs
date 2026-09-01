//! Peak-RSS budget for a full scan: the one test that measures the thing issue #62 was about.
//!
//! Every other bound this release added is checked structurally — a chunked drive loop that cannot
//! hold more than a chunk of results, a staged-byte counter that forces a commit, a byte-charged
//! LRU that evicts. Those are the *right* kind of check, and they are the reason the bounds hold.
//! But each one asserts a property of a component. None of them answers the question the issue
//! actually asked: does the process, end to end, stay inside a memory budget while indexing a
//! corpus far larger than that budget?
//!
//! This test answers it, and deliberately does so as an **affine** bound —
//! `peak_delta <= BASE_OVERHEAD + PER_FILE * files` — rather than a flat ceiling. A scan is allowed
//! to cost something per file (the index does grow), so a flat number would either be so loose it
//! proved nothing or so tight it flaked. What must never happen is the *superlinear* growth of
//! materialising the whole corpus at once, which is exactly what an affine bound rejects.
//!
//! ## Why this file is its own binary
//!
//! `scanner_pool` is a process-wide `OnceLock`, so the first scan in a process fixes the thread
//! count for every later one, and thread stacks and rayon's per-worker arenas are a large part of
//! what is being measured. Sharing a binary with other tests would mean measuring whatever they
//! happened to allocate first. Cargo gives each `tests/*.rs` its own process, so this file is the
//! isolation mechanism.
//!
//! ## Why the measurement is `ru_maxrss`
//!
//! `getrusage(RUSAGE_SELF).ru_maxrss` is a kernel-maintained high-water mark: it cannot miss a
//! spike between samples, which a polling thread reading current RSS certainly can — and a spike
//! that lands between two samples is precisely the failure mode here. The cost of that reliability
//! is that it is monotonic and never falls, so the baseline has to be taken after everything
//! one-time has already been paid. See `warm_up` below.
//!
//! ## Why the corpus is synthetic, and why embeddings and documents are off
//!
//! A real repository makes the numbers depend on whatever happens to be checked out, so the bound
//! could only ever be relative. Generated files make an *absolute* assertion possible. Embeddings
//! are off because loading an ONNX model would dwarf and hide the scan itself — the same reason
//! `tests/embed_streaming_smoke.rs` documents that it cannot assert an absolute ceiling.

use std::path::Path;

use basemind::config::Config;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

/// Files in the measured corpus. Large enough that materialising all of them at once would show up
/// far outside the budget, small enough that the test stays a few seconds.
const FILES: usize = 4_000;

/// Roughly the size of an ordinary source file. Uniform on purpose: the point is the *count*
/// scaling, and mixing in a few huge files would make a failure ambiguous between "the drive loop
/// leaked" and "one file was big".
const LINES_PER_FILE: usize = 120;

/// Fixed allowance for everything that does not scale with the corpus: the rayon pool's worker
/// stacks, tree-sitter's parser and language objects, fjall's memtables and block cache, and the
/// allocator's own retained arenas.
///
/// Calibrated, not guessed. Measured on this tree at two corpus sizes — 3 000 files cost 158 MiB
/// and 9 000 cost 223 MiB — which is a slope of ~11.4 KiB per file and an intercept of ~125 MiB.
/// This is that intercept with roughly 2x headroom for machine variance. The first draft of this
/// test used 768 MiB here and 96 KiB below, which made the budget about seven times the thing it
/// was supposed to catch: it passed at 15% utilisation and could not have failed. Constants for a
/// budget test have to come from a measurement, or the test is decoration.
const BASE_OVERHEAD_BYTES: u64 = 256 * 1024 * 1024;

/// Allowance per file scanned — the slope, and the half of this bound that does the work.
///
/// Measured at ~11.4 KiB per file (see above). A scan is *entitled* to grow with the corpus: the
/// drive outcome keeps a path and a content hash per file, the file map grows, and fjall's index
/// and the allocator's retained arenas grow with it. What it is not entitled to do is keep a
/// `FileResult`, a decoded outline, or a chunk set per file alive across a phase boundary, which is
/// what issue #62 was.
///
/// 20 KiB is that slope with ~1.75x headroom.
///
/// What this bound does and does not catch, measured rather than assumed. Reverting the drive loop
/// to a single whole-corpus chunk — exactly the pre-release behaviour — was measured at both arms
/// on a 4 000-file corpus: 146 MiB chunked against 184 MiB unchunked, so ~38 MiB, or ~10 KiB per
/// file. This test stayed green through that, at 55% of budget.
///
/// That is a fact about the drive loop rather than a hole in the test. `scanner_code` clears a
/// file's BM25 postings inside the worker (`batch.bm25 = Vec::new()`) before the `FileResult`
/// escapes, so what the loop holds per file is a path, a status and two hashes. The chunking is a
/// real win and worth having at monorepo scale, but it is not what made issue #62 a 43.8 GiB
/// number, and this test is not its guard — `scanner_drive`'s own `on_batch` bound is.
///
/// What this test does guard is the gross end-to-end shape: any structure that starts scaling with
/// the corpus and *stays* scaled — a retained outline, a chunk set, an unbounded staging buffer —
/// shows up here as slope, which is the failure issue #62 actually was.
const PER_FILE_BYTES: u64 = 20 * 1024;

/// Write `FILES` synthetic Rust files with enough real structure that the scanner does actual work
/// on each — symbols to extract, calls to record, imports to resolve.
fn build_corpus(root: &Path) {
    // The root guard admits a directory carrying `basemind.toml`, which is far cheaper than
    // shelling out to `git init` and does not depend on git being installed. ~keep
    std::fs::write(root.join("basemind.toml"), "[scan]\n").expect("write basemind.toml");
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("create src");

    for file in 0..FILES {
        let mut body = String::with_capacity(LINES_PER_FILE * 64);
        body.push_str("use std::collections::HashMap;\n\n");
        for item in 0..LINES_PER_FILE {
            body.push_str(&format!(
                "pub fn item_{file}_{item}(input: &str) -> HashMap<String, usize> {{\n    \
                 let mut out = HashMap::new();\n    \
                 out.insert(input.to_string(), {item});\n    \
                 out\n}}\n\n"
            ));
        }
        std::fs::write(src.join(format!("module_{file:05}.rs")), body).expect("write module");
    }
}

/// A config with every optional lane off, so the measurement is the core scan and nothing else.
///
/// `EmbedMode::Inline` at the call sites is not a contradiction: the mode says *when* vectors would
/// be written, and `code_search.embed = false` says none are produced at all, so no ONNX model is
/// ever loaded. There is no `Skip` mode to reach for.
fn lean_config() -> Config {
    let mut config = Config::with_defaults();
    config.code_search.embed = false;
    config.resources.scan_threads = 4;
    config
}

/// Pay every one-time cost before the baseline is taken.
///
/// `ru_maxrss` never falls, so anything allocated for the first time *after* the baseline is
/// charged to the scan whether or not the scan is responsible: the rayon pool's worker stacks, the
/// tree-sitter grammar for Rust, fjall's initial mapping. Scanning a tiny throwaway corpus first
/// forces all of it, so what the assertion measures afterwards is the marginal cost of corpus size
/// — which is the only thing it is entitled to claim.
fn warm_up() {
    let warm = tempfile::tempdir().expect("tempdir");
    let root = warm.path();
    std::fs::write(root.join("basemind.toml"), "[scan]\n").expect("write basemind.toml");
    std::fs::create_dir_all(root.join("src")).expect("create src");
    std::fs::write(root.join("src/warm.rs"), "pub fn warm() -> usize { 1 }\n").expect("write warm file");

    let config = lean_config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open warm store");
    scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("warm scan");
}

fn peak_bytes() -> u64 {
    basemind::sysres::sample()
        .peak_bytes
        .expect("this platform must report a peak RSS for the budget to mean anything")
}

#[test]
fn a_full_scan_stays_inside_an_affine_memory_budget() {
    let data_home = tempfile::tempdir().expect("data home");
    // SAFETY: this binary contains exactly one test, so no other thread is reading the environment.
    unsafe { std::env::set_var("BASEMIND_DATA_HOME", data_home.path()) };

    warm_up();
    let baseline = peak_bytes();

    let corpus = tempfile::tempdir().expect("corpus");
    let root = corpus.path();
    build_corpus(root);

    let config = lean_config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");
    let report = scan(root, &mut store, &config, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan");

    assert!(
        report.stats.scanned >= FILES,
        "the corpus must actually have been scanned, or the budget is measuring nothing \
         (scanned {}, expected at least {FILES})",
        report.stats.scanned,
    );

    let peak = peak_bytes();
    let delta = peak.saturating_sub(baseline);
    let budget = BASE_OVERHEAD_BYTES + PER_FILE_BYTES * (FILES as u64);

    eprintln!(
        "scan_memory_budget: {FILES} files, peak delta {} MiB, budget {} MiB ({}% used)",
        delta / (1 << 20),
        budget / (1 << 20),
        delta * 100 / budget.max(1),
    );

    assert!(
        delta <= budget,
        "a {FILES}-file scan grew peak RSS by {delta_mb} MiB, over its {budget_mb} MiB budget \
         ({base_mb} MiB fixed + {per_file} KiB x {FILES} files). This is the shape of issue #62: \
         some structure sized by the corpus is being held across a phase boundary instead of being \
         dropped at a chunk edge.",
        delta_mb = delta / (1 << 20),
        budget_mb = budget / (1 << 20),
        base_mb = BASE_OVERHEAD_BYTES / (1 << 20),
        per_file = PER_FILE_BYTES / 1024,
    );
}
