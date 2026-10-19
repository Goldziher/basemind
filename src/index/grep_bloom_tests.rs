use super::*;

fn row_for(text: &str) -> Vec<u8> {
    build_row(text.as_bytes())
}

fn rejects(pattern: &str, text: &str) -> bool {
    let needle = Needle::compile(pattern).expect("usable needle");
    let row = row_for(text);
    !needle.passes(parse_row(&row).expect("row"))
}

#[test]
fn a_present_literal_is_never_rejected() {
    let text = "fn main() { let post_processing = 1; }\n".repeat(50);
    assert!(!rejects("post_processing", &text));
    assert!(!rejects(r"post_processing\s*=", &text));
    assert!(!rejects("(?i)POST_PROCESSING", &text));
    assert!(!rejects("zzz_absent|post_processing", &text));
}

#[test]
fn an_absent_literal_is_rejected() {
    assert!(rejects("eq9fsw", "fn main() { println!(\"hello\"); }"));
}

#[test]
fn patterns_without_a_required_trigram_sweep_everything() {
    for pattern in [".*", r"\w+", "ab", "a|bcd", "(?i)k", "^", r"\d{3}"] {
        assert!(Needle::compile(pattern).is_none(), "{pattern} must not be prefiltered");
    }
}

#[test]
fn case_insensitive_literals_match_every_casing() {
    let needle = Needle::compile("(?i)needle").expect("expanded to case variants");
    for text in ["xx NEEDLE xx", "xx needle xx", "xx NeEdLe xx"] {
        assert!(needle.passes(parse_row(&row_for(text)).unwrap()), "{text}");
    }
}

#[test]
fn unicode_literals_are_matched_on_their_utf8_bytes() {
    let needle = Needle::compile("caf\u{e9}s").expect("literal");
    assert!(needle.passes(parse_row(&row_for("two caf\u{e9}s here")).unwrap()));
}

#[test]
fn foreign_or_short_rows_do_not_parse() {
    let mut row = row_for("hello world");
    assert!(parse_row(&row).is_some());
    row[0] = ROW_VERSION + 1;
    assert!(parse_row(&row).is_none());
    assert!(parse_row(&[ROW_VERSION; HEADER_BYTES]).is_none());
    assert!(parse_row(&[]).is_none());
}

#[test]
fn row_size_tracks_the_file_within_its_bounds() {
    assert_eq!(build_row(b"abc").len(), HEADER_BYTES + MIN_BLOOM_BYTES);
    let big = vec![b'x'; 8 * 1024];
    assert_eq!(build_row(&big).len(), HEADER_BYTES + 1024);
    let huge = vec![b'x'; 4 * 1024 * 1024];
    assert_eq!(build_row(&huge).len(), HEADER_BYTES + MAX_BLOOM_BYTES);
}

fn open_db() -> (tempfile::TempDir, IndexDb) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = IndexDb::open(dir.path()).expect("index");
    (dir, db)
}

fn put(db: &IndexDb, hash_hex: &str, text: &str) {
    let mut writer = db.writer();
    writer.upsert_grep_bloom(hash_hex, row_for(text));
    writer.commit().expect("commit");
}

#[test]
fn a_row_round_trips_under_its_content_hash() {
    let (_dir, db) = open_db();
    put(&db, "aa11", "fn alpha() {}");
    assert_eq!(
        db.grep_bloom_row("aa11").as_deref(),
        Some(row_for("fn alpha() {}").as_slice())
    );
    assert!(db.grep_bloom_current("aa11"));
    assert!(!db.grep_bloom_current("bb22"));
}

#[test]
fn a_hash_that_prefixes_another_reads_only_its_own_row() {
    let (_dir, db) = open_db();
    put(&db, "ab", "short key");
    put(&db, "abc", "longer key");
    assert_eq!(
        db.grep_bloom_row("ab").as_deref(),
        Some(row_for("short key").as_slice())
    );
    assert_eq!(
        db.grep_bloom_row("abc").as_deref(),
        Some(row_for("longer key").as_slice())
    );
    assert!(db.grep_bloom_row("a").is_none());
}

#[test]
fn the_sweep_drops_only_hashes_no_live_file_references() {
    let (_dir, db) = open_db();
    for hash in ["h1", "h2", "h3"] {
        put(&db, hash, hash);
    }
    let live: ahash::AHashSet<&str> = ["h1", "h3"].into_iter().collect();
    assert_eq!(db.sweep_grep_bloom(&live).expect("sweep"), 1);
    assert!(db.grep_bloom_current("h1") && db.grep_bloom_current("h3"));
    assert!(!db.grep_bloom_current("h2"));
    assert_eq!(
        db.sweep_grep_bloom(&live).expect("sweep"),
        0,
        "a second sweep is a no-op"
    );
}

#[test]
fn two_paths_with_the_same_content_share_one_row_and_both_skip() {
    let (_dir, db) = open_db();
    let root = tempfile::tempdir().expect("root");
    let text = "fn unrelated() { println!(\"hello\"); }\n";
    let mut entries = Vec::new();
    for name in ["a.rs", "b.rs"] {
        std::fs::write(root.path().join(name), text).expect("write");
        let meta = std::fs::metadata(root.path().join(name)).expect("stat");
        entries.push(GrepEntry {
            rel: RelPath::from(name),
            hash_hex: "same".to_owned(),
            size_bytes: meta.len(),
            mtime_ns: mtime_nanos(&meta),
        });
    }
    put(&db, "same", text);
    put(&db, "same", text);
    assert_eq!(db.grep_bloom_row_count(), 1);
    let needle = Needle::compile("zzz_absent_literal").expect("needle");
    assert_eq!(db.grep_skip_verdicts(root.path(), &entries, &needle), vec![true, true]);
}

#[test]
fn a_live_file_that_moved_off_its_indexed_stamp_is_a_candidate() {
    let (_dir, db) = open_db();
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("a.rs"), "fn unrelated() {}\n").expect("write");
    let meta = std::fs::metadata(root.path().join("a.rs")).expect("stat");
    put(&db, "h", "fn unrelated() {}\n");
    let needle = Needle::compile("zzz_absent_literal").expect("needle");
    let entry = |size_bytes| GrepEntry {
        rel: RelPath::from("a.rs"),
        hash_hex: "h".to_owned(),
        size_bytes,
        mtime_ns: mtime_nanos(&meta),
    };
    assert_eq!(
        db.grep_skip_verdicts(root.path(), &[entry(meta.len())], &needle),
        vec![true]
    );
    assert_eq!(
        db.grep_skip_verdicts(root.path(), &[entry(meta.len() + 1)], &needle),
        vec![false]
    );
}

/// Plant a row under `hash` whose first byte is not the current layout, as an older build left it.
fn put_stale_layout(db: &IndexDb, hash: &str) {
    let mut row = row_for(hash);
    row[0] = ROW_VERSION - 1;
    let mut writer = db.writer();
    writer.upsert_grep_bloom(hash, row);
    writer.commit().expect("commit");
}

#[test]
fn the_first_sweep_drops_older_layout_rows_and_records_it_in_meta() {
    let (_dir, db) = open_db();
    put(&db, "h1", "current");
    put_stale_layout(&db, "h2");
    let live: ahash::AHashSet<&str> = ["h1", "h2"].into_iter().collect();
    assert!(db.meta.get(META_ROW_SWEPT_VER).expect("meta").is_none());
    assert_eq!(
        db.sweep_grep_bloom(&live).expect("sweep"),
        1,
        "a live hash with an unreadable row goes"
    );
    assert!(db.grep_bloom_current("h1") && db.grep_bloom_row("h2").is_none());
    assert_eq!(
        db.meta.get(META_ROW_SWEPT_VER).expect("meta").as_deref(),
        Some([ROW_VERSION].as_slice())
    );
}

#[test]
fn later_sweeps_read_keys_only_and_still_remove_dead_keys() {
    let (_dir, db) = open_db();
    let live: ahash::AHashSet<&str> = ["h1", "h2"].into_iter().collect();
    db.sweep_grep_bloom(&live).expect("first sweep");

    put(&db, "h1", "current");
    put_stale_layout(&db, "h2");
    put(&db, "dead", "no file holds this");
    assert_eq!(db.sweep_grep_bloom(&live).expect("steady sweep"), 1);
    assert!(
        db.grep_bloom_row("dead").is_none(),
        "a dead key is removed without reading it"
    );
    assert!(
        db.grep_bloom_row("h2").is_some(),
        "the value of a live key is not decoded after the version sweep ran"
    );
}

#[test]
fn without_the_marker_the_version_sweep_reads_values_again() {
    let (_dir, db) = open_db();
    let live: ahash::AHashSet<&str> = ["h1"].into_iter().collect();
    db.sweep_grep_bloom(&live).expect("first sweep");
    put_stale_layout(&db, "h1");
    assert_eq!(
        db.sweep_grep_bloom(&live).expect("steady sweep"),
        0,
        "marker present: values unread"
    );
    db.meta.remove(META_ROW_SWEPT_VER).expect("forget the marker");
    assert_eq!(db.sweep_grep_bloom(&live).expect("version sweep"), 1);
}

#[test]
fn remove_grep_bloom_rows_drops_only_the_named_hashes() {
    let (_dir, db) = open_db();
    for hash in ["h1", "h2", "h3"] {
        put(&db, hash, hash);
    }
    db.remove_grep_bloom_rows(["h1", "h3", "never-indexed"])
        .expect("remove");
    assert!(db.grep_bloom_current("h2"));
    assert!(db.grep_bloom_row("h1").is_none() && db.grep_bloom_row("h3").is_none());
}

fn skip_for(db: &IndexDb, root: &Path, entry: GrepEntry) -> bool {
    let needle = Needle::compile("zzz_absent_literal").expect("needle");
    db.grep_skip_verdicts(root, &[entry], &needle)[0]
}

#[test]
fn an_unknown_indexed_stamp_is_never_trusted_even_when_the_live_stamp_is_also_unknown() {
    let (_dir, db) = open_db();
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("a.rs");
    std::fs::write(&path, "fn unrelated() {}\n").expect("write");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(std::time::UNIX_EPOCH))
        .expect("set mtime to the epoch");
    let meta = std::fs::metadata(&path).expect("stat");
    assert_eq!(mtime_nanos(&meta), 0, "the live stamp reads as 0");
    put(&db, "h", "fn unrelated() {}\n");
    let entry = GrepEntry {
        rel: RelPath::from("a.rs"),
        hash_hex: "h".to_owned(),
        size_bytes: meta.len(),
        mtime_ns: 0,
    };
    assert!(!skip_for(&db, root.path(), entry), "0 == 0 is not a match");
}

#[test]
fn settled_mtime_zeroes_a_stamp_within_two_seconds_of_the_clock_either_side() {
    use crate::scanner_file::settled_mtime;
    let now = 10_000_000_000_i64;
    assert_eq!(settled_mtime(now, now), 0);
    assert_eq!(settled_mtime(now - 1_999_999_999, now), 0);
    assert_eq!(
        settled_mtime(now + 1_999_999_999, now),
        0,
        "a slightly future stamp is untrusted too"
    );
    assert_eq!(settled_mtime(now - 2_000_000_000, now), now - 2_000_000_000);
    assert_eq!(settled_mtime(now + 60_000_000_000, now), now + 60_000_000_000);
    assert_eq!(settled_mtime(0, now), 0);
}

/// Every text `pattern` matches must pass the needle; `usable` also pins that the pattern is
/// prefiltered at all, so the loop cannot pass by the needle quietly not existing.
fn never_false_skips(pattern: &str, usable: bool, texts: &[&str]) {
    let re = regex::Regex::new(pattern).expect("regex");
    let needle = Needle::compile(pattern);
    assert_eq!(needle.is_some(), usable, "{pattern}: usable");
    let Some(needle) = needle else { return };
    let mut matched = 0;
    for text in texts.iter().filter(|text| re.is_match(text)) {
        matched += 1;
        assert!(
            needle.passes(parse_row(&row_for(text)).expect("row")),
            "{pattern} matches {text:?} but the bloom rejects it"
        );
    }
    assert!(matched > 0, "{pattern}: no sample matched, the check proved nothing");
}

#[test]
fn mixed_length_alternations_never_false_skip() {
    for (pattern, usable, texts) in [
        ("a|bcd", false, &["a", "bcd", "xx a xx"][..]),
        ("abc|d", false, &["d", "abc"][..]),
        ("foo|ba", false, &["ba", "foo"][..]),
        ("(abc|d)ef", true, &["abcef", "def", "xx def"][..]),
        ("foobar|bazqux", true, &["foobar", "bazqux"][..]),
        ("x(abc|de)y", true, &["xabcy", "xdey"][..]),
    ] {
        never_false_skips(pattern, usable, texts);
    }
}

#[test]
fn word_boundary_literals_never_false_skip() {
    never_false_skips(r"\bfoo\b", true, &["foo", "a foo b", "(foo)"]);
    never_false_skips(r"\bfoobar\w*", true, &["foobar", "x foobarbaz"]);
}

#[test]
fn unicode_case_folding_never_false_skips() {
    never_false_skips("(?i)kit", true, &["KIT", "kit", "\u{212A}it", "\u{212A}IT"]);
    never_false_skips("(?i)strasse", true, &["STRASSE", "\u{17F}trasse", "Stra\u{17F}se"]);
    never_false_skips("(?i)caf\u{e9}s", true, &["CAF\u{c9}S", "caf\u{e9}s"]);
}
