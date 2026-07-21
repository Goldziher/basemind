//! Lightweight markdown-to-terminal rendering for the transcript.
//!
//! Turns a message body into styled [`Line`]s, honoring embedded newlines (the core fix — a ratatui
//! [`Line`] does not break on `\n`, and [`Wrap`](ratatui::widgets::Wrap) only wraps on width) plus a
//! small, hand-rolled markdown subset: fenced code blocks, ATX headings, bullet lists, and inline
//! `**bold**` / `` `code` ``. No markdown crate — the grammar is deliberately tiny and single-pass,
//! and every span owns its string so the returned lines are `'static`.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Foreground for code (fenced blocks and inline `` `code` ``), readable on a dark terminal.
const CODE_FG: Color = Color::Rgb(0xC8, 0xC8, 0xA0);

/// The fence marker that opens and closes a code block.
const FENCE: &str = "```";

/// Maximum `#` count for an ATX heading.
const MAX_HEADING_LEVEL: usize = 6;

/// Render a message body into styled terminal lines, honoring `\n` and lightweight markdown.
///
/// Each source line becomes one [`Line`]; a blank source line becomes an empty [`Line`]. Fenced code
/// blocks are rendered verbatim (no inline parsing) with a distinct code style, and their fence lines
/// are dropped. An unclosed fence renders the remaining lines as code to end-of-string.
pub fn render_markdown(source: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in source.split('\n') {
        if is_fence(raw) {
            in_code = !in_code;
            continue;
        }
        if in_code {
            lines.push(Line::from(code_span(raw)));
        } else {
            lines.push(render_text_line(raw));
        }
    }
    lines
}

/// Whether `raw` opens or closes a fenced code block (```` ``` ```` optionally with a language).
fn is_fence(raw: &str) -> bool {
    raw.starts_with(FENCE)
}

/// Render one non-code source line: heading, bullet, or inline-styled text.
fn render_text_line(raw: &str) -> Line<'static> {
    if let Some(text) = heading_text(raw) {
        return Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some((indent, content)) = bullet_parts(raw) {
        let mut spans = vec![Span::raw(format!("{indent}• "))];
        spans.extend(inline_spans(content));
        return Line::from(spans);
    }
    Line::from(inline_spans(raw))
}

/// The text of an ATX heading (`# ` … `###### `), with the markers and one space stripped.
fn heading_text(raw: &str) -> Option<&str> {
    let hashes = raw.bytes().take_while(|&byte| byte == b'#').count();
    if (1..=MAX_HEADING_LEVEL).contains(&hashes) && raw.as_bytes().get(hashes) == Some(&b' ') {
        Some(&raw[hashes + 1..])
    } else {
        None
    }
}

/// Split a bullet line into `(leading indent, content)` when it starts with `- ` or `* `.
fn bullet_parts(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim_start();
    let indent = &raw[..raw.len() - trimmed.len()];
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .map(|content| (indent, content))
}

/// A verbatim code span (no inline parsing) styled for a code block or inline code.
fn code_span(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(CODE_FG))
}

/// Parse inline `**bold**` and `` `code` `` markers in a single pass.
///
/// Unmatched or odd markers are emitted literally — the parser never drops text and never panics.
/// Returns an empty vector for empty input so a blank source line renders as an empty [`Line`].
fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut plain_start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if let Some((span, next)) = try_marker(text, bytes, index) {
            push_plain(text, plain_start, index, &mut spans);
            spans.push(span);
            index = next;
            plain_start = index;
        } else {
            index += 1;
        }
    }
    push_plain(text, plain_start, bytes.len(), &mut spans);
    spans
}

/// Try to parse a `**bold**` or `` `code` `` marker opening at `index`.
///
/// Returns the styled span and the index just past the closing marker, or `None` when `index` is not
/// a marker or the marker is never closed (so the caller emits the character literally).
fn try_marker(text: &str, bytes: &[u8], index: usize) -> Option<(Span<'static>, usize)> {
    if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'*') {
        let end = find_double_star(bytes, index + 2)?;
        let span = Span::styled(
            text[index + 2..end].to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        );
        return Some((span, end + 2));
    }
    if bytes[index] == b'`' {
        let end = find_byte(bytes, index + 1, b'`')?;
        return Some((code_span(&text[index + 1..end]), end + 1));
    }
    None
}

/// Push `text[start..end]` as a plain span when the range is non-empty.
fn push_plain(text: &str, start: usize, end: usize, spans: &mut Vec<Span<'static>>) {
    if end > start {
        spans.push(Span::raw(text[start..end].to_string()));
    }
}

/// Find the next `**` pair at or after `start`, returning the index of its first `*`.
fn find_double_star(bytes: &[u8], start: usize) -> Option<usize> {
    (start..bytes.len().saturating_sub(1)).find(|&j| bytes[j] == b'*' && bytes[j + 1] == b'*')
}

/// Find the next occurrence of `needle` at or after `start`.
fn find_byte(bytes: &[u8], start: usize, needle: u8) -> Option<usize> {
    (start..bytes.len()).find(|&j| bytes[j] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate the visible text of a line's spans.
    fn visible(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn three_source_lines_yield_three_lines() {
        let lines = render_markdown("a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(visible(&lines[0]), "a");
        assert_eq!(visible(&lines[2]), "c");
    }

    #[test]
    fn blank_middle_line_yields_empty_line() {
        let lines = render_markdown("a\n\nb");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].spans.is_empty(), "blank line must have no spans");
    }

    #[test]
    fn trailing_newline_yields_trailing_empty_line() {
        let lines = render_markdown("a\n");
        assert_eq!(lines.len(), 2);
        assert!(lines[1].spans.is_empty());
    }

    #[test]
    fn fenced_block_excludes_fences_and_styles_code() {
        let lines = render_markdown("```rust\nlet x = 1;\nlet y = 2;\n```");
        assert_eq!(lines.len(), 2, "fence lines are dropped");
        for (line, expected) in lines.iter().zip(["let x = 1;", "let y = 2;"]) {
            assert_eq!(line.spans.len(), 1);
            assert_eq!(line.spans[0].content.as_ref(), expected);
            assert_eq!(line.spans[0].style.fg, Some(CODE_FG));
        }
    }

    #[test]
    fn fenced_block_does_not_parse_inline_markup() {
        let lines = render_markdown("```\n**not bold** `not code`\n```");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "**not bold** `not code`");
        assert_eq!(lines[0].spans[0].style.fg, Some(CODE_FG));
    }

    #[test]
    fn unclosed_fence_renders_rest_as_code() {
        let lines = render_markdown("intro\n```\ncode line");
        assert_eq!(lines.len(), 2);
        assert_eq!(visible(&lines[0]), "intro");
        assert_eq!(lines[1].spans[0].style.fg, Some(CODE_FG));
        assert_eq!(lines[1].spans[0].content.as_ref(), "code line");
    }

    #[test]
    fn heading_strips_hashes_and_is_bold_cyan() {
        let lines = render_markdown("# Title");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        let span = &lines[0].spans[0];
        assert_eq!(span.content.as_ref(), "Title");
        assert_eq!(span.style.fg, Some(Color::Cyan));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn six_hashes_are_a_heading_but_seven_are_not() {
        assert_eq!(visible(&render_markdown("###### Deep")[0]), "Deep");
        assert_eq!(visible(&render_markdown("####### Nope")[0]), "####### Nope");
    }

    #[test]
    fn hash_without_space_is_not_a_heading() {
        assert_eq!(visible(&render_markdown("#nospace")[0]), "#nospace");
    }

    #[test]
    fn bullet_line_gets_marker_and_preserves_indent() {
        let lines = render_markdown("  - item");
        assert_eq!(lines.len(), 1);
        assert_eq!(visible(&lines[0]), "  • item");
    }

    #[test]
    fn star_bullet_is_recognized() {
        assert_eq!(visible(&render_markdown("* item")[0]), "• item");
    }

    #[test]
    fn bold_inline_yields_bold_first_span() {
        let lines = render_markdown("**hi** there");
        assert!(lines[0].spans.len() >= 2);
        let first = &lines[0].spans[0];
        assert_eq!(first.content.as_ref(), "hi");
        assert!(first.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(visible(&lines[0]), "hi there");
    }

    #[test]
    fn inline_code_span_is_styled() {
        let lines = render_markdown("a `b` c");
        let code = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "b")
            .expect("code span present");
        assert_eq!(code.style.fg, Some(CODE_FG));
        assert_eq!(visible(&lines[0]), "a b c");
    }

    #[test]
    fn unmatched_bold_renders_literally_without_panic() {
        let lines = render_markdown("**bold");
        assert_eq!(lines.len(), 1);
        assert_eq!(visible(&lines[0]), "**bold");
    }

    #[test]
    fn unmatched_backtick_renders_literally() {
        assert_eq!(visible(&render_markdown("a `b c")[0]), "a `b c");
    }

    #[test]
    fn multibyte_text_is_not_corrupted() {
        let lines = render_markdown("café **crème** ☕");
        assert_eq!(visible(&lines[0]), "café crème ☕");
    }

    #[test]
    fn empty_source_yields_one_empty_line() {
        let lines = render_markdown("");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.is_empty());
    }
}
