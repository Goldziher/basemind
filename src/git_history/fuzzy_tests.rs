use super::*;

#[test]
fn edit_distance_counts_insert_delete_replace_and_swap_as_one() {
    let cases: &[(&str, &str, usize, Option<usize>)] = &[
        ("config", "config", 2, Some(0)),
        ("confg", "config", 2, Some(1)),
        ("collectr", "collector", 2, Some(1)),
        ("confgi", "config", 2, Some(1)),
        ("conxig", "config", 2, Some(1)),
        ("cnfg", "config", 2, Some(2)),
        ("cnfg", "config", 1, None),
        ("bulk", "builder", 2, None),
        ("ab", "abcdef", 2, None),
        ("", "ab", 2, Some(2)),
    ];
    for &(a, b, max, expected) in cases {
        assert_eq!(
            edit_distance_within(a.as_bytes(), b.as_bytes(), max),
            expected,
            "{a} vs {b}"
        );
        assert_eq!(
            edit_distance_within(b.as_bytes(), a.as_bytes(), max),
            expected,
            "{b} vs {a}"
        );
    }
}

#[test]
fn a_term_tolerates_edits_by_length() {
    let cases = [(0, 0), (3, 0), (4, 1), (7, 1), (8, 2), (20, 2)];
    for (len, allowed) in cases {
        assert_eq!(edits_allowed(len), allowed, "len {len}");
    }
}

#[test]
fn subsequence_keeps_order_and_allows_gaps() {
    assert!(is_subsequence(b"nhrschfld", b"nhirschfeld"));
    assert!(is_subsequence(b"", b"abc"));
    assert!(!is_subsequence(b"ba", b"abc"));
    assert!(!is_subsequence(b"abcd", b"abc"));
}

#[test]
fn nearness_ranks_exact_then_prefix_and_edits_then_subsequence() {
    let last = Probe {
        term: "conf",
        last: true,
    };
    let inner = Probe {
        term: "conf",
        last: false,
    };
    let cases: &[(&Probe<'_>, &str, bool, Option<usize>)] = &[
        (&last, "conf", false, Some(0)),
        (&last, "config", false, Some(1)),
        (&inner, "config", false, None),
        (&inner, "confg", false, Some(1)),
        (&inner, "cnfg", false, None),
        (&inner, "cxxxonxf", true, Some(3)),
        (&inner, "cxxxonxf", false, None),
    ];
    for &(probe, term, subsequence, expected) in cases {
        assert_eq!(
            nearness(probe, term.as_bytes(), subsequence),
            expected,
            "{} vs {term}",
            probe.term
        );
    }
    let short = Probe { term: "co", last: true };
    assert_eq!(nearness(&short, b"config", false), None, "too short to complete");
}

#[test]
fn a_page_skips_then_takes_and_reports_when_full() {
    let mut page = Page::new(2, 2);
    let filled: Vec<bool> = (0..4u32).map(|ord| page.add(ord, ord % 2 == 0)).collect();
    assert_eq!(filled, vec![false, false, false, true]);
    assert_eq!(page.taken, vec![(2, true), (3, false)]);
}
