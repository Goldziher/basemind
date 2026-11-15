//! What a `code_grep` hit shows of its line and of the symbol it sits in.
//!
//! Split from `helpers_grep.rs` (the sweep) for the 1000-line cap.

use super::types::{GrepHit, GrepSymbol};
use crate::path::RelPath;

/// Longest line text a hit carries. A hit in a minified bundle would otherwise ship the bundle.
const LINE_TEXT_MAX: usize = 400;

/// Line `line_idx` of `source` without its line terminator, borrowed.
pub(super) fn line_text<'a>(source: &'a str, line_starts: &[usize], line_idx: usize) -> &'a str {
    let start = line_starts[line_idx];
    let end = line_starts.get(line_idx + 1).copied().unwrap_or(source.len());
    source[start..end].trim_end_matches('\n').trim_end_matches('\r')
}

/// `line` as a hit shows it: whole when it fits [`LINE_TEXT_MAX`], else a window of that many bytes
/// placed around the match at `match_start..match_start + match_len` (byte offsets in the line),
/// with `…` marking each cut side. A context line passes `0, 0` and keeps its head.
///
/// The window is centred on the match rather than anchored at the start of the line: a hit far into
/// a minified line would otherwise show 400 bytes that do not contain what was searched for.
pub(super) fn clip_line(line: &str, match_start: usize, match_len: usize) -> String {
    if line.len() <= LINE_TEXT_MAX {
        return line.to_owned();
    }
    let lead = LINE_TEXT_MAX.saturating_sub(match_len.min(LINE_TEXT_MAX)) / 2;
    let mut end = (match_start.saturating_sub(lead) + LINE_TEXT_MAX).min(line.len());
    let mut start = end.saturating_sub(LINE_TEXT_MAX);
    while !line.is_char_boundary(start) {
        start += 1;
    }
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = String::with_capacity(end - start + 2 * '…'.len_utf8());
    if start > 0 {
        text.push('…');
    }
    text.push_str(&line[start..end]);
    if end < line.len() {
        text.push('…');
    }
    text
}

/// Name the innermost symbol holding each hit. One outline read per file with hits on the page.
///
/// The outline's byte spans describe the file as it was indexed. A file edited since then gets no
/// annotation: its hits come from the bytes on disk, and a span laid over different bytes names the
/// wrong symbol.
pub(super) fn annotate_symbols(cache: &super::MapCache, hits: &mut [GrepHit]) {
    let mut current: Option<(RelPath, Option<std::sync::Arc<crate::extract::FileMapL1>>)> = None;
    for hit in hits {
        if current.as_ref().is_none_or(|(path, _)| *path != hit.path) {
            current = Some((hit.path.clone(), cache.get(&hit.path)));
        }
        let Some((_, Some(l1))) = current.as_ref() else {
            continue;
        };
        if hit.stale {
            continue;
        }
        hit.symbol = l1
            .symbols
            .iter()
            .filter(|symbol| symbol.start_byte <= hit.start_byte && hit.start_byte < symbol.end_byte)
            .min_by_key(|symbol| symbol.end_byte - symbol.start_byte)
            .map(|symbol| GrepSymbol {
                name: symbol.name.clone(),
                kind: super::helpers::kind_to_str(symbol.kind),
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_that_fits_is_returned_whole() {
        assert_eq!(clip_line("let x = 1;", 4, 1), "let x = 1;");
    }

    #[test]
    fn a_long_line_is_cut_to_a_window_around_the_match() {
        let line = format!("{}NEEDLE{}", "a".repeat(1_000), "b".repeat(1_000));
        let text = clip_line(&line, 1_000, 6);
        assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
        assert!(text.contains("NEEDLE"));
        assert_eq!(text.len(), LINE_TEXT_MAX + 2 * '…'.len_utf8());
        let before = text.trim_start_matches('…').find("NEEDLE").expect("needle");
        assert_eq!(
            before,
            (LINE_TEXT_MAX - 6) / 2,
            "the match sits in the middle of the window"
        );
    }

    #[test]
    fn a_match_near_either_end_keeps_that_end_unmarked() {
        let line = format!("NEEDLE{}", "b".repeat(1_000));
        let head = clip_line(&line, 0, 6);
        assert!(head.starts_with("NEEDLE") && head.ends_with('…'), "{head}");
        let line = format!("{}NEEDLE", "a".repeat(1_000));
        let tail = clip_line(&line, 1_000, 6);
        assert!(tail.starts_with('…') && tail.ends_with("NEEDLE"), "{tail}");
    }

    #[test]
    fn a_context_line_keeps_its_head_and_a_cut_never_splits_a_character() {
        let line = "é".repeat(600);
        let text = clip_line(&line, 0, 0);
        assert!(text.ends_with('…') && !text.starts_with('…'));
        assert!(text.len() <= LINE_TEXT_MAX + '…'.len_utf8());
        let middle = clip_line(&line, 601, 2);
        assert!(middle.starts_with('…') && middle.ends_with('…'), "{middle}");
    }

    #[test]
    fn line_text_drops_the_terminator() {
        let source = "one\r\ntwo\nthree";
        let starts = [0, 5, 9];
        assert_eq!(line_text(source, &starts, 0), "one");
        assert_eq!(line_text(source, &starts, 1), "two");
        assert_eq!(line_text(source, &starts, 2), "three");
    }
}
