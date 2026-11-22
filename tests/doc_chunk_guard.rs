//! Regression tests for the document-extraction stall: xberg's markdown splitter is quadratic in the
//! document size (it re-collects and re-sorts every remaining element per chunk), so a multi-megabyte
//! text file pinned a scan worker for tens of minutes. Large text-like files now take a linear chunker,
//! and a wall-clock budget abandons any extraction that still overruns. The budget tests stall their
//! document with `test_stall`, so it overruns by construction rather than by the splitter being slow.

#![cfg(feature = "documents")]

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use basemind::config::ConfigV1;
use basemind::extract::ExtractError;
use basemind::extract::doc::{DocConfig, extract_doc};
use basemind::extract::doc_guard::{ChunkCutovers, abandoned_extractions, test_stall};
use basemind::scanner::{EmbedMode, ScanSource, scan};
use basemind::store::Store;

static SERIAL: Mutex<()> = Mutex::new(());

/// Run one test at a time, starting from an idle process: these tests each push megabytes through a
/// chunker, and share the process-wide stall list and abandoned-thread ledger. Each test releases
/// its neighbours' stalled extractions and waits for their threads to finish first.
fn exclusive() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    test_stall::release();
    let started = Instant::now();
    while abandoned_extractions() > 0 {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "released extractions never finished"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    guard
}

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
    let _serial = exclusive();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("syslog.txt");
    let body = pathological_log(5 * 1024 * 1024);
    fs::write(&path, &body).unwrap();

    let started = Instant::now();
    let doc = extract_doc(&path, Some("text/plain"), &offline_doc_config()).expect("extract");
    let elapsed = started.elapsed();

    assert!(doc.linear_chunked, "a 5 MB text file must bypass the markdown splitter");
    assert!(
        elapsed < Duration::from_secs(20),
        "took {elapsed:?}; the quadratic splitter needed ~100 s"
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
    let _serial = exclusive();
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
    let _serial = exclusive();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stalled.md");
    fs::write(&path, "# Stalled\n\nNever extracted.\n").unwrap();
    let _stalled = test_stall::stall(&["stalled.md"]);
    let config = DocConfig {
        extraction_timeout_secs: 1,
        ..offline_doc_config()
    };

    let started = Instant::now();
    let error = extract_doc(Path::new(&path), Some("text/markdown"), &config).expect_err("must time out");

    assert!(matches!(error, ExtractError::DocTimeout(_)), "got {error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the caller must be freed at the budget"
    );
}

fn scan_once(root: &Path, cfg: &ConfigV1) -> basemind::scanner::ScanReport {
    let mut store = Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
    scan(
        root,
        &mut store,
        cfg,
        ScanSource::WorkingTree,
        EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan")
}

fn offline_scan_config() -> ConfigV1 {
    basemind::store::init_isolated_cache();
    let mut cfg = ConfigV1::with_defaults();
    cfg.documents.embed = false;
    cfg
}

#[test]
fn scan_with_a_pathological_file_completes_and_counts_it_as_degraded() {
    let _serial = exclusive();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("syslog.txt"), pathological_log(5 * 1024 * 1024)).unwrap();
    fs::write(dir.path().join("guide.md"), markdown_doc()).unwrap();

    let started = Instant::now();
    let report = scan_once(dir.path(), &offline_scan_config());

    // The stall this guards took tens of minutes; the bound leaves room for a runner running
    // thousands of tests at once.
    assert!(
        started.elapsed() < Duration::from_secs(120),
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
fn structured_data_is_linear_but_not_counted_as_degraded() {
    let _serial = exclusive();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("cfg.json"), r#"{"name": "demo", "items": [1, 2, 3]}"#).unwrap();
    fs::write(dir.path().join("cfg.yaml"), "name: demo\nitems:\n  - alpha\n").unwrap();

    let report = scan_once(dir.path(), &offline_scan_config());

    assert_eq!(report.stats.docs_indexed, 2);
    assert_eq!(report.stats.docs_degraded, 0, "structured-by-type is not degraded");
    assert_eq!(report.stats.docs_text_fallback, 0);
}

#[test]
fn scan_counts_a_timed_out_document_and_still_indexes_the_rest() {
    let _serial = exclusive();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("stalled.md"), "# Stalled\n\nNever extracted.\n").unwrap();
    fs::write(dir.path().join("guide.md"), markdown_doc()).unwrap();
    let _stalled = test_stall::stall(&["stalled.md"]);
    let mut cfg = offline_scan_config();
    cfg.documents.extraction_timeout_secs = 1;

    let report = scan_once(dir.path(), &cfg);

    assert_eq!(report.stats.doc_timeouts, 1);
    assert_eq!(
        report.stats.extract_failed, 1,
        "a timeout is also an extraction failure"
    );
    assert_eq!(report.stats.docs_indexed, 1, "the normal document is unaffected");
    let store = Store::open(dir.path(), basemind::store::VIEW_WORKING).unwrap();
    assert!(store.lookup_doc("stalled.md").is_none());
    assert!(store.lookup_doc("guide.md").is_some());
}

/// The lines of `source` a chunk cites, end to end.
fn cited<'a>(source: &'a str, chunk: &basemind::extract::doc::DocChunk) -> String {
    let (Some(first), Some(last)) = (chunk.line_start, chunk.line_end) else {
        panic!("chunk {:?} cites no lines", chunk.text);
    };
    let lines: Vec<&'a str> = source.split_inclusive('\n').collect();
    lines[first as usize - 1..last as usize].concat()
}

/// Text and data files are chunked from the file itself: xberg reflows them (a log into one line, a
/// JSON object into `key: value` paragraphs), and a chunk of that text can not be found by line.
#[test]
fn every_chunk_of_a_text_or_data_file_cites_its_lines() {
    let _serial = exclusive();
    let dir = tempfile::tempdir().unwrap();
    let log = pathological_log(64 * 1024);
    let prose =
        "A paragraph of prose that is wrapped\nat a fixed width, the way people\nwrite text files.\n\n".repeat(60);
    let json = format!(
        "{{\n{}\n}}\n",
        (0..80)
            .map(|i| format!("  \"service{i}\": {{\n    \"port\": {i},\n    \"tags\": [\"a\", \"b\"]\n  }}"))
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let yaml: String = (0..80)
        .map(|i| format!("service{i}:\n  port: {i}\n  tags:\n    - a\n"))
        .collect();
    let toml: String = (0..80)
        .map(|i| format!("[service{i}]\nport = {i}\ntags = [\"a\"]\n\n"))
        .collect();
    let csv: String = std::iter::once("id,name,port\n".to_string())
        .chain((0..300).map(|i| format!("{i},service {i},{}\n", 8000 + i)))
        .collect();
    let small_cutover = DocConfig {
        chunk_cutovers: ChunkCutovers {
            markdown_bytes: 1024,
            plain_text_bytes: 1024,
        },
        ..offline_doc_config()
    };
    for (name, mime, body, config) in [
        ("app.log", "text/plain", &log, offline_doc_config()),
        ("big.log", "text/plain", &log, small_cutover),
        ("notes.txt", "text/plain", &prose, offline_doc_config()),
        ("cfg.json", "application/json", &json, offline_doc_config()),
        ("cfg.yaml", "application/yaml", &yaml, offline_doc_config()),
        ("Cargo.toml", "application/toml", &toml, offline_doc_config()),
        ("data.csv", "text/csv", &csv, offline_doc_config()),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, body).unwrap();
        let doc = extract_doc(&path, Some(mime), &config).expect(name);
        assert!(doc.chunks.len() > 1, "{name} must chunk");
        assert_eq!(&doc.content, body, "{name}: the chunks are cut from the file");
        for chunk in &doc.chunks {
            assert_eq!(
                chunk.text,
                body[chunk.byte_start as usize..chunk.byte_end as usize],
                "{name}"
            );
            assert!(
                cited(body, chunk).contains(chunk.text.trim()),
                "{name}: {:?}",
                chunk.text
            );
        }
        assert_eq!(doc.chunks[0].line_start, Some(1), "{name}");
        assert_eq!(
            doc.chunks.last().unwrap().line_end,
            Some(u32::try_from(body.lines().count()).unwrap()),
            "{name}: the last line is cited"
        );
    }
}
