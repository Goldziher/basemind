use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A pid no live process holds, as `scan_evidence`'s tests use for a killed scan.
const DEAD_PID: u32 = 0x7FFF_FFFE;

fn dead_writer(cache_dir: &Path, total: usize, done: usize, started_unix_ms: Option<u64>) {
    let record = EmbedRecord {
        total,
        done,
        started_unix_ms,
        pid: DEAD_PID,
        process_start: None,
        boot: None,
    };
    write(cache_dir, &record);
}

#[test]
fn no_record_reports_nothing_and_owes_no_resume() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(status(dir.path(), 1_000), None);
    assert!(!resume_due(dir.path()));
}

#[test]
fn owed_vectors_are_reported_with_no_eta_until_a_pass_runs() {
    let dir = tempfile::tempdir().unwrap();
    record_owed(dir.path(), 40);
    assert_eq!(
        status(dir.path(), 1_000),
        Some(EmbeddingsStatus {
            done: 0,
            total: 40,
            eta_secs: None
        })
    );
    assert!(
        !resume_due(dir.path()),
        "this process wrote it and is alive: not interrupted"
    );
}

#[test]
fn a_record_whose_writer_is_gone_is_an_interrupted_pass_to_resume() {
    let dir = tempfile::tempdir().unwrap();
    dead_writer(dir.path(), 100, 25, Some(1_000));
    assert!(resume_due(dir.path()));
    assert_eq!(
        status(dir.path(), 61_000),
        Some(EmbeddingsStatus {
            done: 25,
            total: 100,
            eta_secs: None
        }),
        "no pass runs, so there is no eta"
    );
}

#[test]
fn a_live_pass_reports_an_eta_from_its_own_rate() {
    let dir = tempfile::tempdir().unwrap();
    let embedded = AtomicUsize::new(0);
    let reporter = EmbedReporter::begin(dir.path().to_path_buf(), 100, &embedded);
    embedded.store(25, Ordering::Relaxed);
    reporter.report();
    let record = read(dir.path()).unwrap();
    assert_eq!((record.total, record.done), (100, 25));
    let started = record.started_unix_ms.unwrap();
    // 25 documents in 50 s leave 75 more at 2 s each.
    assert_eq!(
        status(dir.path(), started + 50_000),
        Some(EmbeddingsStatus {
            done: 25,
            total: 100,
            eta_secs: Some(150)
        })
    );
}

#[test]
fn a_pass_that_embeds_more_than_it_counted_owed_grows_its_total() {
    let dir = tempfile::tempdir().unwrap();
    let embedded = AtomicUsize::new(0);
    let reporter = EmbedReporter::begin(dir.path().to_path_buf(), 2, &embedded);
    embedded.store(5, Ordering::Relaxed);
    reporter.report();
    let status = status(dir.path(), now_unix_ms()).unwrap();
    assert_eq!((status.done, status.total), (5, 5));
}

#[test]
fn clearing_the_record_ends_the_report_and_the_resume() {
    let dir = tempfile::tempdir().unwrap();
    dead_writer(dir.path(), 10, 3, Some(1));
    clear(dir.path());
    assert_eq!(status(dir.path(), 1_000), None);
    assert!(!resume_due(dir.path()));
}

#[test]
fn an_empty_record_reports_nothing() {
    let dir = tempfile::tempdir().unwrap();
    record_owed(dir.path(), 0);
    assert_eq!(status(dir.path(), 1_000), None);
}

#[test]
fn an_unreadable_record_is_no_record() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(progress_path(dir.path()), b"{not json").unwrap();
    assert_eq!(read(dir.path()), None);
    assert!(!resume_due(dir.path()));
}

#[test]
fn eta_from_rate_rounds_up_and_needs_a_rate() {
    assert_eq!(eta_from_rate(10_000, 0, 10), None, "nothing done: no rate");
    assert_eq!(eta_from_rate(0, 5, 10), None, "no time elapsed: no rate");
    assert_eq!(eta_from_rate(10_000, 10, 10), None, "nothing left");
    assert_eq!(
        eta_from_rate(10_000, 3, 10),
        Some(24),
        "7 left at 3.33 s each, rounded up"
    );
}

#[test]
fn the_notice_names_the_counts_and_the_keyword_fallback() {
    let status = EmbeddingsStatus {
        done: 3,
        total: 9,
        eta_secs: Some(12),
    };
    let notice = status.notice();
    assert!(notice.contains("3 of 9 documents embedded"), "{notice}");
    assert!(notice.contains("matched by keyword only"), "{notice}");
}
