//! Read ranges: hit lines grouped by file and widened into the fewest line spans that cover them,
//! so an agent reads each region around its hits once (the migration check's `ranges`).
//!
//! The contract later callers rely on:
//!
//! - Lines are 1-based and spans inclusive. A hit on line `n` with context `c` covers
//!   `max(1, n - c) ..= min(line_count, n + c)`.
//! - Within a file, spans are sorted, and two spans that overlap or touch (`next.start <=
//!   prev.end + 1`) are one span. Every line of a span is within `c` lines of one of its hits.
//! - Files keep the order of their first hit, so a ranked hit list keeps its ranking; hits within
//!   a file may arrive in any order.
//! - A hit on line 0 or past the file's known length is not covered by any span and is counted in
//!   `MergedRanges::out_of_file` (an index older than the file), never dropped silently.
//! - A line budget keeps whole spans in order until the next one does not fit; that one is cut to
//!   the lines left when the cut still covers one of its hits, and every later span is left out.
//!   A cut sets `MergedRanges::truncated` and counts what it left out in
//!   `MergedRanges::omitted_hits`. A span list carries no bytes.

#![cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the migration check reads only the merged spans; the line budget and the counts are the \
                  merger's tested contract"
    )
)]

use ahash::AHashMap;

use crate::path::RelPath;

/// Lines of context each side of a hit when the caller names none.
pub(crate) const DEFAULT_CONTEXT: u32 = 5;

/// A 1-based, inclusive span of lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineSpan {
    pub(crate) start: u32,
    pub(crate) end: u32,
}

impl LineSpan {
    /// The number of lines the span covers.
    pub(crate) fn lines(self) -> u32 {
        self.end - self.start + 1
    }
}

/// One merged span and the number of hits it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRange {
    pub(crate) span: LineSpan,
    pub(crate) hits: u32,
}

/// The ranges of one file, sorted by line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRanges {
    pub(crate) path: RelPath,
    pub(crate) ranges: Vec<ReadRange>,
}

impl FileRanges {
    /// Hits the file's ranges cover.
    pub(crate) fn hits(&self) -> u32 {
        self.ranges.iter().map(|range| range.hits).sum()
    }
}

/// How hits widen into ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeOptions {
    /// Lines of context each side of a hit.
    pub(crate) context: u32,
    /// The most lines all ranges together may cover; `None` is no limit.
    pub(crate) max_lines: Option<u32>,
}

impl Default for RangeOptions {
    fn default() -> Self {
        Self {
            context: DEFAULT_CONTEXT,
            max_lines: None,
        }
    }
}

/// The merged ranges of a hit list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergedRanges {
    /// Files with at least one range, in the order of their first hit.
    pub(crate) files: Vec<FileRanges>,
    /// Lines all ranges cover together.
    pub(crate) lines: u32,
    /// Hits on line 0 or past the end of their file.
    pub(crate) out_of_file: u32,
    /// The line budget cut the ranges.
    pub(crate) truncated: bool,
    /// Hits inside the file that no kept range covers because of the budget.
    pub(crate) omitted_hits: u32,
}

/// Groups `hits` (`(path, 1-based line)`) by file and merges them into read ranges.
/// `line_count` gives a file's length when it is known; an unknown length clamps nothing at the
/// end. See the module docs for the rules.
pub(crate) fn merge_hits(
    hits: impl IntoIterator<Item = (RelPath, u32)>,
    line_count: impl Fn(&RelPath) -> Option<u32>,
    options: RangeOptions,
) -> MergedRanges {
    let mut order: Vec<(RelPath, Vec<u32>)> = Vec::new();
    let mut slot: AHashMap<RelPath, usize> = AHashMap::new();
    for (path, line) in hits {
        let index = *slot.entry(path.clone()).or_insert_with(|| {
            order.push((path, Vec::new()));
            order.len() - 1
        });
        order[index].1.push(line);
    }

    let mut out = MergedRanges::default();
    let mut remaining = options.max_lines;
    for (path, mut lines) in order {
        let last_line = line_count(&path).unwrap_or(u32::MAX);
        let before = lines.len();
        lines.retain(|&line| line >= 1 && line <= last_line);
        out.out_of_file += count(before - lines.len());
        lines.sort_unstable();

        let mut ranges = Vec::new();
        for (span, hit_lines) in widen(&lines, options.context, last_line) {
            if out.truncated {
                out.omitted_hits += count(hit_lines.len());
                continue;
            }
            let kept = match remaining {
                Some(left) if span.lines() > left => {
                    out.truncated = true;
                    cut(span, hit_lines, left)
                }
                _ => Some(ReadRange {
                    span,
                    hits: count(hit_lines.len()),
                }),
            };
            let kept_hits = kept.map_or(0, |range| range.hits);
            out.omitted_hits += count(hit_lines.len()) - kept_hits;
            if let Some(range) = kept {
                out.lines += range.span.lines();
                if let Some(left) = remaining.as_mut() {
                    *left -= range.span.lines();
                }
                ranges.push(range);
            }
        }
        if !ranges.is_empty() {
            out.files.push(FileRanges { path, ranges });
        }
    }
    out
}

/// Widens sorted, in-file `lines` by `context` and merges spans that overlap or touch. Each span
/// comes with the hit lines it covers.
fn widen(lines: &[u32], context: u32, last_line: u32) -> Vec<(LineSpan, &[u32])> {
    let mut out: Vec<(LineSpan, &[u32])> = Vec::new();
    let mut first = 0;
    for (index, &line) in lines.iter().enumerate() {
        let span = LineSpan {
            start: line.saturating_sub(context).max(1),
            end: line.saturating_add(context).min(last_line),
        };
        match out.last_mut() {
            Some((last, covered)) if span.start <= last.end.saturating_add(1) => {
                last.end = last.end.max(span.end);
                *covered = &lines[first..=index];
            }
            _ => {
                first = index;
                out.push((span, &lines[index..=index]));
            }
        }
    }
    out
}

/// `span` cut to its first `left` lines, when that still covers one of `hit_lines`.
fn cut(span: LineSpan, hit_lines: &[u32], left: u32) -> Option<ReadRange> {
    if left == 0 {
        return None;
    }
    let end = span.start + left - 1;
    let hits = count(hit_lines.iter().filter(|&&line| line <= end).count());
    (hits > 0).then_some(ReadRange {
        span: LineSpan { start: span.start, end },
        hits,
    })
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `spans` as a list, `"a-b,c"`: comma-separated, a one-line span written as its line.
pub(crate) fn format_spans(spans: &[LineSpan]) -> String {
    let mut out = String::new();
    for (index, span) in spans.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if span.start == span.end {
            out.push_str(&span.start.to_string());
        } else {
            out.push_str(&format!("{}-{}", span.start, span.end));
        }
    }
    out
}

#[cfg(test)]
#[path = "ranges_tests.rs"]
mod tests;
