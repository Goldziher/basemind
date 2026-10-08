//! Equivalence of the dictionary-backed name scans with the full-partition walk they replace.
//!
//! Every comparison runs the SAME scan twice on one database: once with the dictionary disabled
//! (the legacy walk, which is the reference) and once with it built, and requires identical pages.

use crate::extract::{Call, FileMapL1, FileMapL2, Implementation};
use crate::index::IndexDb;
use crate::path::RelPath;

use super::scan_calls_by_name;
use crate::mcp::helpers_impls::scan_impls_fjall;

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
    let mut writer = db.writer();
    writer.upsert_file(&rel, &l1(&impls), Some(&l2(calls))).unwrap();
    writer.commit().unwrap();
}

type CallPage = (u32, bool, Vec<Vec<u8>>, Vec<(String, u32, u32)>, bool);

fn call_page(db: &IndexDb, needle: &str, limit: usize, cursor: Option<&[u8]>) -> CallPage {
    let page = scan_calls_by_name(db, needle, limit, cursor).unwrap();
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
        db,
        needle,
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
                db.callee_names.set_disabled(true);
                let want = call_page(db, needle, limit, want_cursor.as_deref());
                db.callee_names.set_disabled(false);
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
                    db.trait_names.set_disabled(true);
                    let want = impl_page(db, needle, language, limit, cursor.as_deref());
                    db.trait_names.set_disabled(false);
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
    db.callee_names.always_use_dictionary();
    db.trait_names.always_use_dictionary();
    db.callee_names.build_blocking(&db.calls_by_callee);
    db.trait_names.build_blocking(&db.implementations_by_trait);
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
        db.callee_names.len() > 100,
        "dictionary should hold the distinct callees"
    );
    assert_equivalent(&db, "after build");

    // The corpus must exercise the cap, or the partial-total path is untested.
    let capped = scan_calls_by_name(&db, "run", 1, None).unwrap();
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
    db.callee_names.build_blocking(&db.calls_by_callee);
    db.trait_names.build_blocking(&db.implementations_by_trait);
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
    let page = scan_calls_by_name(&db, "late_arrival", 10, None).unwrap();
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
    db.callee_names.set_disabled(true);
    let want = call_page(&db, "run", 20, None);
    db.callee_names.set_disabled(false);
    // First query: kicks off the background build and is served by the walk.
    let first = call_page(&db, "run", 20, None);
    assert_eq!(want, first);
}

/// Timing comparison, run by hand against a large index:
/// `BM_FR_VIEW=/path/to/view_dir cargo test --release -- --ignored --nocapture time_name_scans`
/// (`view_dir` holds `index.fjall`; use a copy, the open is read-write).
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
    db.callee_names.build_blocking(&db.calls_by_callee);
    eprintln!(
        "callee dictionary: {} names, {} KiB resident, built in {:?}",
        db.callee_names.len(),
        db.callee_names.snapshot_bytes() / 1024,
        t.elapsed()
    );
    let t = Instant::now();
    db.trait_names.build_blocking(&db.implementations_by_trait);
    eprintln!(
        "trait dictionary: {} names, {} KiB resident, built in {:?}",
        db.trait_names.len(),
        db.trait_names.snapshot_bytes() / 1024,
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
            db.callee_names.set_disabled(true);
            let t = Instant::now();
            let want = call_page(&db, needle, 50, None);
            walk.push(t.elapsed());
            db.callee_names.set_disabled(false);
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
