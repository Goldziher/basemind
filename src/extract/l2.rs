use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryMatch};

use super::{Call, DocComment, ExtractError, FileMapL2, SCHEMA_VER, capture_name};
use crate::lang::{LangId, ParseOutcome, QueryKind, parse_with_default_timeout, try_get_query, with_parser};

pub fn extract_l2(lang: LangId, source: &[u8]) -> Result<FileMapL2, ExtractError> {
    let outcome = with_parser(lang, |p| parse_with_default_timeout(p, source))?;
    let tree = match outcome {
        ParseOutcome::Ok(t) => t,
        ParseOutcome::Failed => return Err(ExtractError::ParseFailure),
        ParseOutcome::TimedOut => {
            return Err(ExtractError::ParseTimeout(crate::lang::DEFAULT_PARSE_TIMEOUT));
        }
    };
    extract_l2_from_tree(lang, &tree, source)
}

/// Extract L2 data (calls + docs) from a pre-parsed tree-sitter `Tree`. Separated from
/// `extract_l2` so the scanner can share one parse between L1 and L2 when eager L2 is
/// enabled, avoiding a second full parse per file on the hot path.
pub(crate) fn extract_l2_from_tree(
    lang: LangId,
    tree: &tree_sitter::Tree,
    source: &[u8],
) -> Result<FileMapL2, ExtractError> {
    let root = tree.root_node();

    let mut calls = run_calls(lang, root, source)?;
    if lang == "markdown" {
        calls.extend(markdown_references(source));
    }
    let docs = run_docs(lang, root, source)?;

    Ok(FileMapL2 {
        schema_ver: SCHEMA_VER,
        language: lang.to_string(),
        calls,
        docs,
    })
}

fn run_calls(lang: LangId, root: tree_sitter::Node, source: &[u8]) -> Result<Vec<Call>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Calls)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    crate::lang::with_query_cursor(|cursor| {
        let mut iter = cursor.matches(&q, root, source);
        while let Some(m) = iter.next() {
            if let Some(call) = build_call(&q, m, source) {
                out.push(call);
            }
        }
    });
    Ok(out)
}

fn run_docs(lang: LangId, root: tree_sitter::Node, source: &[u8]) -> Result<Vec<DocComment>, ExtractError> {
    let Some(q) = try_get_query(lang, QueryKind::Docs)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    crate::lang::with_query_cursor(|cursor| {
        let mut iter = cursor.matches(&q, root, source);
        while let Some(m) = iter.next() {
            if let Some(doc) = build_doc(&q, m, source) {
                out.push(doc);
            }
        }
    });
    Ok(out)
}

fn build_call(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<Call> {
    let mut callee: Option<String> = None;
    let mut range_node = None;
    for cap in m.captures {
        let cname = capture_name(q, cap.index);
        match cname {
            "call.callee" => {
                callee = cap.node.utf8_text(source).ok().map(|s| s.to_string());
            }
            "call.range" => range_node = Some(cap.node),
            _ => {}
        }
    }
    let node = range_node?;
    let pos = node.start_position();
    Some(Call {
        callee: callee?,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
        start_row: pos.row as u32,
        start_col: pos.column as u32,
    })
}

fn build_doc(q: &Query, m: &QueryMatch, source: &[u8]) -> Option<DocComment> {
    for cap in m.captures {
        if capture_name(q, cap.index) == "doc.text" {
            let node = cap.node;
            let text = node.utf8_text(source).ok()?.to_string();
            return Some(DocComment {
                text,
                start_byte: node.start_byte() as u32,
                end_byte: node.end_byte() as u32,
            });
        }
    }
    None
}

/// Harvest Obsidian's reference graph from a Markdown file's raw bytes as [`Call`]s. The
/// tree-sitter-markdown grammar does not model any of these — wikilinks, `#tags`, and the inline
/// half of standard links live inside opaque `inline` nodes — so we scan the bytes line by line.
/// Three reference kinds are surfaced, all as `Call`s whose `callee` a `find_references` query can
/// match:
///
/// - **Wikilinks** `[[Note]]`, `[[Note#Heading|alias]]`, `![[Embed]]` → callee is the note name
///   (the `#anchor` and `|alias` stripped), matching how Obsidian resolves a link by name.
/// - **Standard links** `[text](Note.md)`, `![alt](img/Diagram.png)` → callee is the destination's
///   note/file name (directory, `.md` extension, and `#anchor` stripped, `%20` decoded), so a vault
///   using Markdown links shares one backlink graph with wikilink vaults. External URLs
///   (`http(s)://`, `mailto:`) and bare `#anchor` links are skipped.
/// - **Tags** `#tag`, nested `#area/sub` → callee is the tag *with* its leading `#`, so
///   `find_references "#project"` lists every note carrying that tag. Inline tags plus YAML
///   frontmatter `tags:` (inline `[a, b]` and block `- a` list forms) are both harvested.
///
/// The scan is fence-aware: fenced code blocks (```` ``` ````/`~~~`) are skipped so code such as
/// `#include` or `[foo](bar)` in a snippet does not pollute the graph. It does not descend into
/// inline-code spans, so a `#hex` inside backticks on a prose line can still be captured — rare and
/// acceptable. `memchr` keeps line splitting linear.
pub(crate) fn markdown_references(source: &[u8]) -> Vec<Call> {
    let mut out = Vec::new();
    let mut row = 0u32;
    let mut line_start = 0usize;
    let mut in_fence = false;
    let mut fence_char = 0u8;
    let mut in_frontmatter = source.starts_with(b"---\n") || source.starts_with(b"---\r\n");
    let mut in_tags_block = false;
    let mut first_line = true;

    let len = source.len();
    loop {
        let nl = memchr::memchr(b'\n', &source[line_start..])
            .map(|n| line_start + n)
            .unwrap_or(len);
        let mut line_end = nl;
        if line_end > line_start && source[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let line = &source[line_start..line_end];

        if in_frontmatter {
            if !first_line && (line == b"---" || line == b"...") {
                in_frontmatter = false;
            } else if !first_line {
                scan_frontmatter_line(line, line_start, row, &mut in_tags_block, &mut out);
            }
        } else {
            let trimmed = trim_ascii_start(line);
            if trimmed.starts_with(b"```") || trimmed.starts_with(b"~~~") {
                let marker = trimmed[0];
                if !in_fence {
                    in_fence = true;
                    fence_char = marker;
                } else if marker == fence_char {
                    in_fence = false;
                }
            } else if !in_fence {
                scan_body_line(line, line_start, row, &mut out);
            }
        }

        first_line = false;
        if nl == len {
            break;
        }
        row += 1;
        line_start = nl + 1;
    }
    out
}

fn trim_ascii_start(line: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
        i += 1;
    }
    &line[i..]
}

/// Scan one non-fenced body line for wikilinks, standard note links, and inline tags.
fn scan_body_line(line: &[u8], line_start: usize, row: u32, out: &mut Vec<Call>) {
    scan_wikilinks(line, line_start, row, out);
    scan_standard_links(line, line_start, row, out);
    scan_inline_tags(line, line_start, row, out);
}

fn push_call(out: &mut Vec<Call>, callee: String, line_start: usize, col: usize, span: usize, row: u32) {
    let start = line_start + col;
    out.push(Call {
        callee,
        start_byte: start as u32,
        end_byte: (start + span) as u32,
        start_row: row,
        start_col: col as u32,
    });
}

fn scan_wikilinks(line: &[u8], line_start: usize, row: u32, out: &mut Vec<Call>) {
    use memchr::memmem;
    let mut i = 0;
    while let Some(rel) = memmem::find(&line[i..], b"[[") {
        let open = i + rel;
        let inner = open + 2;
        let Some(close_rel) = memmem::find(&line[inner..], b"]]") else {
            break;
        };
        let close = inner + close_rel;
        if let Some(target) = wikilink_target(&line[inner..close]) {
            push_call(out, target, line_start, open, close + 2 - open, row);
        }
        i = close + 2;
    }
}

fn scan_standard_links(line: &[u8], line_start: usize, row: u32, out: &mut Vec<Call>) {
    use memchr::memmem;
    let mut i = 0;
    while let Some(rel) = memmem::find(&line[i..], b"](") {
        let mid = i + rel;
        let dest_start = mid + 2;
        let Some(close_rel) = memchr::memchr(b')', &line[dest_start..]) else {
            break;
        };
        let dest_end = dest_start + close_rel;
        if let Some(target) = mdlink_target(&line[dest_start..dest_end]) {
            push_call(out, target, line_start, dest_start, dest_end - dest_start, row);
        }
        i = dest_end + 1;
    }
}

fn scan_inline_tags(line: &[u8], line_start: usize, row: u32, out: &mut Vec<Call>) {
    let mut i = 0;
    while i < line.len() {
        if line[i] != b'#' {
            i += 1;
            continue;
        }
        let ok_prefix = i == 0
            || matches!(
                line[i - 1],
                b' ' | b'\t' | b'(' | b'[' | b'{' | b'>' | b'*' | b'_' | b'~' | b'"' | b'\''
            );
        let link_anchor = i >= 2 && line[i - 1] == b'(' && line[i - 2] == b']';
        if !ok_prefix || link_anchor {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut has_alpha = false;
        while j < line.len() && is_tag_char(line[j]) {
            if line[j].is_ascii_alphabetic() || line[j] >= 0x80 {
                has_alpha = true;
            }
            j += 1;
        }
        if j > i + 1
            && has_alpha
            && let Ok(tag) = std::str::from_utf8(&line[i..j])
        {
            push_call(out, tag.to_string(), line_start, i, j - i, row);
        }
        i = j.max(i + 1);
    }
}

fn is_tag_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/') || b >= 0x80
}

/// Parse a frontmatter line for `tags:` entries, emitting each as a `#tag` call. Handles the inline
/// `tags: [a, b]` / `tags: a, b` form and the block form (`tags:` followed by `  - a` list lines);
/// `in_tags_block` carries the block state across lines. Any other unindented `key:` ends the block.
fn scan_frontmatter_line(line: &[u8], line_start: usize, row: u32, in_tags_block: &mut bool, out: &mut Vec<Call>) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let indent = text.len() - text.trim_start().len();
    let trimmed = text.trim();

    if *in_tags_block {
        if let Some(item) = trimmed.strip_prefix("- ") {
            emit_tag_value(item, line_start, indent, row, out);
            return;
        }
        if indent == 0 && trimmed.contains(':') {
            *in_tags_block = false;
        } else if trimmed.is_empty() {
            return;
        }
    }

    let key = trimmed.split(':').next().unwrap_or("");
    if key == "tags" || key == "tag" {
        let rest = trimmed.strip_prefix(key).unwrap_or("").trim_start_matches(':').trim();
        if rest.is_empty() {
            *in_tags_block = true;
            return;
        }
        let inner = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(rest);
        for part in inner.split(',') {
            emit_tag_value(part.trim(), line_start, indent, row, out);
        }
    }
}

fn emit_tag_value(raw: &str, line_start: usize, col: usize, row: u32, out: &mut Vec<Call>) {
    let value = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.is_empty() || !value.bytes().all(is_tag_char) {
        return;
    }
    push_call(out, format!("#{value}"), line_start, col, value.len(), row);
}

/// Reduce a wikilink's inner text to its target note name: drop the `|alias` display text, then the
/// `#heading` anchor, then trim. Returns `None` for an empty target.
fn wikilink_target(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?;
    let s = s.split('|').next().unwrap_or(s);
    let s = s.split('#').next().unwrap_or(s);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Reduce a standard-link destination to a note/file name, or `None` if it is an external URL or a
/// bare same-page anchor. Strips the `#anchor`/`?query`, the directory prefix, and a trailing `.md`,
/// and decodes `%20` — so `[t](notes/My%20Note.md#h)` resolves to `My Note`, unifying the backlink
/// graph with `[[My Note]]`.
fn mdlink_target(raw: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(raw).ok()?.trim();
    if s.is_empty() || s.starts_with('#') || s.contains("://") || s.starts_with("mailto:") {
        return None;
    }
    let s = s.split(['#', '?']).next().unwrap_or(s);
    let s = s.rsplit('/').next().unwrap_or(s);
    let s = s.strip_suffix(".md").unwrap_or(s);
    let decoded = s.replace("%20", " ");
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod markdown_ref_tests {
    use super::*;

    fn callees(src: &str) -> Vec<String> {
        markdown_references(src.as_bytes())
            .into_iter()
            .map(|c| c.callee)
            .collect()
    }

    #[test]
    fn extracts_plain_aliased_anchored_and_embed_wikilinks() {
        let src = "# Note\n\nSee [[Other Note]] and [[Target#Section|click here]].\n\nEmbed: ![[Diagram.png]]\nNo link here.\n";
        assert_eq!(
            callees(src),
            vec![
                "Other Note".to_string(),
                "Target".to_string(),
                "Diagram.png".to_string()
            ]
        );
    }

    #[test]
    fn reports_accurate_row_and_col_for_wikilink() {
        let src = "line0\nline1\nabc [[Target]]\n";
        let links = markdown_references(src.as_bytes());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].callee, "Target");
        assert_eq!(links[0].start_row, 2);
        assert_eq!(links[0].start_col, 4);
    }

    #[test]
    fn ignores_unclosed_and_empty_wikilinks() {
        assert_eq!(callees("an [[unclosed link\nand [[ ]] empty\n"), Vec::<String>::new());
    }

    #[test]
    fn extracts_standard_note_links_and_skips_external_urls() {
        let src = "[to a note](My%20Note.md) [image](img/Diagram.png) [site](https://example.com) [top](#heading)\n";
        assert_eq!(callees(src), vec!["My Note".to_string(), "Diagram.png".to_string()]);
    }

    #[test]
    fn extracts_inline_tags_including_nested_and_ignores_non_tags() {
        let src = "Tagged #project and #area/sub here. Not a#tag, not #123, anchor [[N#Head]].\n";
        assert_eq!(
            callees(src),
            vec!["N".to_string(), "#project".to_string(), "#area/sub".to_string()]
        );
    }

    #[test]
    fn skips_tags_and_links_inside_fenced_code() {
        let src = "real #tag\n```\n#include <stdio.h>\n[nope](nope.md)\n```\nafter #done\n";
        assert_eq!(callees(src), vec!["#tag".to_string(), "#done".to_string()]);
    }

    #[test]
    fn extracts_frontmatter_tags_inline_and_block() {
        let inline = "---\ntitle: Note\ntags: [alpha, beta]\n---\nbody #gamma\n";
        assert_eq!(
            callees(inline),
            vec!["#alpha".to_string(), "#beta".to_string(), "#gamma".to_string()]
        );
        let block = "---\ntags:\n  - one\n  - two\naliases: [x]\n---\n";
        assert_eq!(callees(block), vec!["#one".to_string(), "#two".to_string()]);
    }
}
