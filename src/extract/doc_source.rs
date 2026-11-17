//! Chunks of a document whose file is its text: plain text and logs, CSV and TSV, JSON and NDJSON,
//! YAML and TOML (ADR-0029 rule 10).
//!
//! xberg rewrites these before it chunks them: a plain-text paragraph becomes one line, a JSON or
//! YAML file a list of `key: value` paragraphs under headings. No chunk of that text can be found
//! again in the file, so no chunk could cite a line. Here the file itself is chunked, in one pass,
//! and every chunk's line range follows from its byte range.
//!
//! - Prose and logs break at a blank line, else at a line end, and overlap by whole lines.
//! - Records (CSV, TSV, NDJSON) break at a line end with no overlap; a CSV or TSV chunk after the
//!   first is embedded with the header line in front of it.
//! - Trees (JSON, YAML, TOML) break before the least indented line in the back half of the window
//!   that does not close a block, so a chunk tends to hold whole top-level entries.
//!
//! A line longer than the window is cut at whitespace, else at a character boundary.

use std::path::Path;

use super::DocChunk;

/// How a source's lines group, which decides where a chunk may end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Paragraphs and log lines.
    Prose,
    /// One record per line; `header` when the first line names the columns.
    Records { header: bool },
    /// Nested keys, by indentation.
    Tree,
}

/// The shape of a document of `mime` when its file is its text, else `None`.
pub(crate) fn shape(mime: &str) -> Option<Shape> {
    let mime = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match mime.as_str() {
        "text/plain" => Some(Shape::Prose),
        "text/csv" | "application/csv" | "text/tab-separated-values" => Some(Shape::Records { header: true }),
        "application/x-ndjson" | "application/jsonl" | "application/x-jsonlines" => {
            Some(Shape::Records { header: false })
        }
        "application/json" | "text/json" | "application/yaml" | "application/x-yaml" | "text/yaml" | "text/x-yaml"
        | "application/toml" | "application/x-toml" | "text/toml" | "text/x-toml" => Some(Shape::Tree),
        _ => None,
    }
}

/// The text of the file at `path`, invalid UTF-8 replaced (no line break is added or lost). `None`
/// when it cannot be read or holds only whitespace.
pub(crate) fn read(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Byte spans of the chunks of `text`, each at most `max_chars` characters, in order. Whitespace-only
/// spans are dropped. O(n): each window is scanned once, and an overlap only within its own window.
pub(crate) fn spans(text: &str, shape: Shape, max_chars: usize, overlap: usize) -> Vec<(usize, usize)> {
    let max_chars = max_chars.max(1);
    let overlap = match shape {
        Shape::Prose => overlap.min(max_chars.saturating_sub(1)),
        Shape::Records { .. } | Shape::Tree => 0,
    };
    let mut spans = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let window_end = text[start..]
            .char_indices()
            .nth(max_chars)
            .map_or(text.len(), |(i, _)| start + i);
        let end = if window_end == text.len() {
            window_end
        } else {
            break_in(text, start, window_end, shape)
        };
        if !text[start..end].trim().is_empty() {
            spans.push((start, end));
        }
        if end >= text.len() {
            break;
        }
        start = if overlap == 0 {
            end
        } else {
            overlap_start(text, start, end, overlap)
        };
    }
    spans
}

/// Where a chunk starting at `start` and limited to `window_end` ends: the best line start in the
/// back half of the window, else the end of the last whitespace there, else `window_end`.
fn break_in(text: &str, start: usize, window_end: usize, shape: Shape) -> usize {
    let mut half = start + (window_end - start) / 2;
    while !text.is_char_boundary(half) {
        half -= 1;
    }
    let bytes = text.as_bytes();
    // `(rank, at)`: the highest rank wins, the later position on a tie.
    let mut best: Option<(usize, usize)> = None;
    for newline in memchr::memchr_iter(b'\n', &bytes[half..window_end]) {
        let at = half + newline + 1;
        if at <= start {
            continue;
        }
        let rank = match shape {
            Shape::Records { .. } => 0,
            Shape::Prose => usize::from(line_is_blank(bytes, at)),
            Shape::Tree => match tree_rank(bytes, at) {
                Some(rank) => rank,
                None => continue,
            },
        };
        if best.is_none_or(|(best_rank, _)| rank >= best_rank) {
            best = Some((rank, at));
        }
    }
    if let Some((_, at)) = best {
        return at;
    }
    text[half..window_end]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(window_end, |(i, c)| half + i + c.len_utf8())
}

/// Whether the line that ends just before `at` (a line start) is blank, so `at` starts a paragraph.
fn line_is_blank(bytes: &[u8], at: usize) -> bool {
    let before = &bytes[..at - 1];
    let line_start = memchr::memrchr(b'\n', before).map_or(0, |i| i + 1);
    before[line_start..].iter().all(u8::is_ascii_whitespace)
}

/// The rank of a tree break before the line at `at`: higher for a less indented line; `None` for a
/// line that closes a block (`}`, `]`), which belongs with the chunk it closes. The line may run
/// past the window: its indentation is what places it.
fn tree_rank(bytes: &[u8], at: usize) -> Option<usize> {
    let indent = bytes[at..].iter().take_while(|b| **b == b' ' || **b == b'\t').count();
    match bytes.get(at + indent) {
        Some(b'}' | b']') => None,
        _ => Some(usize::MAX - indent),
    }
}

/// The start of the chunk after `[start, end)`: the first line start in its last `overlap`
/// characters, or `end` when that tail holds none.
fn overlap_start(text: &str, start: usize, end: usize, overlap: usize) -> usize {
    let tail = text[start..end]
        .char_indices()
        .rev()
        .nth(overlap.saturating_sub(1))
        .map_or(start, |(i, _)| start + i);
    if tail <= start {
        return end;
    }
    if text.as_bytes()[tail - 1] == b'\n' {
        return tail;
    }
    memchr::memchr(b'\n', &text.as_bytes()[tail..end])
        .map(|i| tail + i + 1)
        .filter(|&next| next < end)
        .unwrap_or(end)
}

/// The 1-based first and last line of each span of `text`.
pub(crate) fn line_ranges(text: &str, spans: &[(usize, usize)]) -> Vec<(u32, u32)> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(memchr::memchr_iter(b'\n', text.as_bytes()).map(|i| i + 1))
        .collect();
    let line_of = |offset: usize| u32::try_from(line_starts.partition_point(|&s| s <= offset)).unwrap_or(u32::MAX);
    spans
        .iter()
        .map(|&(start, end)| (line_of(start), line_of(end.saturating_sub(1).max(start))))
        .collect()
}

/// The chunks of `source` and, when `embed`, the text each is embedded from: the chunk itself, with
/// the header line in front of every CSV or TSV chunk but the first.
pub(crate) fn chunks(
    source: &str,
    shape: Shape,
    max_chars: usize,
    overlap: usize,
    embed: bool,
) -> (Vec<DocChunk>, Vec<String>) {
    let spans = spans(source, shape, max_chars, overlap);
    let lines = line_ranges(source, &spans);
    let header = match shape {
        Shape::Records { header: true } => source.lines().next().filter(|line| !line.trim().is_empty()),
        _ => None,
    };
    let mut dense = Vec::new();
    let chunks = spans
        .iter()
        .zip(lines)
        .map(|(&(start, end), (line_start, line_end))| {
            let text = source[start..end].to_string();
            if embed {
                dense.push(match header {
                    Some(header) if start > 0 => format!("{header}\n{text}"),
                    _ => text.clone(),
                });
            }
            DocChunk {
                byte_start: u32::try_from(start).unwrap_or(u32::MAX),
                byte_end: u32::try_from(end).unwrap_or(u32::MAX),
                text,
                line_start: Some(line_start),
                line_end: Some(line_end),
                ..DocChunk::default()
            }
        })
        .collect();
    (chunks, dense)
}

/// A document's chunks, the text they were cut from and their embedding inputs.
pub(crate) struct Chunked {
    pub content: String,
    pub chunks: Vec<DocChunk>,
    pub dense: Vec<String>,
    /// The chunks were cut from the file, not from xberg's text.
    pub from_source: bool,
}

/// Chunk the document at `path` that xberg extracted as `result`: from the file when it is its own
/// text (`shape`, else the shape of the type xberg reports), else from xberg's text, by xberg's
/// chunks unless `linear` or xberg was asked for none. `result`'s text is taken.
pub(crate) fn chunk_document(
    path: &Path,
    shape: Option<Shape>,
    result: &mut xberg::types::ExtractedDocument,
    linear: bool,
    config: &super::DocConfig,
    embed: bool,
) -> Chunked {
    let content = std::mem::take(&mut result.content);
    let shape = shape.or_else(|| self::shape(&result.mime_type));
    if let Some(shape) = shape
        && let Some(source) = read(path)
    {
        let (chunks, dense) = chunks(&source, shape, config.max_characters, config.overlap, embed);
        return Chunked {
            content: source,
            chunks,
            dense,
            from_source: true,
        };
    }
    let inputs: Vec<super::text::ChunkInput<'_>> = match result.chunks.as_deref() {
        Some(chunks) if !linear => chunks.iter().map(super::text::ChunkInput::of_xberg).collect(),
        _ => crate::extract::doc_guard::linear_chunk_spans(&content, config.max_characters, config.overlap)
            .into_iter()
            .map(|(start, end)| super::text::ChunkInput::of_span(&content, start, end))
            .collect(),
    };
    let source = super::text::read_text_source(path, &result.mime_type);
    let (chunks, dense) = super::text::build_chunks(&content, &result.mime_type, source.as_deref(), inputs, embed);
    Chunked {
        content,
        chunks,
        dense,
        from_source: false,
    }
}

/// Bytes past the previous entity in which the next one is looked for.
const ENTITY_WINDOW: usize = 64 * 1024;

/// Point each entity at its text in `source`: xberg found it in its own rendering, whose offsets do
/// not hold in the source the chunks were cut from. Entities are searched in order, each in a bounded
/// window after the previous match; one not found there is placed in no chunk.
pub(crate) fn place_entities(source: &str, entities: &mut [super::DocEntity]) {
    entities.sort_by_key(|entity| entity.start);
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    // The same span named twice (two categories) is placed once.
    let mut previous: Option<((u32, u32), (u32, u32))> = None;
    for entity in entities.iter_mut() {
        let original = (entity.start, entity.end);
        if let Some((seen, placed)) = previous
            && seen == original
        {
            (entity.start, entity.end) = placed;
            continue;
        }
        let needle = entity.text.as_bytes();
        let window_end = cursor.saturating_add(ENTITY_WINDOW).min(bytes.len());
        let found = (!needle.is_empty())
            .then(|| memchr::memmem::find(&bytes[cursor..window_end], needle))
            .flatten();
        match found {
            Some(i) => {
                let start = cursor + i;
                entity.start = u32::try_from(start).unwrap_or(u32::MAX);
                entity.end = u32::try_from(start + needle.len()).unwrap_or(u32::MAX);
                cursor = start + needle.len();
            }
            None => {
                entity.start = u32::MAX;
                entity.end = u32::MAX;
            }
        }
        previous = Some((original, (entity.start, entity.end)));
    }
}

#[cfg(test)]
#[path = "doc_source_tests.rs"]
mod tests;
