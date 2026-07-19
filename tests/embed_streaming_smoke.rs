//! Regression coverage for the Inline embed pass's **streaming** LanceDB write (task #41).
//!
//! The bug: `scanner::scan` accumulated EVERY file's embedding rows (a 768-dim `Vec<f32>` plus
//! chunk text per chunk) for the whole corpus in RAM, then wrote once. On a large repo that held
//! multiple GB resident at once, and on the long-lived daemon it never released — a 13 GB
//! phys_footprint that thrashed a 16 GB machine.
//!
//! The fix streams the write per file: the deferred descriptors (`PendingDocBatch` /
//! `PendingCodeBatch`) carry only metadata (path + blob hash + counts), and the post-barrier flush
//! re-reads each file's already-persisted `.doc.msgpack` / `.chunk.msgpack` blob to rebuild its rows
//! one file at a time. Peak embed RAM is one file's rows, independent of corpus size.
//!
//! Two guards live here:
//! 1. [`embed_results_land_and_rescan_is_a_near_noop`] — always on, no embedder required: an Inline
//!    scan indexes code + documents, and a second Inline pass over identical content is a near-noop
//!    (the content-hash-keyed `Unchanged` gate skips every file, so nothing is re-embedded or
//!    re-written). When the embedder IS available, it further asserts the persisted blobs carry the
//!    vectors the flush streams — i.e. embed results still land correctly.
//! 2. [`embed_pass_peak_footprint_does_not_ratchet`] — `#[ignore]`d + macOS-only: the actual
//!    phys_footprint ratchet harness over a few thousand files across five Inline passes.
//!
//! The struct-shape guarantee (descriptors are metadata-only, never embedding rows) is pinned by the
//! in-crate unit tests `pending_code_batch_is_metadata_only` / `pending_doc_batch_is_metadata_only`.
#![cfg(all(feature = "documents", feature = "code-search"))]

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::{Store, VIEW_WORKING};

/// A small documented function + struct: the chunker emits at least one symbol chunk with real
/// lexical content, so the code-search tier has something to embed.
const CODE_BODY: &str = "/// Parse a configuration file's text into a typed Config value.\n\
pub fn parse_config(text: &str) -> Config {\n\
\x20   let _ = text;\n\
\x20   Config { name: String::new() }\n\
}\n\
\n\
pub struct Config {\n\
\x20   pub name: String,\n\
}\n";

/// Write `code_files` `.rs` files and `doc_files` `.svg` files under `root`, each with test-unique
/// content so their content-addressed sidecars never collide with a sibling test's. SVG is chosen
/// for the document tier because it is xberg-extractable yet is NOT a tree-sitter language, so it
/// routes to the document tier rather than the code tier (the same choice `scan_smoke` makes).
fn seed_repo(root: &std::path::Path, code_files: usize, doc_files: usize, marker: &str) {
    for i in 0..code_files {
        let body = format!("{CODE_BODY}\n// {marker}-code-{i}\n");
        std::fs::write(root.join(format!("code_{i}.rs")), body).expect("write code file");
    }
    for i in 0..doc_files {
        let body = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><text>configuration parser notes: the name \
             field is populated from a configuration file's text at load time {marker}-doc-{i}</text></svg>"
        );
        std::fs::write(root.join(format!("doc_{i}.svg")), body).expect("write doc file");
    }
}

/// Config with both embed tiers on. Documents embed by default; code-search embed is opt-in.
fn embed_config() -> ConfigV1 {
    let mut cfg = ConfigV1::with_defaults();
    cfg.code_search.embed = true;
    cfg.documents.embed = true;
    cfg
}

/// True once any code file in `root`'s scan produced a chunk sidecar carrying vectors — i.e. the
/// embedder was actually available. When false, the model was offline and vector-specific
/// assertions must be skipped (the streaming/idempotence assertions still hold).
fn embedder_produced_vectors(store: &Store, code_files: usize) -> bool {
    for i in 0..code_files {
        let path = format!("code_{i}.rs");
        if let Some(entry) = store.lookup(&path)
            && let Ok(Some(blob)) = store.read_chunks_by_hex(&entry.hash_hex)
            && blob.embedding_dim > 0
        {
            return true;
        }
    }
    false
}

#[test]
fn embed_results_land_and_rescan_is_a_near_noop() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let code_files = 12usize;
    let doc_files = 8usize;
    seed_repo(root, code_files, doc_files, "streaming-smoke");

    let cfg = embed_config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");

    // Pass 1 — Inline: indexes + embeds every file.
    let p1 = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("inline scan 1");
    assert!(
        p1.stats.updated >= code_files,
        "pass 1 must index every source file: updated={} code_files={code_files}",
        p1.stats.updated
    );
    assert!(
        p1.stats.docs_indexed >= doc_files,
        "pass 1 must index every document: docs_indexed={} doc_files={doc_files}",
        p1.stats.docs_indexed
    );

    // Pass 2 — Inline over identical content must be a near-noop: the content-hash-keyed Unchanged
    // gate skips every file, so nothing is re-embedded and nothing is rewritten to LanceDB. This is
    // the "a full Inline pass over an already-embedded corpus is a near-noop" guarantee — the
    // per-file marker (blob hash) that gates the rewrite, checked before a batch is ever produced.
    let p2 = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("inline scan 2");
    assert_eq!(
        p2.stats.updated, 0,
        "pass 2 must not re-index any unchanged source file"
    );
    assert_eq!(
        p2.stats.docs_indexed, 0,
        "pass 2 must not re-embed any unchanged document"
    );
    assert_eq!(
        p2.stats.skipped_unchanged,
        code_files + doc_files,
        "every code + doc file is skipped as unchanged on the second Inline pass"
    );

    // Correctness: when the embedder is available, the persisted blobs the flush streams from must
    // actually carry the vectors (dim > 0, one per chunk). Skips gracefully offline.
    if embedder_produced_vectors(&store, code_files) {
        let code_entry = store.lookup("code_0.rs").expect("code file indexed");
        let code_blob = store
            .read_chunks_by_hex(&code_entry.hash_hex)
            .expect("read chunk sidecar")
            .expect("chunk sidecar present");
        assert!(code_blob.embedding_dim > 0, "code chunks embedded");
        assert_eq!(
            code_blob.embeddings.len(),
            code_blob.chunks.len(),
            "every code chunk carries exactly one vector (the flush's source of truth)"
        );

        let doc_entry = store.lookup_doc("doc_0.svg").expect("doc file indexed");
        let doc_blob = store
            .read_doc_by_hex(&doc_entry.hash_hex)
            .expect("read doc blob")
            .expect("doc blob present");
        assert!(
            doc_blob.embedding_dim > 0 && !doc_blob.chunks.is_empty(),
            "document embedded with at least one chunk vector"
        );
        assert!(
            doc_blob
                .chunks
                .iter()
                .all(|c| c.embedding.len() == doc_blob.embedding_dim as usize),
            "every doc chunk carries a full-width embedding"
        );
    } else {
        eprintln!("SKIP vector-landing assertions: embedder unavailable (offline / cold model)");
    }
}

/// The phys_footprint ratchet harness for the long-lived-daemon leak. `#[ignore]`d because it seeds
/// hundreds–thousands of files and runs repeated full Inline embed passes (heavy, and needs the
/// embedding model), and macOS-only because it reads `phys_footprint` via `vmmap` — RSS is not
/// acceptable here (macOS compresses idle pages, so RSS understates the resident cost the leak paid).
///
/// What it asserts: the process's **current** physical footprint does not climb across repeated
/// rescans — i.e. embed working sets are RELEASED between passes. That is the exact daemon symptom
/// the bug caused ("on the long-lived daemon it never released → 13 GB phys_footprint"), and unlike
/// an absolute ceiling it is machine-independent (per the harness-canary "no absolute thresholds"
/// rule). The absolute footprint is dominated by the loaded ONNX model + lance/datafusion runtime
/// (~2 GB in this process) and is deliberately NOT asserted. The definitive per-descriptor
/// shape guard (no embedding rows accumulated corpus-wide) is the in-crate structural unit test
/// `pending_code_batch_is_metadata_only` / `pending_doc_batch_is_metadata_only`.
///
/// `BM_FOOTPRINT_FILES` overrides the corpus size (default 600). Run with:
/// ```bash
/// arch -arm64 cargo test --features crawl,memory,comms,code-intel-js,code-search \
///   --test embed_streaming_smoke -- --ignored --nocapture
/// ```
#[test]
#[ignore = "heavy: seeds hundreds of files + repeated embed passes; needs the embedding model; macOS-only"]
#[cfg(target_os = "macos")]
fn embed_pass_footprint_does_not_ratchet_across_rescans() {
    basemind::store::init_isolated_cache();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let total: usize = std::env::var("BM_FOOTPRINT_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let code_files = total * 2 / 3;
    let doc_files = total - code_files;
    seed_repo(root, code_files, doc_files, "footprint-ratchet");

    let cfg = embed_config();
    let mut store = Store::open(root, VIEW_WORKING).expect("open store");

    // Warm/first pass: loads the embedding model and does the one real embed of the whole corpus.
    // With the streaming fix, peak here is baseline + one file's rows; pre-fix it was baseline +
    // every file's rows held at once. If the model never loads, there is nothing to measure — skip.
    let warm = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("warm inline scan");
    eprintln!(
        "warm pass: updated={} docs_indexed={} files={total}",
        warm.stats.updated, warm.stats.docs_indexed
    );
    if !embedder_produced_vectors(&store, code_files) {
        eprintln!("SKIP: embedder produced no vectors (offline / cold model) — nothing to ratchet");
        return;
    }

    // Repeated rescans over identical content. Each is a near-noop (every file Unchanged), and the
    // current footprint must not climb pass over pass — that upward creep is the daemon leak. Report
    // both current and peak footprint for evidence; assert only on the machine-independent creep.
    let mut currents = Vec::new();
    for pass in 0..5 {
        let report = scan(root, &mut store, &cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("inline rescan");
        let current = phys_footprint_mb().expect("read phys_footprint on macOS");
        let peak = phys_footprint_peak_mb().unwrap_or(0.0);
        eprintln!(
            "pass {pass}: updated={} skipped_unchanged={} current_phys_footprint={current:.1} MB peak={peak:.1} MB",
            report.stats.updated, report.stats.skipped_unchanged
        );
        assert_eq!(
            report.stats.skipped_unchanged,
            code_files + doc_files,
            "every file must be skipped as unchanged on rescan (near-noop re-pass)"
        );
        currents.push(current);
    }

    let first = currents[0];
    let last = currents[currents.len() - 1];
    // The pre-fix daemon grew unbounded (GBs) across rescans because embed rows were never released.
    // A generous 400 MB tolerance absorbs allocator/model-arena jitter while still failing hard on
    // the unbounded corpus-scale creep the leak produced.
    const CREEP_TOLERANCE_MB: f64 = 400.0;
    assert!(
        last - first < CREEP_TOLERANCE_MB,
        "current phys_footprint crept from {first:.1} MB to {last:.1} MB across 5 rescans \
         (> {CREEP_TOLERANCE_MB} MB) — embed working sets are not being released between passes"
    );
}

/// Current process physical footprint in MB, parsed from `vmmap -summary`. macOS-only; returns
/// `None` if `vmmap` is unavailable or its output can't be parsed.
#[cfg(target_os = "macos")]
fn phys_footprint_mb() -> Option<f64> {
    vmmap_field("Physical footprint:")
}

/// Peak physical footprint in MB (`Physical footprint (peak):`). macOS-only.
#[cfg(target_os = "macos")]
fn phys_footprint_peak_mb() -> Option<f64> {
    vmmap_field("Physical footprint (peak):")
}

/// Run `vmmap -summary <pid>` and parse the MB value on the line beginning with `label`.
#[cfg(target_os = "macos")]
fn vmmap_field(label: &str) -> Option<f64> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("/usr/bin/vmmap")
        .args(["-summary", &pid])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(label) {
            return parse_mem_mb(rest.trim());
        }
    }
    None
}

/// Parse a vmmap size token like `123.4M`, `1.2G`, or `456K` into MB.
#[cfg(target_os = "macos")]
fn parse_mem_mb(token: &str) -> Option<f64> {
    let token = token.split_whitespace().next()?;
    let (num, unit) = token.split_at(token.find(|c: char| c.is_ascii_alphabetic())?);
    let value: f64 = num.parse().ok()?;
    let mb = match unit {
        "K" => value / 1024.0,
        "M" => value,
        "G" => value * 1024.0,
        _ => return None,
    };
    Some(mb)
}
