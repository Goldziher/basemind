//! The hotspot ranking: the window walk, the top-slice thresholds, the coincidence rule and the
//! function length.

use super::*;
use crate::git::ChangeKind;

fn commit(time: i64, email: &str, files: &[&str]) -> CommitInfo {
    CommitInfo {
        sha: String::new(),
        short_sha: String::new(),
        summary: String::new(),
        author: "name".to_string(),
        author_email: email.to_string(),
        author_time_unix: time,
        body: String::new(),
        files: files
            .iter()
            .map(|path| (RelPath::from(*path), ChangeKind::Modified))
            .collect(),
    }
}

fn signals(commits: u32, lines: u32) -> Signals {
    Signals {
        commits,
        authors: 1,
        lines,
        ..Signals::default()
    }
}

#[test]
fn since_takes_days_weeks_and_years_only() {
    assert_eq!(parse_since("90d").unwrap(), 90 * SECS_PER_DAY);
    assert_eq!(parse_since("2w").unwrap(), 14 * SECS_PER_DAY);
    assert_eq!(parse_since("1y").unwrap(), 365 * SECS_PER_DAY);
    for bad in ["", "d", "0d", "-3d", "6m", "90", "1.5w", "9999999999999999y"] {
        let error = parse_since(bad).expect_err(bad).to_string();
        assert!(error.contains("`90d`, `12w` or `1y`"), "{bad}: {error}");
    }
}

/// A commit's commit time when it is its author time.
fn committed_when_authored(commit: &CommitInfo) -> Option<i64> {
    Some(commit.author_time_unix)
}

#[test]
fn the_window_ends_at_the_given_end_and_counts_commits_and_authors_per_path() {
    let day = SECS_PER_DAY;
    let history = [
        commit(100 * day, "A@x", &["a.rs", "b.rs"]),
        commit(99 * day, "a@x", &["a.rs"]),
        commit(95 * day, "b@x", &["a.rs"]),
        // Older than the 7-day window: not counted.
        commit(50 * day, "c@x", &["a.rs", "c.rs"]),
    ];
    let window = collect_window(
        |skip, take| history.iter().skip(skip).take(take).cloned().collect(),
        committed_when_authored,
        101 * day,
        7 * day,
        INDEXED_COMMIT_CAP,
    );
    assert_eq!((window.start, window.end), (94 * day, 101 * day));
    assert_eq!(window.commits, 3);
    let (commits, authors) = &window.per_path[&RelPath::from("a.rs")];
    // `A@x` and `a@x` are one author.
    assert_eq!((*commits, authors.len()), (3, 2));
    assert_eq!(window.per_path[&RelPath::from("b.rs")].0, 1);
    assert!(!window.per_path.contains_key(&RelPath::from("c.rs")));
    assert_eq!(window.truncated_reason, None);
}

#[test]
fn one_backdated_commit_does_not_end_the_window_but_a_run_of_them_does() {
    let day = SECS_PER_DAY;
    let mut history = vec![commit(100 * day, "a@x", &["a.rs"]), commit(1, "a@x", &["old.rs"])];
    history.push(commit(99 * day, "a@x", &["late.rs"]));
    history.extend((0..OUT_OF_WINDOW_RUN).map(|_| commit(1, "a@x", &["old.rs"])));
    history.push(commit(98 * day, "a@x", &["beyond.rs"]));
    let window = collect_window(
        |skip, take| history.iter().skip(skip).take(take).cloned().collect(),
        committed_when_authored,
        100 * day,
        7 * day,
        INDEXED_COMMIT_CAP,
    );
    assert!(window.per_path.contains_key(&RelPath::from("late.rs")));
    assert!(!window.per_path.contains_key(&RelPath::from("beyond.rs")));
    assert!(!window.per_path.contains_key(&RelPath::from("old.rs")));
}

#[test]
fn a_run_of_commits_authored_before_the_window_but_made_in_it_does_not_end_the_walk() {
    let day = SECS_PER_DAY;
    let mut rebased = commit(1, "a@x", &["rebased.rs"]);
    rebased.sha = "rebased".to_string();
    let mut history = vec![commit(100 * day, "a@x", &["head.rs"])];
    history.extend(std::iter::repeat_n(rebased, OUT_OF_WINDOW_RUN + 10));
    history.push(commit(98 * day, "b@x", &["beyond.rs"]));
    let window = collect_window(
        |skip, take| history.iter().skip(skip).take(take).cloned().collect(),
        |commit: &CommitInfo| {
            Some(if commit.sha == "rebased" {
                99 * day
            } else {
                commit.author_time_unix
            })
        },
        100 * day,
        7 * day,
        INDEXED_COMMIT_CAP,
    );
    assert_eq!(window.commits, 2);
    let mut paths: Vec<&str> = window.per_path.keys().map(|path| path.as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["beyond.rs", "head.rs"]);
    assert_eq!(window.truncated_reason, None);
}

#[test]
fn the_walk_stops_at_its_cap_and_says_so() {
    let history: Vec<CommitInfo> = (0..30).map(|i| commit(1000 - i, "a@x", &["a.rs"])).collect();
    let window = collect_window(
        |skip, take| history.iter().skip(skip).take(take).cloned().collect(),
        committed_when_authored,
        1000,
        SECS_PER_DAY,
        10,
    );
    assert_eq!(window.commits, 10);
    assert_eq!(window.truncated_reason, Some("commit_cap"));
}

#[test]
fn a_threshold_is_the_smallest_value_held_by_at_most_the_slice() {
    // Twenty files at ten percent: the top two.
    assert_eq!(threshold((1..=20).rev(), 10), Some(19));
    // 3 files: the slice is at least one file.
    assert_eq!(threshold([5, 9, 1].into_iter(), 10), Some(9));
    // A tie that overflows the slice is out whole: the value above it is the cut.
    assert_eq!(threshold([9, 7, 7, 7, 1, 1, 1, 1, 1, 1, 1].into_iter(), 20), Some(9));
    assert_eq!(threshold([7, 7, 7, 1, 1, 1, 1, 1, 1, 1, 1].into_iter(), 10), None);
    assert_eq!(threshold([2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1].into_iter(), 10), Some(2));
    assert_eq!(threshold([1, 1, 1, 1].into_iter(), 10), None);
    assert_eq!(threshold([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].into_iter(), 50), Some(3));
    // A zero is never in a top slice.
    assert_eq!(threshold([0, 0, 0].into_iter(), 10), None);
    assert_eq!(threshold([4].into_iter(), 10), Some(4));
    assert_eq!(threshold(std::iter::empty(), 10), None);
}

#[test]
fn a_file_is_reported_when_two_signals_coincide_sorted_by_how_many() {
    let mut files: Vec<(RelPath, Signals)> = (0..17)
        .map(|i| (RelPath::from(format!("small{i:02}.rs")), signals(1, 10)))
        .collect();
    // Churned and large: commits, lines.
    files.push((RelPath::from("hot.rs"), signals(9, 900)));
    // Churned, many authors and the longest function: three signals.
    let mut hottest = signals(8, 800);
    hottest.authors = 5;
    hottest.longest_function = Some(200);
    hottest.longest_function_at = Some("big:3".to_string());
    files.push((RelPath::from("hottest.rs"), hottest));
    // Large only: one signal is not a hotspot.
    files.push((RelPath::from("large.rs"), signals(1, 5000)));

    // 20 files: each slice is the top 2.
    let (thresholds, rows) = rank(files, 10);
    assert_eq!(thresholds.commits, Some(8));
    assert_eq!(thresholds.authors, Some(5));
    assert_eq!(thresholds.lines, Some(900));
    assert_eq!(thresholds.longest_function, Some(200));
    assert_eq!((thresholds.symbols, thresholds.importers), (None, None));
    let got: Vec<(&str, Vec<&str>)> = rows
        .iter()
        .map(|row| (row.path.as_str().unwrap(), row.signals.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("hottest.rs", vec!["commits", "authors", "longest_function"]),
            ("hot.rs", vec!["commits", "lines"]),
        ]
    );
    assert_eq!(rows[0].longest_function_at.as_deref(), Some("big:3"));
}

#[test]
fn scope_is_the_path_or_a_directory_above_it() {
    let path = RelPath::from("src/engine/git/log.rs");
    assert!(in_scope(&path, None));
    assert!(in_scope(&path, Some(b"src/engine")));
    assert!(in_scope(&path, Some(b"src/engine/git/log.rs")));
    assert!(!in_scope(&path, Some(b"src/eng")));
    assert!(!in_scope(&path, Some(b"tests")));
}

#[test]
fn the_longest_function_is_measured_in_lines_from_its_byte_span() {
    let source = b"fn a() {}\nfn b() {\n  x();\n  y();\n}\nstruct S {\n\n\n\n}\n";
    let symbol = |name: &str, kind, start: usize, end: usize| crate::extract::Symbol {
        name: name.to_string(),
        kind,
        start_byte: start as u32,
        end_byte: end as u32,
        start_row: 0,
        start_col: 0,
        signature: None,
        decorators: Vec::new(),
    };
    let b_start = 10;
    let b_end = b_start + 24;
    let symbols = [
        symbol("a", SymbolKind::Function, 0, 9),
        symbol("b", SymbolKind::Method, b_start, b_end),
        // A longer struct is not a function.
        symbol("S", SymbolKind::Struct, b_end + 1, source.len() - 1),
    ];
    assert_eq!(&source[b_start..b_end], b"fn b() {\n  x();\n  y();\n}");
    assert_eq!(longest_function(&symbols, source), Some((4, "b:2".to_string())));
    assert_eq!(longest_function(&symbols[2..], source), None);
}
