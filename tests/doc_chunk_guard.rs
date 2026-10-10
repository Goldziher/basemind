//! Regression tests for the document-extraction stall: xberg's markdown splitter is quadratic in the
//! document size (it re-collects and re-sorts every remaining element per chunk), so a multi-megabyte
//! text file pinned a scan worker for tens of minutes. Large text-like files now take a linear chunker,
//! and a wall-clock budget abandons any extraction that still overruns.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use basemind::config::ConfigV1;
use basemind::extract::ExtractError;
use basemind::extract::doc::{DocConfig, extract_doc};
use basemind::extract::doc_guard::ChunkCutovers;
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::Store;

/// A syslog-style file. At 3 MB the markdown splitter needs ~100 s in a debug build (measured: 0.9 s at
/// 256 KB, 3.3 s at 512 KB, 11.6 s at 1 MB, 40 s at 2 MB), so any run under a few seconds proves it
/// was never reached.
fn pathological_log(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 100);
    let mut i = 0u64;
    while text.len() < bytes {
        text.push_str(&format!(
            "2021-01-27 14:{:02}:{:02} host{} svc[{i}]: event id={} status=ok\n",
            i % 60,
            i % 59,
            i % 7,
            i * 31
        ));
        i += 1;
    }
    text
}

fn markdown_doc() -> String {
    (0..60)
        .map(|i| {
            format!("## Section {i}\n\nSome *text* with `code` and a [link](http://x/{i}).\n\n- item a\n- item b\n\n")
        })
        .collect()
}

fn offline_doc_config() -> DocConfig {
    DocConfig {
        embed: false,
        embedding_preset: None,
        ..DocConfig::default()
    }
}

#[test]
fn pathological_text_file_completes_quickly_via_linear_chunking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("syslog.txt");
    let body = pathological_log(3 * 1024 * 1024);
    fs::write(&path, &body).unwrap();

    let started = Instant::now();
    let doc = extract_doc(&path, Some("text/plain"), &offline_doc_config()).expect("extract");
    let elapsed = started.elapsed();

    assert!(doc.linear_chunked, "a 3 MB text file must bypass the markdown splitter");
    assert!(
        elapsed < Duration::from_secs(20),
        "took {elapsed:?}; the splitter alone needs ~100 s"
    );
    assert!(doc.chunks.len() > 1000);
    for chunk in &doc.chunks {
        assert!(chunk.text.chars().count() <= 1000);
        assert_eq!(
            chunk.text,
            doc.content[chunk.byte_start as usize..chunk.byte_end as usize]
        );
    }
    assert_eq!(
        doc.chunks.last().unwrap().byte_end as usize,
        doc.content.len(),
        "the tail is covered"
    );
}

#[test]
fn normal_documents_chunk_exactly_as_before() {
    let dir = tempfile::tempdir().unwrap();
    for (name, mime, body) in [
        ("guide.md", "text/markdown", markdown_doc()),
        ("notes.txt", "text/plain", "plain prose line.\n".repeat(400)),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, &body).unwrap();
        let guarded = extract_doc(&path, Some(mime), &offline_doc_config()).expect("guarded");
        let unguarded = extract_doc(
            &path,
            Some(mime),
            &DocConfig {
                chunk_cutovers: ChunkCutovers {
                    markdown_bytes: u64::MAX,
                    plain_text_bytes: u64::MAX,
                },
                ..offline_doc_config()
            },
        )
        .expect("unguarded");
        assert!(
            !guarded.linear_chunked,
            "{name} is under the cutover and must keep the splitter"
        );
        assert!(guarded.chunks.len() > 1, "{name} must actually chunk");
        assert_eq!(guarded.chunks, unguarded.chunks, "{name} chunks must be unchanged");
    }
}

#[test]
fn extraction_past_its_wall_clock_budget_is_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("syslog.txt");
    fs::write(&path, pathological_log(2 * 1024 * 1024)).unwrap();
    let config = DocConfig {
        extraction_timeout_secs: 1,
        // Force the quadratic splitter so the budget, not the linear route, is what is under test.
        chunk_cutovers: ChunkCutovers {
            markdown_bytes: u64::MAX,
            plain_text_bytes: u64::MAX,
        },
        ..offline_doc_config()
    };

    let started = Instant::now();
    let error = extract_doc(Path::new(&path), Some("text/plain"), &config).expect_err("must time out");

    assert!(matches!(error, ExtractError::DocTimeout(_)), "got {error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the caller must be freed at the budget"
    );
}

fn scan_once(root: &Path, cfg: &ConfigV1) -> basemind::scanner::ScanReport {
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    scan(root, &mut store, cfg, ScanSource::WorkingTree, EmbedMode::Inline).expect("scan")
}

fn offline_scan_config() -> ConfigV1 {
    basemind::store::init_isolated_cache();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg
}

#[test]
fn scan_with_a_pathological_file_completes_and_counts_it_as_degraded() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("syslog.txt"), pathological_log(3 * 1024 * 1024)).unwrap();
    fs::write(dir.path().join("guide.md"), markdown_doc()).unwrap();

    let started = Instant::now();
    let report = scan_once(dir.path(), &offline_scan_config());

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "scan took {:?}",
        started.elapsed()
    );
    assert_eq!(report.stats.docs_indexed, 2);
    assert_eq!(
        report.stats.docs_degraded, 1,
        "only the large log takes the linear chunker"
    );
    assert_eq!(report.stats.doc_timeouts, 0);
}

#[test]
fn scan_counts_a_timed_out_document_and_still_indexes_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("syslog.txt"), pathological_log(2 * 1024 * 1024)).unwrap();
    fs::write(dir.path().join("guide.md"), markdown_doc()).unwrap();
    let mut cfg = offline_scan_config();
    cfg.documents.extraction_timeout_secs = 1;
    cfg.documents.plain_text_chunk_max_bytes = u64::MAX;

    let report = scan_once(dir.path(), &cfg);

    assert_eq!(report.stats.doc_timeouts, 1);
    assert_eq!(
        report.stats.extract_failed, 1,
        "a timeout is also an extraction failure"
    );
    assert_eq!(report.stats.docs_indexed, 1, "the normal document is unaffected");
    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    assert!(store.lookup_doc("syslog.txt").is_none());
    assert!(store.lookup_doc("guide.md").is_some());
}
