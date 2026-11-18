//! Equivalence of the dictionary-backed name scans with the full-partition walk they replace.
//!
//! Every comparison runs the SAME scan twice on one database: once with the dictionary disabled
//! (the legacy walk, which is the reference) and once with it built, and requires identical pages.

use crate::extract::{Call, FileMapL1, FileMapL2, Implementation};
use crate::index::IndexDb;
use crate::path::RelPath;

use super::{scan_calls_by_name, scan_calls_examining};
use crate::mcp::helpers_impls::{scan_impls_examining, scan_impls_fjall};
use crate::mcp::helpers_refs_match::Matching;

/// Deterministic xorshift, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Names that overlap as prefixes, suffixes and infixes, differ in case, vary in length (so the
/// length-prefixed key order differs from plain byte order), and include non-ASCII.
fn vocabulary() -> Vec<String> {
    let mut names: Vec<String> = [
        "a",
        "ab",
        "abc",
        "run",
        "Run",
        "runner",
        "run_all",
        "rerun",
        "spawn",
        "spawn_blocking",
        "unspawn",
        "é",
        "caf\u{e9}",
        "日本語",
        "日本",
        "Display",
        "fmt",
        "x.fmt",
        "Foo::new",
        "new",
        "renew",
        "get",
        "get_or_insert",
        "forget",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    names.extend((0..120).map(|i| format!("fn_{i}")));
    names.extend((0..40).map(|i| format!("{}_helper", "x".repeat(i % 9 + 1))));
    names
}

fn call(callee: &str, start: u32) -> Call {
    Call {
        callee: callee.to_string(),
        start_byte: start,
        end_byte: start + 1,
        start_row: start / 10,
        start_col: start % 10,
    }
}

fn l1(impls: &[(String, String)]) -> FileMapL1 {
    FileMapL1 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        size_bytes: 0,
        had_errors: false,
        error_count: 0,
        symbols: Vec::new(),
        imports: Vec::new(),
        implementations: impls
            .iter()
            .enumerate()
            .map(|(i, (t, ty))| Implementation {
                trait_name: t.clone(),
                impl_type: ty.clone(),
                start_byte: i as u32 * 7,
                start_row: i as u32,
                start_col: 0,
            })
            .collect(),
        rationale: Vec::new(),
        extract_epoch: crate::extract::EXTRACT_EPOCH,
    }
}

fn l2(calls: Vec<Call>) -> FileMapL2 {
    FileMapL2 {
        schema_ver: crate::extract::SCHEMA_VER,
        language: "rust".to_string(),
        calls,
        docs: Vec::new(),
    }
}

fn path(i: usize) -> RelPath {
    let ext = ["rs", "py", "ts"][i % 3];
    RelPath::from(format!("src/d{}/f{i}.{ext}", i % 7).as_str())
}

fn lang_of(rel: &RelPath) -> &'static str {
    match rel.as_str().unwrap_or_default().rsplit('.').next() {
        Some("rs") => "rust",
        Some("py") => "python",
        _ => "typescript",
    }
}

/// The content key file `rel` is written under: one of its own, so every file has rows of its own.
fn ck(rel: &RelPath) -> crate::index::keys::ContentKey {
    crate::index::keys::content_key(&rel.to_str_lossy(), lang_of(rel), crate::extract::EXTRACT_EPOCH)
}

/// Write `rel`'s rows and add it to the view's live keys, as a scan and its file map write do.
fn upsert(db: &IndexDb, rel: &RelPath, l1: &FileMapL1, l2: Option<&FileMapL2>) {
    let mut writer = db.writer();
    writer.upsert_file(rel, &ck(rel), l1, l2).unwrap();
    writer.commit().unwrap();
    let live = db.live_keys();
    let entries = live.iter().map(|(path, key)| (path.clone(), *key));
    db.publish_live_keys(crate::index::live_keys::LiveKeys::from_entries(
        entries.chain([(rel.clone(), ck(rel))]),
    ));
}

/// Write file `i` with a random selection of calls and implementations. `hot` names are repeated
/// enough across the corpus to push past the `scan_cap` of a small page.
fn write_file(db: &IndexDb, rng: &mut Rng, vocab: &[String], i: usize) {
    let rel = path(i);
    let n = 1 + rng.below(40);
    let mut calls: Vec<Call> = (0..n)
        .map(|k| call(&vocab[rng.below(vocab.len())], (k * 11) as u32))
        .collect();
    if i.is_multiple_of(4) {
        // `run` and `fmt` appear in far more than 2000 keys overall.
        calls.extend((0..120).map(|k| call(["run", "fmt"][k % 2], 1_000 + (k * 13) as u32)));
    }
    let impls: Vec<(String, String)> = (0..rng.below(4))
        .map(|k| (vocab[rng.below(vocab.len())].clone(), format!("Ty{i}_{k}")))
        .collect();
    upsert(db, &rel, &l1(&impls), Some(&l2(calls)));
}

type CallPage = (u32, bool, Vec<Vec<u8>>, Vec<(String, u32, u32)>, bool);

fn call_page(db: &IndexDb, needle: &str, limit: usize, cursor: Option<&[u8]>) -> CallPage {
    let page = scan_calls_by_name(&db.view(), needle, Matching::Substring, limit, cursor).unwrap();
    let hits = page.hits.iter().map(|h| (h.callee.clone(), h.line, h.column)).collect();
    (
        page.total,
        page.total_is_partial,
        page.hit_keys,
        hits,
        page.next_cursor.is_some(),
    )
}

type ImplPage = (usize, bool, Vec<Vec<u8>>, bool);

fn impl_page(db: &IndexDb, needle: &str, language: Option<&str>, limit: usize, cursor: Option<&[u8]>) -> ImplPage {
    let page = scan_impls_fjall(
        &db.view(),
        needle,
        Matching::Substring,
        language,
        limit,
        cursor,
        |rel, lang| lang_of(rel) == lang,
        |_, _| (0, 0),
    )
    .unwrap();
    (page.total, page.total_is_partial, page.hit_keys, page.has_more)
}

const NEEDLES: &[&str] = &[
    "",
    "run",
    "Run",
    "fmt",
    "a",
    "é",
    "日",
    "日本",
    "spawn",
    "get",
    "_helper",
    "xx_",
    "fn_1",
    "fn_11",
    "Display",
    "new",
    "zzz-absent",
    "Foo::new",
    "x.f",
];

/// Page through every result of `needle` under both modes and compare each page.
fn assert_equivalent(db: &IndexDb, label: &str) {
    for needle in NEEDLES {
        for limit in [2usize, 50, 400] {
            let (mut want_cursor, mut got_cursor): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
            for step in 0..10 {
                db.callee_dict().set_disabled(true);
                let want = call_page(db, needle, limit, want_cursor.as_deref());
                db.callee_dict().set_disabled(false);
                let got = call_page(db, needle, limit, got_cursor.as_deref());
                assert_eq!(want, got, "{label}: calls needle={needle:?} limit={limit} step={step}");
                if !want.4 {
                    break;
                }
                // Cursors are the last key emitted, so both modes hold the same bytes.
                want_cursor = want.2.last().cloned();
                got_cursor = got.2.last().cloned();
            }
        }
        for language in [None, Some("rust"), Some("python")] {
            for limit in [2usize, 100] {
                let mut cursor: Option<Vec<u8>> = None;
                for step in 0..10 {
                    db.trait_dict().set_disabled(true);
                    let want = impl_page(db, needle, language, limit, cursor.as_deref());
                    db.trait_dict().set_disabled(false);
                    let got = impl_page(db, needle, language, limit, cursor.as_deref());
                    assert_eq!(
                        want, got,
                        "{label}: impls needle={needle:?} lang={language:?} limit={limit} step={step}"
                    );
                    if !want.3 {
                        break;
                    }
                    cursor = want.2.last().cloned();
                }
            }
        }
    }
}

fn build_dicts(db: &IndexDb) {
    // Without this the small test vocabulary routes most needles to the walk and proves nothing.
    db.callee_dict().always_use_dictionary();
    db.trait_dict().always_use_dictionary();
    db.callee_dict()
        .build_blocking(db.scoped_for_test(crate::index::view::Part::CallsByCallee));
    db.trait_dict()
        .build_blocking(db.scoped_for_test(crate::index::view::Part::ImplementationsByTrait));
}

#[test]
fn dictionary_scans_match_the_full_walk() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let vocab = vocabulary();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for i in 0..220 {
        write_file(&db, &mut rng, &vocab, i);
    }
    build_dicts(&db);
    assert!(
        db.callee_dict().len() > 100,
        "dictionary should hold the distinct callees"
    );
    assert_equivalent(&db, "after build");

    // The corpus must exercise the cap, or the partial-total path is untested.
    let capped = scan_calls_by_name(&db.view(), "run", Matching::Substring, 1, None).unwrap();
    assert!(capped.total_is_partial, "hot name must hit scan_cap");

    // Rescan: rewrite some files (names change, old keys are removed), drop others, add new files
    // introducing names the built snapshot has never seen.
    for i in (0..220).step_by(3) {
        write_file(&db, &mut rng, &vocab, i);
    }
    let mut writer = db.writer();
    for i in (1..220).step_by(5) {
        writer.remove_file(&path(i)).unwrap();
    }
    writer.commit().unwrap();
    assert_equivalent(&db, "after rescan");

    let mut fresh = vocab.clone();
    fresh.extend(["brand_new_name", "run_brand_new", "Zebra", "日本語2"].map(String::from));
    for i in 220..260 {
        write_file(&db, &mut rng, &fresh, i);
    }
    assert_equivalent(&db, "after new names");

    // A fold merges the new names into the snapshot without changing any answer.
    db.callee_dict()
        .build_blocking(db.scoped_for_test(crate::index::view::Part::CallsByCallee));
    db.trait_dict()
        .build_blocking(db.scoped_for_test(crate::index::view::Part::ImplementationsByTrait));
    assert_equivalent(&db, "after rebuild");
}

#[test]
fn names_recorded_before_the_build_completes_are_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let vocab = vocabulary();
    let mut rng = Rng(7);
    for i in 0..20 {
        write_file(&db, &mut rng, &vocab, i);
    }
    // Written while no snapshot exists (as during a background build): recorded, not yet folded.
    write_file(&db, &mut rng, &["late_arrival".to_string()], 99);
    build_dicts(&db);
    let page = scan_calls_by_name(&db.view(), "late_arrival", Matching::Substring, 10, None).unwrap();
    assert!(page.total > 0);
    assert_equivalent(&db, "late arrival");
}

#[test]
fn unbuilt_dictionary_still_answers_via_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let vocab = vocabulary();
    let mut rng = Rng(11);
    for i in 0..30 {
        write_file(&db, &mut rng, &vocab, i);
    }
    db.callee_dict().set_disabled(true);
    let want = call_page(&db, "run", 20, None);
    db.callee_dict().set_disabled(false);
    // First query: kicks off the background build and is served by the walk.
    let first = call_page(&db, "run", 20, None);
    assert_eq!(want, first);
}

/// Timing comparison, run by hand against a large index:
/// `BM_FR_VIEW=/path/to/dir cargo test --release -- --ignored --nocapture time_name_scans`
/// (`dir` holds an `index.fjall`, such as a group directory; use a copy, the open is read-write).
#[test]
#[ignore = "manual benchmark; needs BM_FR_VIEW"]
fn time_name_scans() {
    use std::time::Instant;
    let Ok(view) = std::env::var("BM_FR_VIEW") else {
        eprintln!("BM_FR_VIEW unset");
        return;
    };
    let db = IndexDb::open(std::path::Path::new(&view)).unwrap();
    let t = Instant::now();
    db.callee_dict()
        .build_blocking(db.scoped_for_test(crate::index::view::Part::CallsByCallee));
    eprintln!(
        "callee dictionary: {} names, {} KiB resident, built in {:?}",
        db.callee_dict().len(),
        db.callee_dict().snapshot_bytes() / 1024,
        t.elapsed()
    );
    let t = Instant::now();
    db.trait_dict()
        .build_blocking(db.scoped_for_test(crate::index::view::Part::ImplementationsByTrait));
    eprintln!(
        "trait dictionary: {} names, {} KiB resident, built in {:?}",
        db.trait_dict().len(),
        db.trait_dict().snapshot_bytes() / 1024,
        t.elapsed()
    );
    let needles = [
        "new",
        "get",
        "run",
        "print",
        "len",
        "UniqueNameInArmis",
        "zzz_absent_name",
        "__init__",
        "a",
    ];
    for needle in needles {
        let mut walk = Vec::new();
        let mut dict = Vec::new();
        let mut first = None;
        for round in 0..5 {
            db.callee_dict().set_disabled(true);
            let t = Instant::now();
            let want = call_page(&db, needle, 50, None);
            walk.push(t.elapsed());
            db.callee_dict().set_disabled(false);
            let t = Instant::now();
            let got = call_page(&db, needle, 50, None);
            dict.push(t.elapsed());
            assert_eq!(want, got, "needle {needle:?} round {round}");
            first = Some(want.0);
        }
        walk.sort();
        dict.sort();
        eprintln!(
            "calls {needle:>20}: total={:<7} walk p50 {:>10.2?}   dict p50 {:>10.2?}",
            first.unwrap_or(0),
            walk[2],
            dict[2]
        );
    }
}

/// Exact matching is applied on the key, before a hit counts toward the scan cap. Applied after the
/// scan instead, the 17,000 calls to `zapx` (which sort before the qualified `ns::zap`) used up the
/// cap of every round and the exact call sites were never reached.
#[test]
fn an_exact_call_scan_is_not_starved_by_substring_only_callees() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let mut calls: Vec<Call> = (0..17_000).map(|k| call("zapx", k)).collect();
    calls.extend((0..3).map(|k| call("ns::zap", 20_000 + k)));
    upsert(&db, &path(0), &l1(&[]), Some(&l2(calls)));

    for stage in ["walk", "dictionary"] {
        let exact = scan_calls_by_name(&db.view(), "zap", Matching::Exact, 10, None).unwrap();
        let callees: Vec<&str> = exact.hits.iter().map(|hit| hit.callee.as_str()).collect();
        assert_eq!(callees, ["ns::zap"; 3], "{stage}");
        assert_eq!(exact.total, 3, "{stage}");
        assert!(!exact.total_is_partial, "{stage}: the exact scan reached the end");
        assert!(exact.next_cursor.is_none(), "{stage}");

        let substring = scan_calls_by_name(&db.view(), "zap", Matching::Substring, 10, None).unwrap();
        assert!(
            substring.total_is_partial,
            "{stage}: the substring scan still stops at its cap"
        );
        assert!(substring.hits.iter().all(|hit| hit.callee == "zapx"), "{stage}");
        build_dicts(&db);
    }
}

#[test]
fn an_exact_impl_scan_is_not_starved_by_substring_only_traits() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let mut impls: Vec<(String, String)> = (0..17_000)
        .map(|k| ("IteratorExt".to_string(), format!("Ty{k}")))
        .collect();
    impls.extend((0..2).map(|k| ("std::iter::Iterator<Item = u8>".to_string(), format!("Exact{k}"))));
    upsert(&db, &path(0), &l1(&impls), Some(&l2(Vec::new())));

    let scan = |matching| {
        scan_impls_fjall(
            &db.view(),
            "Iterator",
            matching,
            None,
            10,
            None,
            |_, _| true,
            |_, _| (0, 0),
        )
        .unwrap()
    };
    let exact = scan(Matching::Exact);
    let types: Vec<&str> = exact.hits.iter().map(|hit| hit.impl_type.as_str()).collect();
    assert_eq!(types, ["Exact0", "Exact1"]);
    assert_eq!(exact.total, 2);
    assert!(!exact.total_is_partial && !exact.has_more);
    assert!(scan(Matching::Substring).total_is_partial);
}

/// Keys the capped-scan fixtures below hold: 17,000 that only contain the name, then 3 that are it.
const STARVING_KEYS: usize = 17_003;
const EXAMINE_CAP: usize = 5_000;

/// An exact scan rejects substring-only keys without counting them toward `scan_cap`, so it needs
/// its own bound: it stops after examining `EXAMINE_CAP` keys, says the total is partial, and hands
/// back a cursor that continues from the last key it looked at, so paging still reaches every hit.
#[test]
fn an_exact_call_scan_stops_at_the_examined_keys_cap_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let mut calls: Vec<Call> = (0..17_000).map(|k| call("zapx", k)).collect();
    calls.extend((0..3).map(|k| call("ns::zap", 20_000 + k)));
    upsert(&db, &path(0), &l1(&[]), Some(&l2(calls)));

    for stage in ["walk", "dictionary"] {
        let scan = |cursor: Option<&[u8]>, cap| {
            scan_calls_examining(&db.view(), "zap", Matching::Exact, 10, cursor, cap).unwrap()
        };
        let first = scan(None, EXAMINE_CAP);
        assert!(first.hits.is_empty(), "{stage}: no exact call among the first keys");
        assert!(first.total_is_partial, "{stage}: the scan did not reach the end");
        assert!(first.stopped_after.is_some() && first.next_cursor.is_some(), "{stage}");

        let mut callees: Vec<String> = Vec::new();
        let mut pages = 1;
        let mut page = first;
        while let Some(cursor) = page.next_cursor.take() {
            let after = cursor.decode_fjall().unwrap();
            page = scan(Some(&after), EXAMINE_CAP);
            callees.extend(page.hits.iter().map(|hit| hit.callee.clone()));
            pages += 1;
            assert!(pages <= 10, "{stage}: paging must terminate");
        }
        assert_eq!(callees, ["ns::zap"; 3], "{stage}: every exact call is reached, once");
        assert_eq!(pages, STARVING_KEYS.div_ceil(EXAMINE_CAP), "{stage}");
        assert!(!page.total_is_partial, "{stage}: the last page ran to the end");

        let whole = scan(None, STARVING_KEYS);
        assert!(
            !whole.total_is_partial && whole.next_cursor.is_none(),
            "{stage}: a scan that ends exactly at the cap is complete"
        );
        assert_eq!(whole.total, 3, "{stage}");
        build_dicts(&db);
    }
}

#[test]
fn an_exact_impl_scan_stops_at_the_examined_keys_cap_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let mut impls: Vec<(String, String)> = (0..17_000)
        .map(|k| ("IteratorExt".to_string(), format!("Ty{k}")))
        .collect();
    impls.extend((0..3).map(|k| ("std::iter::Iterator<Item = u8>".to_string(), format!("Exact{k}"))));
    upsert(&db, &path(0), &l1(&impls), Some(&l2(Vec::new())));

    let scan = |cursor: Option<&[u8]>| {
        scan_impls_examining(
            &db.view(),
            "Iterator",
            Matching::Exact,
            None,
            10,
            cursor,
            |_, _| true,
            |_, _| (0, 0),
            EXAMINE_CAP,
        )
        .unwrap()
    };
    let mut page = scan(None);
    assert!(page.hits.is_empty() && page.total_is_partial && !page.has_more);
    let mut types: Vec<String> = Vec::new();
    let mut pages = 1;
    while let Some(after) = page.stopped_after.take() {
        page = scan(Some(&after));
        types.extend(page.hits.iter().map(|hit| hit.impl_type.clone()));
        pages += 1;
        assert!(pages <= 10, "paging must terminate");
    }
    assert_eq!(types, ["Exact0", "Exact1", "Exact2"]);
    assert_eq!(pages, STARVING_KEYS.div_ceil(EXAMINE_CAP));
    assert!(!page.total_is_partial);
}

/// A cap of zero would examine nothing and so have no key to resume from; it examines one.
#[test]
fn an_examined_keys_cap_of_zero_still_yields_a_resumable_page() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let impls: Vec<(String, String)> = (0..3).map(|k| ("IteratorExt".to_string(), format!("Ty{k}"))).collect();
    upsert(
        &db,
        &path(0),
        &l1(&impls),
        Some(&l2((0..3).map(|k| call("zapx", k)).collect())),
    );

    let calls = scan_calls_examining(&db.view(), "zap", Matching::Exact, 10, None, 0).unwrap();
    assert!(calls.total_is_partial && calls.stopped_after.is_some() && calls.next_cursor.is_some());
    let impls = scan_impls_examining(
        &db.view(),
        "Iterator",
        Matching::Exact,
        None,
        10,
        None,
        |_, _| true,
        |_, _| (0, 0),
        0,
    )
    .unwrap();
    assert!(impls.total_is_partial && impls.stopped_after.is_some());
}

/// Rows of a content no live path holds (an edited file's earlier content, until the group sweep
/// collects it) are examined and count toward the examined-keys cap. A page they stop says so
/// (`total_is_partial`, a cursor where it stopped), and paging on still reaches every live hit once.
#[test]
fn dead_rows_that_fill_the_examined_keys_cap_stop_a_page_visibly_and_paging_finds_every_live_hit() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    for i in 100..104 {
        let mut writer = db.writer();
        let dead = l2((0..40).map(|k| call("zap", k)).collect());
        writer
            .upsert_file(&path(i), &ck(&path(i)), &l1(&[]), Some(&dead))
            .unwrap();
        writer.commit().unwrap();
    }
    for i in 0..5 {
        upsert(&db, &path(i), &l1(&[]), Some(&l2(vec![call("zap", 1), call("zap", 2)])));
    }

    let mut cursor: Option<Vec<u8>> = None;
    let mut seen: Vec<(String, u32)> = Vec::new();
    let mut stops = 0;
    for _ in 0..200 {
        let page = scan_calls_examining(&db.view(), "zap", Matching::Exact, 100, cursor.as_deref(), 25).unwrap();
        if page.stopped_after.is_some() {
            assert!(page.total_is_partial, "a stop at the cap is reported");
            stops += 1;
        }
        seen.extend(
            page.hits
                .iter()
                .zip(&page.hit_starts)
                .map(|(hit, start)| (hit.path.to_string(), *start)),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next.decode_fjall().unwrap()),
            None => break,
        }
    }
    assert!(stops > 0, "the dead rows filled the cap, or this test proves nothing");
    let mut expected: Vec<(String, u32)> = (0..5)
        .flat_map(|i| [(path(i).to_string(), 1), (path(i).to_string(), 2)])
        .collect();
    expected.sort();
    seen.sort();
    assert_eq!(seen, expected, "every live hit once, no dead one");
}
