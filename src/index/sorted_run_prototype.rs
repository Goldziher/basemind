use std::hint::black_box;
use std::io;
use std::path::Path;
use std::time::Instant;

use tempfile::tempdir;

use super::sorted_run::{SortedRun, SortedRunView, write_run};

fn open_run(path: &Path) -> io::Result<SortedRun> {
    // SAFETY: All mappings use private fixture files, never changed while mapped.
    unsafe { SortedRun::open(path) }
}

use super::IndexDb;
use super::view::{IndexView, Part, StandaloneView};

#[test]
fn sorted_run_reads_binary_keys_and_empty_values() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run");
    let rows: &[(&[u8], &[u8])] = &[(b"a", b""), (b"a\0", b"value"), (b"\xff", b"last")];
    write_run(&path, rows.iter().copied().map(Ok)).unwrap();
    let first = open_run(&path).unwrap();
    let second = open_run(&path).unwrap();
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(first.row(index).unwrap(), *row);
        assert_eq!(second.lower_bound(row.0).unwrap(), index);
    }
    use std::ops::Bound::{Excluded, Included, Unbounded};
    assert_eq!(first.get(b"a").unwrap(), Some(&b""[..]));
    assert_eq!(first.get(b"missing").unwrap(), None);
    assert_eq!(first.prefix(b"a").unwrap(), 0..2);
    assert_eq!(first.prefix(b"\xff").unwrap(), 2..3);
    assert_eq!(first.range(Excluded(b"a"), Included(b"\xff")).unwrap(), 1..3);
    assert_eq!(first.range(Unbounded, Excluded(b"a")).unwrap(), 0..0);
    assert_eq!(first.lower_bound(b"b").unwrap(), 2);
    assert_eq!(first.lower_bound(b"\xff\xff").unwrap(), rows.len());
    assert!(first.row(rows.len()).is_err());
}

#[test]
fn sorted_run_handles_empty_and_rejects_truncated_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run");
    write_run(&path, std::iter::empty::<io::Result<(&[u8], &[u8])>>()).unwrap();
    {
        let run = open_run(&path).unwrap();
        assert_eq!(run.lower_bound(b"key").unwrap(), 0);
    }
    let unordered = dir.path().join("unordered");
    assert!(write_run(&unordered, [(b"b", b""), (b"a", b"")].into_iter().map(Ok)).is_err());
    std::fs::write(&path, b"BMSRUN01").unwrap();
    assert!(open_run(&path).is_err());
}

#[test]
fn sorted_runs_match_a_scanned_index() {
    let source = tempdir().unwrap();
    let store = super::view_tests::scanned_store(source.path());
    let db = store.index_db.as_ref().unwrap();
    assert!(compare_parts(&db.view()) > 0);
}

#[test]
fn sorted_run_view_matches_fjall_point_prefix_and_range_reads() {
    use std::ops::Bound::{Excluded, Included, Unbounded};

    let source = tempdir().unwrap();
    let store = super::view_tests::scanned_store(source.path());
    let db = store.index_db.as_ref().unwrap();
    let reference = db.view();
    let output = tempdir().unwrap();
    SortedRunView::export(output.path(), &reference).unwrap();
    // SAFETY: Fixture files are closed and remain unchanged until all returned bytes drop.
    let mapped = unsafe { SortedRunView::open(output.path()) }.unwrap();
    let mut total = 0;
    for part in Part::ALL {
        let expected = reference.iter(part).collect::<Result<Vec<_>, _>>().unwrap();
        let actual = mapped.iter(part).collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected);
        let Some((key, value)) = expected.first() else {
            continue;
        };
        total += expected.len();
        assert_eq!(mapped.get(part, key).unwrap().as_ref(), Some(value));
        let prefix = &key[..key.len().min(3)];
        assert_eq!(
            mapped.prefix(part, prefix).collect::<Result<Vec<_>, _>>().unwrap(),
            reference.prefix(part, prefix).collect::<Result<Vec<_>, _>>().unwrap(),
        );
        for lower in [Included(key.as_ref()), Excluded(key.as_ref())] {
            assert_eq!(
                mapped
                    .range(part, lower, Unbounded)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                reference
                    .range(part, lower, Unbounded)
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            );
        }
    }
    assert!(total > 0);
}

#[test]
#[ignore = "requires a disposable, complete Fjall view from a reference scan"]
fn compare_sorted_run_with_fjall() {
    let path = std::env::var_os("BASEMIND_PROTOTYPE_VIEW").expect("set BASEMIND_PROTOTYPE_VIEW");
    let path = Path::new(&path);
    assert!(path.join("index.fjall").is_dir(), "requires an existing Fjall index");
    let started = Instant::now();
    let db = IndexDb::open(path).unwrap();
    eprintln!("Fjall open: {:?}", started.elapsed());
    assert!(compare_parts(&StandaloneView(&db)) > 0);
}

fn compare_parts(view: &impl IndexView) -> usize {
    let output = tempdir().unwrap();
    let mut total = 0;
    for part in Part::ALL {
        let path = output.path().join(part.name());
        let started = Instant::now();
        let rows = view.iter(part).map(|row| row.map_err(io::Error::other));
        write_run(&path, rows).unwrap();
        let exported = started.elapsed();
        let started = Instant::now();
        let run = open_run(&path).unwrap();
        let mapped = started.elapsed();
        let mut count = 0;
        for row in view.iter(part) {
            let (key, value) = row.unwrap();
            assert_eq!(run.row(count).unwrap(), (key.as_ref(), value.as_ref()));
            assert_eq!(run.lower_bound(&key).unwrap(), count);
            count += 1;
        }
        assert_eq!(run.row_count(), count);
        total += count;
        if matches!(part, Part::RefsByDef | Part::CallsByCallee) && count > 0 {
            compare_prefix_latency(view, part, &run);
        }
        eprintln!(
            "{}: {} rows, {} bytes, export {:?}, map {:?}",
            part.name(),
            count,
            run.mapped_bytes(),
            exported,
            mapped
        );
    }
    total
}

fn compare_prefix_latency(view: &impl IndexView, part: Part, run: &SortedRun) {
    const SAMPLE_COUNT: usize = 1_000;
    let mut fjall_times = Vec::new();
    let mut mmap_times = Vec::new();
    for index in (0..run.row_count()).step_by((run.row_count() / SAMPLE_COUNT).max(1)) {
        let (key, _) = run.row(index).unwrap();
        let prefix = query_prefix(part, key);
        let started = Instant::now();
        let fjall_count = view.prefix(part, &prefix).fold(0, |count, row| {
            black_box(row.unwrap());
            count + 1
        });
        fjall_times.push(started.elapsed());
        let started = Instant::now();
        let mmap_count = run_prefix_count(run, &prefix);
        mmap_times.push(started.elapsed());
        assert!(fjall_count > 0);
        assert_eq!(fjall_count, mmap_count);
    }
    fjall_times.sort_unstable();
    mmap_times.sort_unstable();
    let percentile = fjall_times.len() * 95 / 100;
    eprintln!(
        "{}: {} prefix queries, p95 Fjall {:?}, mmap {:?}",
        part.name(),
        fjall_times.len(),
        fjall_times[percentile],
        mmap_times[percentile]
    );
}

fn query_prefix(part: Part, key: &[u8]) -> Vec<u8> {
    use super::keys;
    match part {
        Part::RefsByDef => {
            let (path, start, _, _) = keys::parse_ref_by_def(key).unwrap();
            keys::refs_by_def_prefix(&path, start)
        }
        Part::CallsByCallee => {
            let (name, _, _) = keys::parse_call_by_callee(key).unwrap();
            keys::calls_by_callee_prefix(&name)
        }
        _ => unreachable!("only the references and callers keyspaces are sampled"),
    }
}

fn run_prefix_count(run: &SortedRun, prefix: &[u8]) -> usize {
    let start = run.lower_bound(prefix).unwrap();
    let mut count = 0;
    for index in start..run.row_count() {
        let row = run.row(index).unwrap();
        if !row.0.starts_with(prefix) {
            break;
        }
        black_box(row);
        count += 1;
    }
    count
}
