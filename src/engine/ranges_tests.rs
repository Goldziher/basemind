//! The range merger's rules, by example and as properties over arbitrary hit lists.

use proptest::prelude::*;

use super::*;

fn path(name: &str) -> RelPath {
    RelPath::from(name)
}

fn span(start: u32, end: u32) -> LineSpan {
    LineSpan { start, end }
}

fn range(start: u32, end: u32, hits: u32) -> ReadRange {
    ReadRange {
        span: span(start, end),
        hits,
    }
}

fn hits_in(name: &str, lines: &[u32]) -> Vec<(RelPath, u32)> {
    lines.iter().map(|&line| (path(name), line)).collect()
}

fn unlimited(context: u32) -> RangeOptions {
    RangeOptions {
        context,
        max_lines: None,
    }
}

#[test]
fn the_default_context_is_five_lines() {
    let merged = merge_hits(hits_in("a.rs", &[20]), |_| Some(100), RangeOptions::default());
    assert_eq!(merged.files[0].ranges, vec![range(15, 25, 1)]);
    assert_eq!(merged.lines, 11);
}

#[test]
fn overlapping_and_touching_windows_merge_and_a_gap_does_not() {
    // With two lines of context the windows of 10 and 13 overlap, the window of 18 touches
    // theirs, and the window of 24 starts after a gap of one line.
    let merged = merge_hits(hits_in("a.rs", &[18, 10, 24, 13]), |_| None, unlimited(2));
    assert_eq!(merged.files[0].ranges, vec![range(8, 20, 3), range(22, 26, 1)]);
    assert_eq!(merged.files[0].hits(), 4);
    assert!(!merged.truncated);
}

#[test]
fn windows_are_clamped_to_the_first_and_last_line() {
    let merged = merge_hits(hits_in("a.rs", &[1, 30]), |_| Some(31), unlimited(5));
    assert_eq!(merged.files[0].ranges, vec![range(1, 6, 1), range(25, 31, 1)]);
}

#[test]
fn a_hit_past_the_file_or_on_line_zero_is_counted_not_covered() {
    let merged = merge_hits(hits_in("a.rs", &[0, 3, 12]), |_| Some(10), unlimited(1));
    assert_eq!(merged.files[0].ranges, vec![range(2, 4, 1)]);
    assert_eq!(merged.out_of_file, 2);

    let gone = merge_hits(hits_in("b.rs", &[1]), |_| Some(0), unlimited(1));
    assert!(gone.files.is_empty());
    assert_eq!(gone.out_of_file, 1);
}

#[test]
fn files_keep_the_order_of_their_first_hit() {
    let hits = vec![(path("z.rs"), 5), (path("a.rs"), 5), (path("z.rs"), 50)];
    let merged = merge_hits(hits, |_| None, unlimited(0));
    let order: Vec<&RelPath> = merged.files.iter().map(|file| &file.path).collect();
    assert_eq!(order, vec![&path("z.rs"), &path("a.rs")]);
    assert_eq!(merged.files[0].ranges, vec![range(5, 5, 1), range(50, 50, 1)]);
}

#[test]
fn two_hits_on_one_line_count_twice_in_one_range() {
    let merged = merge_hits(hits_in("a.rs", &[7, 7]), |_| None, unlimited(0));
    assert_eq!(merged.files[0].ranges, vec![range(7, 7, 2)]);
}

#[test]
fn the_budget_keeps_whole_ranges_then_cuts_one_and_drops_the_rest() {
    // Ranges 1..=5 (hit 3), 11..=19 (hits 13, 17), then b.rs 1..=4 (hit 2); 5 + 4 lines fit.
    let hits = [hits_in("a.rs", &[3, 13, 17]), hits_in("b.rs", &[2])].concat();
    let options = RangeOptions {
        context: 2,
        max_lines: Some(9),
    };
    let merged = merge_hits(hits, |_| None, options);
    assert_eq!(merged.files.len(), 1);
    assert_eq!(merged.files[0].ranges, vec![range(1, 5, 1), range(11, 14, 1)]);
    assert_eq!(merged.lines, 9);
    assert!(merged.truncated);
    assert_eq!(merged.omitted_hits, 2);
}

#[test]
fn a_cut_that_would_cover_no_hit_drops_the_range() {
    let options = RangeOptions {
        context: 3,
        max_lines: Some(9),
    };
    // 1..=7 fits (7 lines); 17..=23 has 2 lines left, 17..=18, which reach no hit at 20.
    let merged = merge_hits(hits_in("a.rs", &[4, 20]), |_| None, options);
    assert_eq!(merged.files[0].ranges, vec![range(1, 7, 1)]);
    assert_eq!(merged.lines, 7);
    assert!(merged.truncated);
    assert_eq!(merged.omitted_hits, 1);
}

#[test]
fn a_budget_that_fits_everything_cuts_nothing() {
    let options = RangeOptions {
        context: 1,
        max_lines: Some(6),
    };
    let merged = merge_hits(hits_in("a.rs", &[2, 10]), |_| None, options);
    assert_eq!(merged.lines, 6);
    assert!(!merged.truncated);
    assert_eq!(merged.omitted_hits, 0);
}

#[test]
fn span_lists_parse_sorted_and_merged() {
    assert_eq!(
        parse_spans("310-322, 120-148,149,7").expect("parse"),
        vec![span(7, 7), span(120, 149), span(310, 322)]
    );
    assert_eq!(format_spans(&[span(7, 7), span(120, 149)]), "7,120-149");
}

#[test]
fn malformed_span_lists_are_refused_naming_the_shape() {
    for bad in ["", "1,,2", "0", "5-3", "a-b", "1-", "-4", "+3", "1-2-3", "99999999999"] {
        let error = parse_spans(bad).expect_err(bad).to_string();
        assert!(error.contains("separated by commas"), "{bad}: {error}");
    }
}

#[test]
fn a_span_clamps_to_the_file_and_one_past_the_end_is_none() {
    assert_eq!(span(5, 50).clamp(20), Some(span(5, 20)));
    assert_eq!(span(21, 30).clamp(20), None);
}

fn arb_hits() -> impl Strategy<Value = Vec<(u8, u32)>> {
    prop::collection::vec((0u8..4, 0u32..120), 0..60)
}

fn to_hits(raw: &[(u8, u32)]) -> Vec<(RelPath, u32)> {
    raw.iter()
        .map(|&(file, line)| (path(&format!("f{file}")), line))
        .collect()
}

/// File `fN` has `40 * (N + 1)` lines, so some hits fall past the end.
fn length(path: &RelPath) -> Option<u32> {
    let n: u32 = path.to_str_lossy()[1..].parse().expect("fN");
    Some(40 * (n + 1))
}

proptest! {
    #[test]
    fn ranges_cover_every_in_file_hit_and_nothing_beyond_its_window(raw in arb_hits(), context in 0u32..8) {
        let hits = to_hits(&raw);
        let merged = merge_hits(hits.clone(), length, unlimited(context));
        let in_file = |(p, line): &(RelPath, u32)| *line >= 1 && Some(*line) <= length(p);
        let valid = hits.iter().filter(|hit| in_file(hit)).count() as u32;
        prop_assert_eq!(merged.out_of_file, hits.len() as u32 - valid);
        prop_assert_eq!(merged.files.iter().map(FileRanges::hits).sum::<u32>(), valid);
        prop_assert!(!merged.truncated);

        for file in &merged.files {
            let last = length(&file.path).expect("known");
            let lines: Vec<u32> = hits.iter().filter(|h| h.0 == file.path && in_file(h)).map(|h| h.1).collect();
            for pair in file.ranges.windows(2) {
                prop_assert!(pair[1].span.start > pair[0].span.end + 1, "sorted, disjoint, not touching");
            }
            for r in &file.ranges {
                prop_assert!(r.span.start >= 1 && r.span.end <= last);
                let inside = lines.iter().filter(|&&l| l >= r.span.start && l <= r.span.end).count() as u32;
                prop_assert_eq!(inside, r.hits);
                for line in r.span.start..=r.span.end {
                    prop_assert!(lines.iter().any(|&h| h.abs_diff(line) <= context), "line {} has no hit near", line);
                }
            }
            for &line in &lines {
                prop_assert_eq!(file.ranges.iter().filter(|r| r.span.start <= line && line <= r.span.end).count(), 1);
            }
        }
    }

    #[test]
    fn a_budget_keeps_a_prefix_of_the_unlimited_ranges(raw in arb_hits(), context in 0u32..8, budget in 0u32..200) {
        let hits = to_hits(&raw);
        let full = merge_hits(hits.clone(), length, unlimited(context));
        let cut = merge_hits(hits, length, RangeOptions { context, max_lines: Some(budget) });
        prop_assert!(cut.lines <= budget);
        prop_assert_eq!(cut.out_of_file, full.out_of_file);
        let total: u32 = full.files.iter().map(FileRanges::hits).sum();
        let kept: u32 = cut.files.iter().map(FileRanges::hits).sum();
        prop_assert_eq!(kept + cut.omitted_hits, total);
        prop_assert_eq!(cut.truncated, full.lines > budget);

        let flat = |m: &MergedRanges| -> Vec<(RelPath, ReadRange)> {
            m.files.iter().flat_map(|f| f.ranges.iter().map(|r| (f.path.clone(), *r))).collect()
        };
        let (full_flat, cut_flat) = (flat(&full), flat(&cut));
        prop_assert!(cut_flat.len() <= full_flat.len());
        for (index, (p, r)) in cut_flat.iter().enumerate() {
            let (fp, fr) = &full_flat[index];
            prop_assert_eq!(p, fp);
            prop_assert_eq!(r.span.start, fr.span.start);
            if index + 1 < cut_flat.len() {
                prop_assert_eq!(r, fr, "only the last kept range may be cut");
            } else {
                prop_assert!(r.span.end <= fr.span.end && r.hits >= 1);
            }
        }
    }

    #[test]
    fn formatting_and_parsing_round_trip(raw in prop::collection::vec((1u32..500, 0u32..20), 1..20)) {
        let spans = merge_spans(raw.iter().map(|&(start, len)| span(start, start + len)).collect());
        prop_assert_eq!(parse_spans(&format_spans(&spans)).expect("parse"), spans);
    }
}
