use super::*;

/// 2025-06-15T12:00:00Z.
const NOW: i64 = 1_749_988_800;

#[test]
fn a_date_bound_is_that_day_in_utc() {
    let cases = [
        ("1970-01-01", false, 0),
        ("1970-01-01", true, 86_399),
        ("2024-02-29", false, 1_709_164_800),
        ("2024-02-29", true, 1_709_164_800 + 86_399),
        ("2025-06-15", false, 1_749_945_600),
    ];
    for (text, end_of_day, expected) in cases {
        assert_eq!(parse_bound("since", text, NOW, end_of_day).unwrap(), expected, "{text}");
    }
}

#[test]
fn a_relative_bound_counts_back_from_now() {
    let cases = [
        ("0d", 0),
        ("1d", 86_400),
        ("2w", 14 * 86_400),
        ("3m", 90 * 86_400),
        ("1y", 365 * 86_400),
    ];
    for (text, span) in cases {
        for end_of_day in [false, true] {
            assert_eq!(
                parse_bound("until", text, NOW, end_of_day).unwrap(),
                NOW - span,
                "{text}"
            );
        }
    }
}

#[test]
fn a_malformed_bound_is_refused_naming_the_argument_and_the_forms() {
    for text in [
        "",
        "yesterday",
        "2025-13-01",
        "2025-02-30",
        "25-01-01",
        "2025-1-1",
        "-5d",
        "5",
        "d",
        "5x",
        "5é",
    ] {
        let refusal = parse_bound("since", text, NOW, false).expect_err(text).to_string();
        assert!(
            refusal.starts_with("`git_log`: `since` must be `YYYY-MM-DD`, or a count of days"),
            "{text}: {refusal}"
        );
    }
}

#[test]
fn a_range_ending_before_it_starts_is_refused() {
    let range = TimeRange::parse(Some("2025-02-01"), Some("2025-01-31"), NOW).expect_err("inverted");
    assert_eq!(
        range.to_string(),
        "`git_log`: `since` (2025-02-01) is after `until` (2025-01-31); no commit can be in that range"
    );
    let range = TimeRange::parse(Some("2025-01-31"), Some("2025-01-31"), NOW).expect("one day");
    assert!(range.contains(1_738_281_600), "its first second");
    assert!(range.contains(1_738_367_999), "its last second");
    assert!(!range.contains(1_738_368_000), "the next day");
    assert!(!range.contains(1_738_281_599), "the day before");
}

#[test]
fn civil_dates_round_trip_through_days() {
    for days in [-1_000_000, -1, 0, 59, 60, 11_016, 19_000, 2_932_896] {
        let (year, month, day) = civil_from_days(days);
        assert_eq!(days_from_civil(year, month, day), days);
    }
}

fn commit(files: &[&str]) -> CommitInfo {
    CommitInfo {
        sha: "0".repeat(40),
        short_sha: "0".repeat(7),
        summary: "tune the Bulk Config loader".to_string(),
        author: "Ada Lovelace".to_string(),
        author_email: "ada@calc.example".to_string(),
        author_time_unix: 100,
        body: String::new(),
        files: files
            .iter()
            .map(|file| (RelPath::from(*file), crate::git::ChangeKind::Modified))
            .collect(),
    }
}

#[test]
fn the_live_walk_applies_every_filter_as_typed() {
    let params = |pattern: &str, author: Option<&str>, path: Option<&str>| SearchGitHistoryParams {
        pattern: pattern.to_string(),
        field: Some("message".to_string()),
        limit: None,
        cursor: None,
        max_tokens: None,
        author: author.map(str::to_string),
        path: path.map(RelPath::from),
        since: None,
        until: None,
    };
    let commit = commit(&["src/config/loader.rs"]);
    let cases = [
        (params("bulk config", None, None), true),
        (params("BulkConfig", None, None), true),
        (params("bulk confg", None, None), false),
        (params("bulk", Some("lovelace"), None), true),
        (params("bulk", Some("babbage"), None), false),
        (params("bulk", None, Some("src/config/loader.rs")), true),
        (params("bulk", None, Some("src/config/")), true),
        (params("bulk", None, Some("src/other/")), false),
        (params("bulk", None, Some("src/config")), false),
    ];
    for (params, expected) in cases {
        let live = LiveQuery::new(&params, FtsScope::Message, TimeRange::default());
        assert_eq!(live.matches(&commit), expected, "{params:?}");
    }
    let before = TimeRange {
        since: None,
        until: Some(99),
    };
    let live = LiveQuery::new(&params("bulk", None, None), FtsScope::Message, before);
    assert!(!live.matches(&commit), "authored after the range ends");
}
