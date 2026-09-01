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
/// stacks, tree-sitter's parser and language objects, fjall's memtables and block cache, the
/// chunker's arenas where it is compiled, and the allocator's own retained arenas.
///
/// ## Why this constant is feature-dependent
///
/// `code_search.enabled` defaults to `true`, so under `code-search` this scan also chunks every
/// file and stages BM25 postings — and staged postings riding outside the only bound that existed
/// are what the investigation identified as the bulk of issue #62's 43.8 GiB. Without the feature
/// that lane is not merely disabled, it is not compiled, so the same corpus exercises a materially
/// different pipeline. One set of constants across both axes therefore has to be either too loose
/// to bind on `full` or too tight to pass on default, and the first draft of these numbers was
/// calibrated only on the default axis — the one that does *not* contain the structure this test
/// exists to catch. Splitting them keeps both axes binding.
///
/// Calibrated, not guessed, on this tree:
///
/// | axis | 2 000 files | 3 000 | 4 000 | 8 000 | 9 000 |
/// |---|---|---|---|---|---|
/// | default | — | 158 MiB | 146 MiB | — | 223 MiB |
/// | `full` | 324 MiB | — | 503 MiB | 522 MiB | — |
///
/// The `full` row is close to flat across a 4x corpus range — the marginal cost from 4 000 to
/// 8 000 files is ~5 KiB per file — which is the result this test wants: chunking memory is
/// bounded, not accumulating. What that row also shows is roughly ±100 MiB of run-to-run variance
/// (the 4 000-file point sits well above the line through the other two), coming from allocator
/// arena retention and rayon scheduling rather than from the corpus. That variance belongs in the
/// intercept. Putting it in the slope instead would buy nothing here and would loosen the bound
/// exactly where a real leak shows up first — at large N.
const BASE_OVERHEAD_BYTES: u64 = if cfg!(feature = "code-search") {
    576 * 1024 * 1024
} else {
    256 * 1024 * 1024
};

/// Allowance per file scanned — the slope, and the half of this bound that does the work.
///
/// A scan is *entitled* to grow with the corpus: the drive outcome keeps a path and a content hash
/// per file, the file map grows, and fjall's index and the allocator's retained arenas grow with
/// it. What it is not entitled to do is keep a `FileResult`, a decoded outline, or a chunk set per
/// file alive across a phase boundary, which is what issue #62 was.
///
/// Measured at ~11.4 KiB per file on the default axis and ~5 KiB per file at the top of the `full`
/// range. Both constants below are that slope with headroom, deliberately kept tight: the slope is
/// what rejects superlinear growth, so it is the half of the bound that must not be inflated to
/// absorb noise.
///
/// Resulting utilisation at the calibration points — high enough to bind, with ~1.3x headroom on
/// the worst observed run:
///
/// | axis | 2 000 | 4 000 | 8 000 |
/// |---|---|---|---|
/// | `full` | 52% | 75% | 68% |
///
/// What this bound does and does not catch, measured rather than assumed. Reverting the drive loop
/// to a single whole-corpus chunk — exactly the pre-release behaviour — was measured at both arms
/// on a 4 000-file corpus on the default axis: 146 MiB chunked against 184 MiB unchunked, so
/// ~38 MiB, or ~10 KiB per file. This test stayed green through that, at 55% of budget.
///
/// That is a fact about the drive loop rather than a hole in the test. `scanner_code` clears a
/// file's BM25 postings inside the worker (`batch.bm25 = Vec::new()`) before the `FileResult`
/// escapes, so what the loop holds per file is a path, a status and two hashes. The chunking is a
/// real win and worth having at monorepo scale, but it is not what made issue #62 a 43.8 GiB
/// number, and this test is not its guard — `scanner_drive`'s own `on_batch` bound is.
///
/// The same measurement was made for the other half of the release, and came out the same way.
/// Removing *both* byte bounds — `INDEX_COMMIT_BATCH_BYTES` and `INDEX_STAGED_BYTES_CEILING` set
/// out of reach, leaving only the 256-file counter, which is exactly the pre-release shape in
/// which BM25 postings rode outside every bound — was measured on the 4 000-file corpus under
/// `full` at 479 MiB, against 503 MiB unmutated. The mutated arm used *less*: the difference is
/// inside this test's run-to-run noise, so at this corpus size the file counter alone is
/// sufficient and the byte ledger buys nothing measurable. Its value shows up at monorepo scale,
/// which no CI leg can afford to run.
///
/// That is worth stating plainly because it bounds what a green run here is evidence *of*. This
/// test is not the ledger's guard either — `scanner_index_batch`'s
/// `the_production_byte_budget_commits_before_the_file_counter` is, and that one was written
/// precisely because the mutation above left this file and every other test in the tree green.
///
/// What this test does guard is the gross end-to-end shape: any structure that starts scaling with
/// the corpus and *stays* scaled — a retained outline, a chunk set, an unbounded staging buffer —
/// shows up here as slope, which is the failure issue #62 actually was.
const PER_FILE_BYTES: u64 = if cfg!(feature = "code-search") {
    24 * 1024
} else {
    20 * 1024
};

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
