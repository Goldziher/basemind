//! Regression pin for GH issue #44: the document re-embed loop.
//!
//! On v0.22.3 a Deferred serve-boot pass wrote doc blobs vectorless and recorded no `DocEntry`;
//! the later Inline pass re-extracted + re-embedded, but `Store::write_doc`'s schema-only skip
//! threw the embedded result away — so the blob stayed vectorless and every entry-less encounter
//! of the same content (renames, rewrites, fresh worktrees) re-ran extraction + embedding again.
//! These tests pin the observable contract with embeddings OFF (no ONNX in CI): extraction runs
//! once per unique content, churn is served from the cached blob (`reused_doc_extraction`), and
//! repeated churn cycles accumulate ZERO fresh extractions.
//!
//! Plain `#[test]` only — `scanner::scan` is synchronous and opening LanceDB inside a tokio
//! runtime panics ("runtime within a runtime").

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;
use std::process::Command;

use basemind::config::ConfigV1;
use basemind::scanner::{EmbedMode, ScanReport, ScanSource, scan, scan_paths};
use basemind::store::Store;
use tempfile::TempDir;

/// Seven small distinct docs. SVG because it is xberg-extractable yet NOT a tree-sitter language,
/// so it routes to the document tier (`.md`/`.txt`/`.csv` all have TSLP grammars and would land in
/// the code tier instead) — the same choice `scan_smoke` and `embed_streaming_smoke` make.
const DOC_NAMES: &[&str] = &[
    "alpha.svg",
    "beta.svg",
    "gamma.svg",
    "delta.svg",
    "epsilon.svg",
    "zeta.svg",
    "eta.svg",
];

/// A few KB of per-name content, salted with `salt` (the fixture root) so different tests in this
/// binary never collide in the process-shared content-addressed blob cache — a collision would let
/// one test's blob satisfy another test's "fresh" scan and skew the reuse counters.
fn doc_body(name: &str, salt: &str) -> String {
    let mut lines = String::with_capacity(4096);
    for line in 0..64 {
        lines.push_str(&format!(
            "<text>{name} line {line}: the quick brown fox jumps over the lazy dog while {salt} watches</text>\n"
        ));
    }
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\">\n{lines}</svg>\n")
}

fn fixture() -> (TempDir, ConfigV1) {
    basemind::store::init_isolated_cache();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let _ = Command::new("git").args(["init", "-q"]).current_dir(root).status();
    let salt = root.display().to_string();
    for name in DOC_NAMES {
        fs::write(root.join(name), doc_body(name, &salt)).expect("write fixture doc");
    }
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.enabled = true;
    // Embeddings off: no ONNX model needed, but extraction + blob caching + the reuse accounting
    // are the same code path the #44 loop rode.
    cfg.documents.embed = false;
    (dir, cfg)
}

fn full_scan(root: &Path, store: &mut Store, cfg: &ConfigV1) -> ScanReport {
    scan(root, store, cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan")
}

#[test]
fn fresh_scan_extracts_every_doc_once() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();

    let report = full_scan(root, &mut store, &cfg);
    assert_eq!(report.stats.docs_indexed, 7, "all 7 docs extracted on first scan");
    assert_eq!(
        report.stats.reused_doc_extraction, 0,
        "nothing to reuse on a fresh corpus"
    );
    for name in DOC_NAMES {
        assert!(store.lookup_doc(name).is_some(), "{name} tracked in doc_files");
    }
}

#[test]
fn idempotent_rescan_refires_zero_extractions() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();

    full_scan(root, &mut store, &cfg);
    let second = full_scan(root, &mut store, &cfg);
    assert_eq!(
        second.stats.docs_indexed, 0,
        "unchanged docs must short-circuit as Unchanged, not re-extract"
    );
    assert_eq!(second.stats.reused_doc_extraction, 0);
}

#[test]
fn rename_churn_reuses_cached_extraction() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);

    fs::rename(root.join("alpha.svg"), root.join("alpha-renamed.svg")).unwrap();
    // The watcher-batch shape: the debouncer reports both the vanished and the appeared path.
    let report = scan_paths(
        root,
        &mut store,
        &cfg,
        &[root.join("alpha.svg"), root.join("alpha-renamed.svg")],
        EmbedMode::Inline,
    )
    .unwrap();

    assert_eq!(report.stats.removed, 1, "old doc path purged");
    assert_eq!(report.stats.docs_indexed, 1, "new doc path indexed");
    assert_eq!(
        report.stats.reused_doc_extraction, 1,
        "rename must be served from the cached blob — zero fresh extraction (issue #44)"
    );
    assert!(store.lookup_doc("alpha.svg").is_none(), "old rel gone from doc_files");
    assert!(
        store.lookup_doc("alpha-renamed.svg").is_some(),
        "new rel tracked in doc_files"
    );
}

#[test]
fn mtime_only_subtree_churn_refires_nothing() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let salt = root.display().to_string();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);

    // Delete + rewrite identical bytes: fresh mtimes/inodes, identical content hashes.
    for name in DOC_NAMES {
        let abs = root.join(name);
        fs::remove_file(&abs).unwrap();
        fs::write(&abs, doc_body(name, &salt)).unwrap();
    }
    let paths: Vec<_> = DOC_NAMES.iter().map(|n| root.join(n)).collect();
    let report = scan_paths(root, &mut store, &cfg, &paths, EmbedMode::Inline).unwrap();

    assert_eq!(
        report.stats.docs_indexed, 0,
        "identical content must short-circuit via the hash fast path"
    );
    assert_eq!(report.stats.removed, 0);
}

/// On v0.22.3 every cycle of this churn cost fresh extract (+ embed, when configured) passes over
/// the same bytes — the #44 loop. Total fresh extractions across all cycles must be zero.
#[test]
fn repeated_churn_cycles_accumulate_zero_fresh_extractions() {
    let (dir, cfg) = fixture();
    let root = dir.path();
    let salt = root.display().to_string();
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).unwrap();
    full_scan(root, &mut store, &cfg);

    let mut total_indexed = 0usize;
    let mut total_reused = 0usize;
    let mut alpha_current = "alpha.svg".to_string();
    for cycle in 0..3 {
        // Rename churn: bounce the alpha doc between two names.
        let alpha_next = if cycle % 2 == 0 {
            "alpha-renamed.svg"
        } else {
            "alpha.svg"
        };
        fs::rename(root.join(&alpha_current), root.join(alpha_next)).unwrap();
        let report = scan_paths(
            root,
            &mut store,
            &cfg,
            &[root.join(&alpha_current), root.join(alpha_next)],
            EmbedMode::Inline,
        )
        .unwrap();
        total_indexed += report.stats.docs_indexed;
        total_reused += report.stats.reused_doc_extraction;
        alpha_current = alpha_next.to_string();

        // Rewrite churn: delete + rewrite identical bytes for every doc.
        for name in DOC_NAMES.iter().filter(|n| **n != "alpha.svg") {
            let abs = root.join(name);
            fs::remove_file(&abs).unwrap();
            fs::write(&abs, doc_body(name, &salt)).unwrap();
        }
        let paths: Vec<_> = DOC_NAMES
            .iter()
            .filter(|n| **n != "alpha.svg")
            .map(|n| root.join(n))
            .collect();
        let report = scan_paths(root, &mut store, &cfg, &paths, EmbedMode::Inline).unwrap();
        total_indexed += report.stats.docs_indexed;
        total_reused += report.stats.reused_doc_extraction;
    }

    assert_eq!(
        total_indexed - total_reused,
        0,
        "churn cycles must accumulate ZERO fresh doc extractions (issue #44 loop)"
    );
    assert!(total_indexed >= 3, "each rename cycle re-indexes the bounced doc");
}
