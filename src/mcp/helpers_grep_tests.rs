//! Unit tests of the `code_grep` sweep: cursor packing, page selection and the equivalence of the
//! buffer-reusing sweep with a plain read-per-file one.

use std::path::Path;

use super::*;
use crate::index::view::IndexView;

#[test]
fn packs_and_unpacks_a_file_and_hit_ordinal() {
    assert_eq!(unpack_cursor(pack_cursor(0, 0)), (0, 0));
    assert_eq!(unpack_cursor(pack_cursor(68_291, 7)), (68_291, 7));
    assert_eq!(unpack_cursor(pack_cursor(1, u32::MAX)), (1, u32::MAX));
}

#[test]
fn selects_whole_files_until_the_limit_is_filled() {
    let (selected, resume) = select_hits(&[2, 0, 3], 5, 0, 0);
    assert_eq!(selected.len(), 2, "both matching files fit the limit");
    assert_eq!(selected[0].take, 2);
    assert_eq!(selected[1].file_idx, 2);
    assert_eq!(selected[1].take, 3);
    assert!(resume.is_none(), "nothing left over, so no resume cursor");
}

#[test]
fn resumes_inside_a_file_whose_matches_exceed_the_limit() {
    let (selected, resume) = select_hits(&[10], 4, 0, 0);
    assert_eq!(selected[0].take, 4);
    assert_eq!(
        unpack_cursor(resume.expect("limit cut the file short")),
        (0, 4),
        "resume at the 5th match of file 0 — no replay, no loss"
    );
}

#[test]
fn resume_cursor_skips_the_hits_already_returned_from_the_first_file() {
    let (selected, resume) = select_hits(&[6], 4, 3, 4);
    assert_eq!(selected[0].hit_skip, 4, "the first file resumes past 4 consumed hits");
    assert_eq!(selected[0].take, 4);
    assert_eq!(
        unpack_cursor(resume.expect("still more matches in the file")),
        (3, 8),
        "next page starts at the 9th match of candidate file 3"
    );
}

#[test]
fn a_literal_pattern_is_prefiltered_and_a_metacharacter_pattern_is_not() {
    assert_eq!(regex::escape("OptimizationStatus"), "OptimizationStatus");
    assert_ne!(regex::escape("fn (spawn|block)"), "fn (spawn|block)");
}

#[test]
fn counts_every_match_and_the_prefilter_never_changes_the_count() {
    let re = LineRegex::new(Regex::new("needle").expect("regex"));
    let finder = Finder::new("needle".as_bytes()).into_owned();
    let source = b"needle\nhay\nneedle needle\n";
    assert_eq!(count_matches(source, &re, None), Some(3));
    assert_eq!(count_matches(source, &re, Some(&finder)), Some(3));
    assert_eq!(count_matches(b"hay only", &re, Some(&finder)), Some(0));
    assert_eq!(
        count_matches(b"needle \xff", &re, Some(&finder)),
        None,
        "non-UTF-8 cannot be searched, and is reported as such"
    );
    assert_eq!(
        count_matches(b"hay \xff", &re, Some(&finder)),
        Some(0),
        "a file without the literal cannot match, whatever its encoding"
    );
}

// ---- equivalence against a plain read-per-file sweep ------------------------------------

/// The oracle: `read_to_string` per file for the count pass, then a second read of the selected
/// files. No buffer reuse, no parallelism, nothing to get wrong.
mod reference {
    use super::*;

    pub(super) fn read(root: &Path, path: &RelPath) -> Option<String> {
        std::fs::read_to_string(root.join(path.to_path_buf())).ok()
    }

    pub(super) fn count(
        root: &Path,
        scanned: &[&RelPath],
        re: &Regex,
        literal: Option<&Finder<'static>>,
        skip_hits: u32,
    ) -> Vec<u32> {
        let mut counts: Vec<u32> = scanned
            .iter()
            .map(|p| match read(root, p) {
                Some(source) => {
                    if let Some(f) = literal
                        && f.find(source.as_bytes()).is_none()
                    {
                        0
                    } else {
                        line_matches(re, &source).count().min(u32::MAX as usize) as u32
                    }
                }
                None => 0,
            })
            .collect();
        if let Some(first) = counts.first_mut() {
            *first = first.saturating_sub(skip_hits);
        }
        counts
    }

    pub(super) fn materialize(
        root: &Path,
        scanned: &[&RelPath],
        selected: &[Selection],
        re: &LineRegex,
        ctx: bool,
        skip_files: usize,
    ) -> (Vec<GrepHit>, Vec<u64>) {
        let mut hits = Vec::new();
        let mut cursors = Vec::new();
        for sel in selected {
            let path = scanned[sel.file_idx];
            let Some(source) = read(root, path) else { continue };
            for (c, h) in collect_hits(path, &source, true, re, sel, ctx, skip_files + sel.file_idx) {
                hits.push(h);
                cursors.push(c);
            }
        }
        (hits, cursors)
    }
}

/// Deterministic xorshift so the generated corpus is identical on every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "needle",
    "Needle",
    "NEEDLE",
    "hay",
    "fn",
    "def",
    "class",
    "post_processing",
    "naïve",
    "日本語",
    "x",
    "foo_bar",
    "\t",
    "  ",
    "😀",
    "needle needle",
];

fn generate_corpus(dir: &Path) -> Vec<RelPath> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut paths = Vec::new();
    for i in 0..120 {
        let rel = format!("d{}/f{i}.txt", i % 5);
        std::fs::create_dir_all(dir.join(format!("d{}", i % 5))).expect("mkdir");
        let mut body = String::new();
        // Some files are deliberately dense (more matches than any page limit), some empty.
        let lines = match i % 7 {
            0 => 0,
            1 => 60,
            _ => (rng.next() % 12) as usize,
        };
        let eol = if i % 3 == 0 { "\r\n" } else { "\n" };
        for _ in 0..lines {
            for _ in 0..(1 + rng.next() % 5) {
                body.push_str(rng.pick(WORDS));
                body.push(' ');
            }
            body.push_str(eol);
        }
        let bytes = match i {
            // Invalid UTF-8: must count zero in both implementations.
            10 | 11 => {
                let mut b = body.into_bytes();
                b.extend_from_slice(b"needle \xff\xfe");
                b
            }
            _ => body.into_bytes(),
        };
        std::fs::write(dir.join(&rel), bytes).expect("write");
        paths.push(RelPath::from(rel));
    }
    // Indexed but gone from disk, as after a delete between scan and query.
    paths.push(RelPath::from("d0/deleted.txt"));
    paths
}

#[test]
fn the_optimised_sweep_is_byte_identical_to_the_original_over_a_generated_corpus() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = generate_corpus(dir.path());
    let refs: Vec<&RelPath> = paths.iter().collect();

    let patterns = [
        "needle",
        "NEEDLE",
        "(?i)needle",
        "needle|hay",
        r"fn\s+\w+",
        "post_processing",
        "naïve",
        "日本語",
        "😀",
        r"\bx\b",
        "^needle",
        "needle$",
        "ne+dle",
        "zzz_absent",
        "(?m)^$",
        "",
    ];
    for pattern in patterns {
        let re = LineRegex::new(Regex::new(pattern).expect("regex"));
        let literal = (regex::escape(pattern) == pattern).then(|| Finder::new(pattern.as_bytes()).into_owned());
        for skip_hits in [0u32, 3] {
            let (new_counts, _) = count_all(
                &Sweep::new(IndexedFiles::contained(dir.path())),
                &refs,
                None,
                &re,
                literal.as_ref(),
                skip_hits,
            );
            let old_counts = reference::count(dir.path(), &refs, &re, literal.as_ref(), skip_hits);
            assert_eq!(
                new_counts, old_counts,
                "counts diverge for {pattern:?} skip_hits={skip_hits}"
            );

            // Walk every page of every limit, comparing hits, cursors and resume points.
            for limit in [25usize, 1000] {
                for ctx in [false, true] {
                    if skip_hits != 0 && limit != 1000 {
                        continue; // keep the paging walk affordable in debug builds
                    }
                    let (mut skip_files, mut skip_hits) = (0usize, skip_hits);
                    loop {
                        let window = &refs[skip_files.min(refs.len())..];
                        let (counts, _) = count_all(
                            &Sweep::new(IndexedFiles::contained(dir.path())),
                            window,
                            None,
                            &re,
                            literal.as_ref(),
                            skip_hits,
                        );
                        let (sel, resume) = select_hits(&counts, limit, skip_files, skip_hits);
                        let new = materialize(
                            IndexedFiles::contained(dir.path()),
                            window,
                            &sel,
                            &|_, _, _| true,
                            &re,
                            ctx,
                            skip_files,
                        );
                        let old_counts = reference::count(dir.path(), window, &re, literal.as_ref(), skip_hits);
                        assert_eq!(counts, old_counts);
                        let old = reference::materialize(dir.path(), window, &sel, &re, ctx, skip_files);
                        assert_eq!(new.1, old.1, "cursors diverge for {pattern:?} limit={limit}");
                        assert_eq!(
                            serde_json::to_string(&new.0).expect("json"),
                            serde_json::to_string(&old.0).expect("json"),
                            "hits diverge for {pattern:?} limit={limit} ctx={ctx}"
                        );
                        let Some(next) = resume else { break };
                        (skip_files, skip_hits) = unpack_cursor(next);
                    }
                }
            }
        }
    }
}

// ---- trigram-bloom prefilter equivalence -------------------------------------------------

/// Index the current bytes and stat of every readable file, as a scan would, returning one entry per
/// path (an unreadable file gets an entry no row answers to).
fn scan_blooms(db: &crate::index::IndexDb, dir: &Path, paths: &[RelPath]) -> Vec<GrepEntry> {
    let mut writer = db.writer();
    let mut entries = Vec::new();
    for rel in paths {
        let abs = dir.join(rel.to_path_buf());
        let (Ok(bytes), Ok(meta)) = (std::fs::read(&abs), std::fs::metadata(&abs)) else {
            entries.push(GrepEntry {
                rel: rel.clone(),
                hash_hex: "unreadable".to_owned(),
                size_bytes: 0,
                mtime_ns: 0,
            });
            continue;
        };
        let hash_hex = crate::hashing::hex(&crate::hashing::hash_bytes(&bytes));
        writer.upsert_grep_bloom(&hash_hex, crate::index::grep_bloom::build_row(&bytes));
        entries.push(GrepEntry {
            rel: rel.clone(),
            hash_hex,
            size_bytes: meta.len(),
            mtime_ns: crate::scanner_file::mtime_nanos(&meta),
        });
    }
    writer.commit().expect("commit blooms");
    entries
}

fn set_mtime_ahead(path: &Path, secs: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).expect("open");
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(secs))
        .expect("set mtime");
}

#[test]
fn the_bloom_prefilter_never_changes_a_result_even_after_edits_deletes_and_new_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx_dir = tempfile::tempdir().expect("index dir");
    let db = crate::index::IndexDb::open(idx_dir.path()).expect("index");
    let mut paths = generate_corpus(dir.path());

    // A long single line with the needle only at the very end, and a unique token in one file.
    let mut long_line = "abc def ghi ".repeat(20_000);
    long_line.push_str("tail_marker_xyz");
    std::fs::write(dir.path().join("d1/long.txt"), long_line).expect("write long");
    paths.push(RelPath::from("d1/long.txt"));
    std::fs::write(dir.path().join("d2/unique.txt"), "alpha unique_token_qq beta\n").expect("write");
    paths.push(RelPath::from("d2/unique.txt"));

    let mut entries = scan_blooms(&db, dir.path(), &paths);

    // After the scan: an edit that adds a token (size changes), a same-size edit with a bumped
    // mtime, a deletion, and a file the scan never saw.
    std::fs::write(dir.path().join("d3/f3.txt"), "now holds unique_token_qq and needle\n").expect("edit");
    let same_size = dir.path().join("d4/f4.txt");
    let before = std::fs::read(&same_size).expect("read");
    let mut edited = vec![b'q'; before.len()];
    let marker = b"unique_token_qq";
    if edited.len() >= marker.len() {
        edited[..marker.len()].copy_from_slice(marker);
    }
    std::fs::write(&same_size, &edited).expect("same-size edit");
    set_mtime_ahead(&same_size, 5);
    std::fs::remove_file(dir.path().join("d1/f1.txt")).expect("delete");
    std::fs::write(dir.path().join("d0/new.txt"), "brand new unique_token_qq needle\n").expect("new");
    paths.push(RelPath::from("d0/new.txt"));
    entries.push(GrepEntry {
        rel: RelPath::from("d0/new.txt"),
        hash_hex: "never-indexed".to_owned(),
        size_bytes: 0,
        mtime_ns: 0,
    });

    let refs: Vec<&RelPath> = paths.iter().collect();
    let patterns = [
        "needle",
        "NEEDLE",
        "(?i)needle",
        "(?i)NeEdLe",
        "needle|hay",
        "post_processing",
        "post_proc",
        "naïve",
        "日本語",
        "😀",
        "unique_token_qq",
        "unique_token_qq|tail_marker_xyz",
        "tail_marker_xyz$",
        "^alpha unique",
        r"unique_token_\w+",
        r"tail_marker_xyz\b",
        "zzz_absent_literal",
        "zzz_absent_literal|needle",
        "(?i)zzz_absent",
        r"fn\s+\w+",
        ".*",
        "ab",
        "",
    ];
    let mut skipped_somewhere = false;
    for pattern in patterns {
        let re = LineRegex::new(Regex::new(pattern).expect("regex"));
        let literal = (regex::escape(pattern) == pattern).then(|| Finder::new(pattern.as_bytes()).into_owned());
        let skip = crate::index::grep_bloom::Needle::compile(pattern)
            .map(|needle| db.view().grep_skip_verdicts(dir.path(), &entries, &needle));
        skipped_somewhere |= skip.as_ref().is_some_and(|s| s.iter().any(|&x| x));
        for skip_hits in [0u32, 2] {
            let (filtered, _) = count_all(
                &Sweep::new(IndexedFiles::contained(dir.path())),
                &refs,
                skip.as_deref(),
                &re,
                literal.as_ref(),
                skip_hits,
            );
            let oracle = reference::count(dir.path(), &refs, &re, literal.as_ref(), skip_hits);
            assert_eq!(filtered, oracle, "counts diverge for {pattern:?} skip_hits={skip_hits}");
            for limit in [7usize, 1000] {
                let (sel_f, resume_f) = select_hits(&filtered, limit, 0, skip_hits);
                let (sel_o, resume_o) = select_hits(&oracle, limit, 0, skip_hits);
                assert_eq!(resume_f, resume_o, "cursor diverges for {pattern:?}");
                let hits_f = materialize(
                    IndexedFiles::contained(dir.path()),
                    &refs,
                    &sel_f,
                    &|_, _, _| true,
                    &re,
                    true,
                    0,
                );
                let hits_o = reference::materialize(dir.path(), &refs, &sel_o, &re, true, 0);
                assert_eq!(hits_f.1, hits_o.1);
                assert_eq!(
                    serde_json::to_string(&hits_f.0).expect("json"),
                    serde_json::to_string(&hits_o.0).expect("json"),
                    "hits diverge for {pattern:?} limit={limit}"
                );
            }
        }
    }
    assert!(
        skipped_somewhere,
        "the prefilter must actually skip files on this corpus"
    );

    // The stale and new files must stay candidates for the token they now hold.
    let needle = crate::index::grep_bloom::Needle::compile("unique_token_qq").expect("needle");
    let skip = db.view().grep_skip_verdicts(dir.path(), &entries, &needle);
    for (rel, skipped) in paths.iter().zip(&skip) {
        if ["d3/f3.txt", "d4/f4.txt", "d0/new.txt", "d2/unique.txt"].contains(&rel.to_string().as_str()) {
            assert!(!skipped, "{rel} holds the token and must not be skipped");
        }
    }
    assert!(
        paths.iter().zip(&skip).any(|(_, skipped)| *skipped),
        "files without the token are skipped"
    );
}

#[test]
fn a_missing_or_foreign_bloom_row_is_a_candidate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx_dir = tempfile::tempdir().expect("index dir");
    let db = crate::index::IndexDb::open(idx_dir.path()).expect("index");
    std::fs::write(dir.path().join("a.txt"), "nothing relevant here\n").expect("write");
    let rel = RelPath::from("a.txt");
    let meta = std::fs::metadata(dir.path().join("a.txt")).expect("stat");
    let entry = GrepEntry {
        rel: rel.clone(),
        hash_hex: crate::hashing::hex(&crate::hashing::hash_bytes(b"nothing relevant here\n")),
        size_bytes: meta.len(),
        mtime_ns: crate::scanner_file::mtime_nanos(&meta),
    };
    let needle = crate::index::grep_bloom::Needle::compile("absent_literal").expect("needle");
    assert_eq!(
        db.view()
            .grep_skip_verdicts(dir.path(), std::slice::from_ref(&entry), &needle),
        vec![false],
        "no row"
    );
    let mut writer = db.writer();
    writer.upsert_grep_bloom(&entry.hash_hex, vec![99; 40]);
    writer.commit().expect("commit");
    assert_eq!(
        db.view()
            .grep_skip_verdicts(dir.path(), std::slice::from_ref(&entry), &needle),
        vec![false],
        "foreign row"
    );
    scan_blooms(&db, dir.path(), std::slice::from_ref(&rel));
    assert_eq!(
        db.view()
            .grep_skip_verdicts(dir.path(), std::slice::from_ref(&entry), &needle),
        vec![true],
        "current row"
    );
    let mut writer = db.writer();
    writer.remove_file(&rel).expect("remove");
    writer.commit().expect("commit");
    assert!(
        db.view().grep_bloom_row(&entry.hash_hex).is_some(),
        "the row is shared by content, so removing one path keeps it"
    );
}

#[test]
fn the_read_buffer_does_not_retain_a_giant_files_capacity() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("big.txt"), vec![b'a'; RETAINED_BUF_CAP * 3]).expect("write");
    let len = with_indexed_bytes(Some(dir.path().join("big.txt")), u64::MAX, <[u8]>::len);
    assert_eq!(len, Some(RETAINED_BUF_CAP * 3));
    READ_BUF.with(|b| assert!(b.borrow().capacity() <= RETAINED_BUF_CAP, "scratch stays bounded"));
}

#[test]
fn a_file_over_the_size_bound_is_not_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, b"12345").expect("write");
    assert_eq!(with_indexed_bytes(Some(file.clone()), 5, <[u8]>::len), Some(5));
    assert_eq!(with_indexed_bytes(Some(file), 4, <[u8]>::len), None);
}

fn line_anchored(pattern: &str) -> LineRegex {
    LineRegex::new(
        regex::RegexBuilder::new(pattern)
            .multi_line(true)
            .crlf(true)
            .build()
            .expect("regex"),
    )
}

/// An anchor matches at the end of the text as well as at every line. After a trailing newline
/// that position is past the last line, where there is nothing to report.
#[test]
fn an_anchor_does_not_match_past_the_last_line() {
    let count = |pattern: &str, text: &str| count_matches(text.as_bytes(), &line_anchored(pattern), None);
    assert_eq!(count("^$", "a\n\nb\n"), Some(1), "the empty second line only");
    assert_eq!(count("^", "a\nb\n"), Some(2));
    assert_eq!(count("$", "a\nb\n"), Some(2));
    assert_eq!(
        count("$", "a\nb"),
        Some(2),
        "the end of an unterminated last line is on that line"
    );
    assert_eq!(count(r"^\s*$", "a\n  \n"), Some(1));
    assert_eq!(count("^$", "a\r\n\r\nb\r\n"), Some(1), "CRLF line ends");
    assert_eq!(count("^$", "a\n"), Some(0));
    assert_eq!(count("^$", "\n"), Some(1), "one empty line");

    let positions: Vec<usize> = line_matches(&line_anchored("^"), "a\nb\n")
        .map(|found| found.start())
        .collect();
    assert_eq!(
        positions,
        [0, 2],
        "the page is built from the same matches as the count"
    );
}

/// A `\r` with no `\n` after it ends no line: it is hidden from the anchors, at the same offset,
/// unless the pattern itself asks for a `\r`.
#[test]
fn a_lone_carriage_return_is_not_a_line_end_for_the_anchors() {
    let anchored = line_anchored;
    let source = "a\rb\r\nc\n";
    let starts = |pattern: &str| {
        let re = anchored(pattern);
        let text = lines_end_at_lf(&re, source);
        assert_eq!(text.len(), source.len(), "offsets are those of the file");
        line_matches(&re, &text).map(|found| found.start()).collect::<Vec<_>>()
    };
    assert_eq!(starts("^b"), Vec::<usize>::new(), "`b` is in the middle of line 1");
    assert_eq!(starts("^."), [0, 5], "two lines start");
    assert_eq!(starts("$"), [3, 6], "and two end, the first before its `\\r\\n`");
    assert_eq!(starts("a.b"), [0], "the lone `\\r` is a character of its line");
    assert_eq!(starts("a\\rb"), [0], "a pattern that names `\\r` sees it");
    for spelling in [
        "a\\x0Db",
        "a\\x0db",
        "a\\x{0D}b",
        "a\\u000Db",
        "a[\\x0d]b",
        "a[\\r]b",
        "a[\\x0C-\\x0D]b",
    ] {
        assert_eq!(starts(spelling), [0], "`{spelling}` names `\\r` as well");
    }
    assert_eq!(
        starts("a[\\x00-\\x1F]b"),
        [0],
        "a range holding the stand-in still matches"
    );
    assert_eq!(starts("^b"), Vec::<usize>::new(), "and a pattern naming none is masked");

    // The stand-in is a character of its own: a pattern that tells it from `\r` is not masked.
    for (spelling, expected) in [
        ("a\\vb", Vec::<usize>::new()),
        ("a\\x0Bb", Vec::new()),
        ("a[\\x00-\\x0B]b", Vec::new()),
        ("a[\\s&&[^\\x0B]]b", vec![0]),
        ("a[^\\x00-\\x0B]b", vec![0]),
    ] {
        assert_eq!(starts(spelling), expected, "`{spelling}` sees the `\\r` as it is");
    }

    let re = anchored("^c");
    assert!(
        matches!(lines_end_at_lf(&re, "a\r\nc\n"), std::borrow::Cow::Borrowed(_)),
        "a file with only `\\r\\n` endings is searched as it is"
    );
}
