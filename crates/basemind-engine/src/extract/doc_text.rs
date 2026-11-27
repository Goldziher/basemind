//! Chunk text and structure, after xberg (ADR-0029 rule 10): what a stored chunk says, where it
//! starts, and which source lines it covers.
//!
//! - HTML entities a Markdown-family source writes in its prose (`R&amp;D`) are decoded once, so
//!   the stored, searched and embedded text says `R&D`. Inside a fenced or indented code block an
//!   entity is literal content and stays.
//! - A chunk never starts inside a fenced block it did not open: the overlap xberg carries over
//!   from the previous chunk is trimmed to the end of that block, or, when the block runs on past
//!   the previous chunk, the chunk reopens it with the block's opening fence line.
//! - A chunk of a text-like source cites the source lines it came from.

use std::borrow::Cow;

/// Whether chunks of `mime` are Markdown text, in which an entity or a backslash escape can stand
/// in prose: Markdown sources, and every format xberg renders to Markdown. Plain text is taken as
/// written.
pub(crate) fn decodes_entities(mime: &str) -> bool {
    !matches!(crate::lance::docs::mime_group(mime), "text" | "other")
}

/// `text` with HTML entities decoded outside code: fenced blocks (```` ``` ```` / `~~~`), indented
/// code and inline code spans keep theirs. One pass, so `&amp;lt;` becomes `&lt;` and stops. A
/// no-break space becomes a space and a soft hyphen is dropped. Borrowed when nothing changes.
///
/// With `in_code`, code is decoded too: for a chunk whose source writes no entity at all, so any
/// entity in it was put there by the extractor.
pub(crate) fn decode_entities_where(text: &str, in_code: bool) -> Cow<'_, str> {
    if !text.contains(['&', '\u{a0}', '\u{ad}', '\\']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if in_code && !closes_fence(trimmed, open) {
                decode_span(line, &mut out, false);
            } else {
                out.push_str(line);
            }
            if closes_fence(trimmed, open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opens_fence(trimmed) {
            fence = Some(open);
            out.push_str(line);
            continue;
        }
        if in_code {
            decode_span(line, &mut out, false);
            continue;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            out.push_str(line);
            continue;
        }
        decode_prose_line(line, &mut out);
    }
    if out == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out)
    }
}

/// The fence character and length when `line` (leading space trimmed) opens a fenced block.
fn opens_fence(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = line.chars().take_while(|c| *c == first).count();
    (run >= 3).then_some((first, run))
}

fn closes_fence(line: &str, (fence, len): (char, usize)) -> bool {
    let run = line.chars().take_while(|c| *c == fence).count();
    run >= len && line[run * fence.len_utf8()..].trim().is_empty()
}

/// Decode one prose line, leaving inline code spans untouched.
fn decode_prose_line(line: &str, out: &mut String) {
    let mut rest = line;
    while let Some(tick) = rest.find('`') {
        decode_span(&rest[..tick], out, true);
        let after = &rest[tick..];
        let run = after.chars().take_while(|c| *c == '`').count();
        let closing = "`".repeat(run);
        match after[run..].find(&closing) {
            Some(end) => {
                let span_end = run + end + run;
                out.push_str(&after[..span_end]);
                rest = &after[span_end..];
            }
            None => {
                out.push_str(after);
                return;
            }
        }
    }
    decode_span(rest, out, true);
}

/// Decode `span`. With `unescape` (prose), a Markdown backslash escape of ASCII punctuation, which
/// xberg's Markdown rendering writes for a literal `&`, `*` or `_` (`R\&D`), is read as the
/// character itself.
fn decode_span(span: &str, out: &mut String, unescape: bool) {
    let mut rest = span;
    while let Some(amp) = rest.find(['&', '\u{a0}', '\u{ad}', '\\']) {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let ch = tail.chars().next().unwrap_or('&');
        if ch == '\\' {
            match tail[1..].chars().next() {
                Some(next) if unescape && next.is_ascii_punctuation() => {
                    out.push(next);
                    rest = &tail[1 + next.len_utf8()..];
                }
                _ => {
                    out.push('\\');
                    rest = &tail[1..];
                }
            }
            continue;
        }
        if ch == '\u{a0}' {
            out.push(' ');
            rest = &tail[ch.len_utf8()..];
            continue;
        }
        if ch == '\u{ad}' {
            rest = &tail[ch.len_utf8()..];
            continue;
        }
        match entity(tail) {
            Some((decoded, len)) => {
                out.push(decoded);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
}

/// Whether `text` contains an entity this module decodes.
pub(crate) fn has_entity(text: &str) -> bool {
    text.match_indices('&').any(|(at, _)| entity(&text[at..]).is_some())
}

/// The character an entity at the start of `s` stands for, and the entity's byte length.
fn entity(s: &str) -> Option<(char, usize)> {
    let end = s[1..].find(';')? + 1;
    if end > 12 {
        return None;
    }
    let name = &s[1..end];
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => ' ',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '\u{2026}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code).filter(|c| *c != '\0')?
        }
    };
    Some((decoded, end + 1))
}

/// Where the fenced blocks of `content` are: `(open line start, open line end, close line end)`
/// byte ranges, the close end being `content.len()` for a block left open.
pub(crate) fn fenced_blocks(content: &str) -> Vec<(usize, usize, usize)> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, usize, (char, usize))> = None;
    let mut at = 0;
    for line in content.split_inclusive('\n') {
        let end = at + line.len();
        let trimmed = line.trim_start();
        match open {
            Some((start, open_end, fence)) if closes_fence(trimmed, fence) => {
                blocks.push((start, open_end, end));
                open = None;
            }
            Some(_) => {}
            None => {
                if let Some(fence) = opens_fence(trimmed) {
                    open = Some((at, end, fence));
                }
            }
        }
        at = end;
    }
    if let Some((start, open_end, _)) = open {
        blocks.push((start, open_end, content.len()));
    }
    blocks
}

/// Where chunk `start` should begin, given that everything before `previous_end` is already in
/// the previous chunk, plus a fence line to put in front when the chunk must reopen a block.
///
/// - Inside a fenced block opened before `start`: move to the block's end when the previous chunk
///   already holds everything up to it; otherwise keep the start and reopen the block with its
///   opening fence line.
/// - Otherwise, inside the overlap: move to the next line start, or word start, not past
///   `previous_end`, so the chunk does not begin mid-line or mid-word.
pub(crate) fn snap_start<'a>(
    content: &'a str,
    blocks: &[(usize, usize, usize)],
    start: usize,
    previous_end: usize,
) -> (usize, Option<&'a str>) {
    if let Some(&(open_start, open_end, close_end)) = blocks
        .iter()
        .find(|(open_start, open_end, close_end)| *open_start < start && start < *close_end && start >= *open_end)
    {
        if close_end <= previous_end {
            return (close_end.min(content.len()), None);
        }
        let opener = content[open_start..open_end].trim_end_matches(['\n', '\r']);
        return (start, Some(opener));
    }
    if start >= previous_end || start == 0 || content.as_bytes().get(start - 1) == Some(&b'\n') {
        return (start, None);
    }
    let Some(window) = content.get(start..previous_end) else {
        return (start, None);
    };
    if let Some(newline) = window.find('\n') {
        return (start + newline + 1, None);
    }
    let prev = content[..start].chars().next_back();
    let next = content[start..].chars().next();
    if prev.is_some_and(char::is_alphanumeric)
        && next.is_some_and(char::is_alphanumeric)
        && let Some(space) = window.find(char::is_whitespace)
    {
        let after = start + space;
        let skip = content[after..]
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum::<usize>();
        return ((after + skip).min(previous_end), None);
    }
    (start, None)
}

/// The 1-based source lines each chunk covers, found by locating its first and last non-empty
/// line in `source`, searching forward from the previous chunk's match. `None` where a chunk's
/// line cannot be found (the extractor rewrote it).
pub(crate) fn source_lines<'a>(source: &str, chunks: impl IntoIterator<Item = &'a str>) -> Vec<Option<(u32, u32)>> {
    source_lines_counted(source, chunks).0
}

/// Bytes of source a lookup may search past its window, as a multiple of the source's length.
const SOURCE_LINES_SPILL_FACTOR: usize = 4;

/// Chunk lines tried, from each end, to find where a chunk starts and where it ends.
const LOOKUP_LINES: usize = 2;

/// A shorter run of plain text than this does not locate a line: it recurs too often.
const MIN_FRAGMENT: usize = 12;

/// The longest run of plain text a fragment lookup searches for.
const MAX_FRAGMENT: usize = 64;

/// `source` with every run of whitespace written as one space, and the offset in it at which each
/// source line starts. The extractor rewraps text (a soft-wrapped Markdown paragraph becomes one
/// line), so lines are matched with their whitespace collapsed.
struct Collapsed {
    text: Vec<u8>,
    line_marks: Vec<usize>,
}

impl Collapsed {
    fn of(source: &str) -> Self {
        let mut text = Vec::with_capacity(source.len());
        let mut line_marks = vec![0];
        let mut in_space = false;
        for &byte in source.as_bytes() {
            if byte.is_ascii_whitespace() {
                if !in_space {
                    text.push(b' ');
                    in_space = true;
                }
                if byte == b'\n' {
                    line_marks.push(text.len());
                }
            } else {
                text.push(byte);
                in_space = false;
            }
        }
        Self { text, line_marks }
    }

    /// The 1-based source line of the byte at `offset` of the collapsed text.
    fn line_of(&self, offset: usize) -> u32 {
        u32::try_from(self.line_marks.partition_point(|&mark| mark <= offset)).unwrap_or(u32::MAX)
    }
}

/// `text` with its whitespace collapsed as [`Collapsed`] does, and trimmed.
fn collapse(text: &str) -> String {
    text.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// A run of plain text in `line` that the extractor's rewriting leaves alone: the line without its
/// list, quote, heading or table marker, cut at the first (or, `from_end`, the last) markup
/// character, whitespace collapsed, at most [`MAX_FRAGMENT`] bytes. `None` when no such run is long
/// enough to locate the line.
fn plain_fragment(line: &str, from_end: bool) -> Option<String> {
    let line = line.trim_start_matches(['#', '>', '*', '-', '+', '|', ' ', '\t']);
    let digits = line.trim_start_matches(|c: char| c.is_ascii_digit());
    let line = if digits.len() < line.len() {
        digits.strip_prefix(['.', ')']).unwrap_or(line)
    } else {
        line
    };
    let markup = |c: char| {
        matches!(
            c,
            '*' | '_' | '`' | '[' | ']' | '(' | ')' | '|' | '<' | '>' | '&' | '\\' | '~'
        )
    };
    let mut runs = line.split(markup).map(collapse).filter(|run| run.len() >= MIN_FRAGMENT);
    let run = if from_end { runs.next_back()? } else { runs.next()? };
    if run.len() <= MAX_FRAGMENT {
        return Some(run);
    }
    let cut = if from_end {
        let mut at = run.len() - MAX_FRAGMENT;
        while !run.is_char_boundary(at) {
            at += 1;
        }
        run[at..].to_string()
    } else {
        let mut at = MAX_FRAGMENT;
        while !run.is_char_boundary(at) {
            at -= 1;
        }
        run[..at].to_string()
    };
    Some(cut)
}

/// What to search for to find `line`: the line itself, whitespace collapsed, then a plain fragment
/// of it.
fn needles(line: &str, from_end: bool) -> impl Iterator<Item = String> {
    std::iter::once(collapse(line)).chain(plain_fragment(line, from_end))
}

/// [`source_lines`], plus the bytes of source searched. A chunk's line is looked for in a window
/// after the previous match sized by the chunk; past the window only while a whole-run budget of
/// [`SOURCE_LINES_SPILL_FACTOR`] times the source lasts. Without the bound a chunk whose line is
/// absent searched to the end of the source, once per chunk: an extractor that reflows a
/// multi-megabyte log into one line made the lookup quadratic and the scan minutes long.
///
/// A chunk starts at the first of its first [`LOOKUP_LINES`] lines found and ends at the last of
/// its last ones, each tried whole and then by a plain fragment, whitespace collapsed on both sides.
pub(crate) fn source_lines_counted<'a>(
    source: &str,
    chunks: impl IntoIterator<Item = &'a str>,
) -> (Vec<Option<(u32, u32)>>, usize) {
    let collapsed = Collapsed::of(source);
    let bytes = collapsed.text.as_slice();
    let mut spill_left = source.len().saturating_mul(SOURCE_LINES_SPILL_FACTOR);
    let mut searched = 0usize;
    let mut find = |from: usize, needle: &str, window: usize, spill: bool| -> Option<usize> {
        if needle.is_empty() || from >= bytes.len() {
            return None;
        }
        let near_end = from.saturating_add(window).min(bytes.len());
        searched += near_end - from;
        if let Some(i) = memchr::memmem::find(&bytes[from..near_end], needle.as_bytes()) {
            return Some(from + i);
        }
        let rest = bytes.len() - near_end;
        if !spill || near_end == bytes.len() || rest > spill_left {
            return None;
        }
        spill_left -= rest;
        searched += rest;
        // Back up so a match straddling the window's end is found.
        let resume = near_end.saturating_sub(needle.len()).max(from);
        memchr::memmem::find(&bytes[resume..], needle.as_bytes()).map(|i| resume + i)
    };
    let mut cursor = 0;
    let lines = chunks
        .into_iter()
        .map(|chunk| {
            let window = chunk.len().saturating_mul(8).saturating_add(4096);
            let distinctive: Vec<&str> = chunk
                .lines()
                .map(str::trim)
                .filter(|line| is_distinctive(line))
                .collect();
            let start = distinctive
                .iter()
                .take(LOOKUP_LINES)
                .find_map(|line| needles(line, false).find_map(|needle| find(cursor, &needle, window, true)))?;
            let end = distinctive
                .iter()
                .rev()
                .take(LOOKUP_LINES)
                .find_map(|line| {
                    needles(line, true).find_map(|needle| find(start, &needle, window, false).map(|i| i + needle.len()))
                })
                .unwrap_or(start + 1);
            cursor = start;
            Some((
                collapsed.line_of(start),
                collapsed.line_of(end.saturating_sub(1).max(start)),
            ))
        })
        .collect();
    (lines, searched)
}

/// Whether `line` can locate a chunk in its source: long enough, and not a fence, a rule or a
/// table's delimiter row, which repeat throughout a document.
fn is_distinctive(line: &str) -> bool {
    line.len() >= 3
        && !line
            .chars()
            .all(|c| matches!(c, '`' | '~' | '-' | '*' | '_' | '=' | '#' | '|' | ':' | ' '))
}

/// Source lines are looked up only in a text source no larger than this.
const SOURCE_LINES_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// What a chunk of a document becomes, given what xberg reported for it.
pub struct ChunkInput<'a> {
    pub text: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub kind: String,
    pub pages: Option<(u32, u32)>,
    pub section: Vec<String>,
    pub heading_context: Option<&'a xberg::types::HeadingContext>,
}

impl<'a> ChunkInput<'a> {
    /// The input of one xberg chunk.
    pub(crate) fn of_xberg(chunk: &'a xberg::types::Chunk) -> Self {
        let meta = &chunk.metadata;
        Self {
            text: chunk.content.clone(),
            byte_start: meta.byte_start,
            byte_end: meta.byte_end,
            kind: serde_json::to_value(chunk.chunk_type)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            pages: meta.first_page.map(|first| (first, meta.last_page.unwrap_or(first))),
            section: meta.heading_path.clone(),
            heading_context: meta.heading_context.as_ref(),
        }
    }

    /// The input of one span of the linear chunker.
    pub(crate) fn of_span(content: &str, start: usize, end: usize) -> Self {
        Self {
            text: content[start..end].to_string(),
            byte_start: start,
            byte_end: end,
            kind: String::new(),
            pages: None,
            section: Vec::new(),
            heading_context: None,
        }
    }
}

/// Turn the chunks of one document into stored chunks and, when `embed`, the text each is embedded
/// from (the heading breadcrumb goes there only). Starts are snapped out of carried-over fences and
/// words, entities decoded for Markdown-family sources, pages kept for paged formats, and source
/// lines found for text sources (`source`).
pub(crate) fn build_chunks(
    content: &str,
    mime: &str,
    source: Option<&str>,
    inputs: Vec<ChunkInput<'_>>,
    embed: bool,
) -> (Vec<super::DocChunk>, Vec<String>) {
    let blocks = fenced_blocks(content);
    let decode = decodes_entities(mime);
    let decode_in_code = decode && source.is_some_and(|source| !has_entity(source));
    let paged = matches!(crate::lance::docs::mime_group(mime), "pdf" | "office");
    let mut previous_end = 0;
    let mut chunks = Vec::with_capacity(inputs.len());
    let mut dense = Vec::new();
    for (i, mut input) in inputs.into_iter().enumerate() {
        let exact = content.get(input.byte_start..input.byte_end) == Some(input.text.as_str());
        if i > 0 && exact {
            let (start, opener) = snap_start(content, &blocks, input.byte_start, previous_end);
            if start > input.byte_start && start < input.byte_end {
                input.text = content[start..input.byte_end].to_string();
                input.byte_start = start;
            }
            if let Some(opener) = opener {
                input.text = format!("{opener}\n{}", input.text);
            }
        }
        if exact {
            previous_end = previous_end.max(input.byte_end);
        }
        let text = if decode {
            decode_entities_where(&input.text, decode_in_code).into_owned()
        } else {
            input.text
        };
        if embed {
            dense.push(match input.heading_context {
                Some(context) if !context.headings.is_empty() => {
                    xberg::chunking::render_heading_breadcrumb(&text, context)
                }
                _ => text.clone(),
            });
        }
        chunks.push(super::DocChunk {
            byte_start: u32::try_from(input.byte_start).unwrap_or(u32::MAX),
            byte_end: u32::try_from(input.byte_end).unwrap_or(u32::MAX),
            text,
            embedding: Vec::new(),
            kind: input.kind,
            page_start: input.pages.filter(|_| paged).map(|p| p.0),
            page_end: input.pages.filter(|_| paged).map(|p| p.1),
            section: input.section,
            line_start: None,
            line_end: None,
        });
    }
    if let Some(source) = source {
        let lines = source_lines(source, chunks.iter().map(|chunk| chunk.text.as_str()));
        for (chunk, lines) in chunks.iter_mut().zip(lines) {
            if let Some((start, end)) = lines {
                chunk.line_start = Some(start);
                chunk.line_end = Some(end);
            }
        }
    }
    (chunks, dense)
}

/// Give `chunks` the vectors `embed` returns for `inputs` (one per chunk) and return their
/// dimension. On any failure no chunk keeps a vector and the reason is returned: the caller indexes
/// the document without them.
pub(crate) fn embed_chunks(
    inputs: &[String],
    chunks: &mut [super::DocChunk],
    embed: impl FnOnce(&[&str]) -> Result<(u16, Vec<Vec<f32>>), String>,
) -> Result<u16, String> {
    let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let (dim, vectors) = embed(&refs)?;
    if vectors.len() != chunks.len() || vectors.iter().any(|v| v.len() != usize::from(dim)) {
        return Err(format!(
            "the embedder returned {} vectors for {} chunks",
            vectors.len(),
            chunks.len()
        ));
    }
    for (chunk, vector) in chunks.iter_mut().zip(vectors) {
        chunk.embedding = vector;
    }
    Ok(dim)
}

/// Embed `inputs` with the document embedder of `preset`: `(dimension, one vector per input)`, or
/// why it could not.
pub(crate) fn embed_document_inputs(
    preset: &str,
    max_threads: usize,
    batch_size: usize,
    provider: crate::config::OnnxProvider,
    inputs: &[&str],
) -> Result<(u16, Vec<Vec<f32>>), String> {
    #[cfg(feature = "test-support")]
    if let Some(embed) = crate::embeddings::test_document_embedder::get() {
        let dim = crate::embeddings::preset_dimension(preset).map_err(|error| format!("{error:#}"))?;
        return embed(inputs, dim).map(|vectors| (dim, vectors));
    }
    let embedder = crate::embeddings::SharedEmbedder::load_with_provider(preset, max_threads, batch_size, provider)
        .map_err(|error| format!("loading dense embedder: {error:#}"))?;
    let vectors = embedder
        .embed_batch(inputs)
        .map_err(|error| format!("embedding document chunks: {error:#}"))?;
    Ok((embedder.dim(), vectors))
}

/// Whether the document embedder of `preset` embeds now: one probe text through
/// [`embed_document_inputs`]. A document whose embedding failed owes its vectors, and is
/// re-extracted to pay them only once this holds.
pub(crate) fn document_embedder_ready(
    preset: &str,
    max_threads: usize,
    batch_size: usize,
    provider: crate::config::OnnxProvider,
) -> bool {
    match embed_document_inputs(preset, max_threads, batch_size, provider, &["embedder probe"]) {
        Ok(_) => true,
        Err(reason) => {
            tracing::info!(%reason, "document embedder unavailable; documents owing vectors wait for it");
            false
        }
    }
}

/// The text of `path` when it is a text source small enough to look lines up in.
pub(crate) fn read_text_source(path: &std::path::Path, mime: &str) -> Option<String> {
    let group = crate::lance::docs::mime_group(mime);
    if !matches!(group, "markdown" | "text") {
        return None;
    }
    let len = std::fs::metadata(path).ok()?.len();
    if len > SOURCE_LINES_MAX_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
#[path = "doc_text_tests.rs"]
mod tests;
