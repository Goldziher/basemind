//! `run_workspace_grep` helper — kept in its own file so `helpers.rs` stays under the
//! 1000-line cap as the MCP surface grows.

use std::cell::RefCell;
use std::io::Read;
use std::sync::atomic::Ordering;

use memchr::memmem::Finder;
use rayon::prelude::*;
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::cursor::Cursor;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use super::helpers_filters::{Filters, PathFilter};
use super::helpers_grep_text::{annotate_symbols, clip_line, line_text};
use super::index_route::blocking;
use super::tool_contract::{IndexedFiles, Sweep};
use super::types::{GrepHit, GrepTruncation, WorkspaceGrepParams, WorkspaceGrepResponse};
use crate::index::grep_bloom::GrepEntry;
use crate::path::RelPath;

/// Upper bound on the file content one `workspace_grep` call may read, summed from the sizes the
/// index already recorded at scan time.
///
/// The bound is on BYTES, not on files visited: grep is a full-corpus linear scan by definition, so
/// a files-visited cap would silently hide every match past the cut — and a rare identifier, which
/// is precisely what one greps for, is exactly what does not live in the first N files. The budget
/// exists only to keep a single call from reading an unbounded workspace (a repo of vendored
/// minified bundles), and it is derived from indexed sizes rather than from a wall-clock deadline so
/// that the cut point is deterministic — a non-deterministic cut would corrupt cursor pagination.
const GREP_BYTE_BUDGET: u64 = 2 * 1024 * 1024 * 1024;

/// Body of the `code` tool's `grep` mode.
///
/// Scans every indexed file that passes the `path_contains` / `language` filters — the full corpus,
/// in parallel — reads each as UTF-8 (non-UTF-8 and unreadable files are skipped and counted in
/// `skipped_files`), and applies the compiled regex. `limit` caps returned HITS, never files
/// scanned, so `total_matches` and `total_files_matched` are exact for the scanned window. Supports
/// in-memory pagination via `cursor` / `next_cursor` on the same
/// `encode_in_memory(offset, generation)` scheme as `list_files`.
pub(super) async fn run_workspace_grep(
    state: &ServerState,
    params: WorkspaceGrepParams,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let format = super::toon::ResponseFormat::parse(params.format.as_deref());
    let limit = params.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).min(SEARCH_LIMIT_MAX) as usize;
    let generation = state.shared.cache_generation.load(Ordering::Relaxed);

    let (skip_files, skip_hits) = match params.cursor.as_ref() {
        Some(c) => {
            let (offset, snapshot_id) = c.decode_in_memory()?;
            if snapshot_id != generation {
                return super::toon::format_result(
                    &WorkspaceGrepResponse {
                        pattern: params.pattern,
                        regex: !params.literal,
                        total_files_matched: 0,
                        total_matches: 0,
                        truncated: false,
                        truncated_reason: None,
                        budgeted: false,
                        hits: Vec::new(),
                        next_cursor: None,
                        skipped_files: 0,
                        cursor_invalidated: true,
                        notice: state.lifecycle_notice(),
                        elapsed_us: super::helpers::elapsed_us(started),
                    },
                    format,
                );
            }
            unpack_cursor(offset)
        }
        None => (0, 0),
    };

    // A literal pattern is escaped into the regex the rest of the sweep (and the trigram prefilter)
    // already understands; the response still echoes what the caller typed.
    let source = if params.literal {
        regex::escape(&params.pattern)
    } else {
        params.pattern.clone()
    };
    // Line-anchored like every grep: without `multi_line`, `^`/`$` match at the file's ends only.
    let re = regex::RegexBuilder::new(&source)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| {
            McpError::invalid_params(
                format!("invalid regex: {e}. Drop `regex` to search for the pattern as literal text."),
                None,
            )
        })
        .map(LineRegex::new)?;

    let literal = (params.literal || regex::escape(&params.pattern) == params.pattern)
        .then(|| Finder::new(params.pattern.as_bytes()).into_owned());

    let filters = Filters {
        tool: "code_grep",
        path: params.path_contains.as_deref().map(PathFilter::contains),
        language: params.language.as_deref(),
    };

    let cache = state.shared.cache.load_full();

    // Grep needs only path, language and indexed size — every one of them in the symbol-free file
    // view — so the candidate scan never decodes an outline however large the corpus is.
    let candidates: Vec<(&RelPath, &super::l1_cache::FileMeta)> = cache
        .file_metas()
        .filter(|(path, meta)| {
            filters.path.as_ref().is_none_or(|f| f.matches(path.as_bytes()))
                && filters.language.is_none_or(|l| *meta.language == *l)
        })
        .collect();
    if candidates.is_empty()
        && let Some(refusal) = super::helpers_files::grep_selects_no_file(state, &cache, &filters)
    {
        return Err(refusal);
    }

    let window = candidates.get(skip_files.min(candidates.len())..).unwrap_or(&[]);
    let (scanned, byte_budget_hit) = apply_byte_budget(window);

    let root = state.shared.root.as_path();
    let files = IndexedFiles::of(state);
    let paths: Vec<&RelPath> = scanned.iter().map(|(path, _)| *path).collect();
    let entries: Vec<GrepEntry> = scanned
        .iter()
        .map(|(path, meta)| GrepEntry {
            rel: (*path).clone(),
            hash_hex: meta.hash_hex.to_string(),
            size_bytes: meta.size_bytes,
            mtime_ns: meta.mtime_ns,
        })
        .collect();
    // Files the per-file trigram blooms prove cannot match are never opened. They keep their slot in
    // `paths` (counted as zero), so window order, `file_idx` and every cursor are those of the full sweep.
    let skip = if bloom_prefilter_enabled() {
        super::index_route::IndexRoute::resolve(state)
            .await
            .grep_skip(root, &source, &entries)
            .await
    } else {
        None
    };
    let (counts, skipped_files) = blocking(|| {
        let sweep = Sweep::new(files);
        count_all(&sweep, &paths, skip.as_deref(), &re, literal.as_ref(), skip_hits)
    });

    let total_matches = counts.iter().fold(0u32, |acc, &c| acc.saturating_add(c));
    let total_files_matched = counts.iter().filter(|&&c| c > 0).count();

    let (selected, limit_resume) = select_hits(&counts, limit, skip_files, skip_hits);
    // Judged against the file the bytes were read from: an external path is not under `root`.
    let is_fresh = |file_idx: usize, file: &std::path::Path, bytes: &[u8]| {
        super::freshness::matches_index(scanned[file_idx].1.into(), file, bytes)
    };
    let (mut hits, hit_cursors) = blocking(|| {
        materialize(
            files,
            &paths,
            &selected,
            &is_fresh,
            &re,
            params.include_context,
            skip_files,
        )
    });
    annotate_symbols(&cache, &mut hits);

    // A page cut by `limit` is paging and says so with its cursor alone. Files the byte budget
    // left unread are a cut the caller did not ask for, on whichever page it shows.
    let next_cursor = match (limit_resume, byte_budget_hit) {
        (Some(offset), _) => Some(Cursor::encode_in_memory(offset, generation)),
        (None, true) => Some(Cursor::encode_in_memory(
            pack_cursor(skip_files + scanned.len(), 0),
            generation,
        )),
        (None, false) => None,
    };

    let budget = super::budget::apply_budget(hits, params.max_tokens);
    let (hits, budgeted, next_cursor) = if budget.budgeted {
        let kept = budget.items.len();
        let resume = hit_cursors
            .get(kept)
            .map(|&offset| Cursor::encode_in_memory(offset, generation));
        (budget.items, true, resume.or(next_cursor))
    } else {
        (budget.items, false, next_cursor)
    };

    super::toon::format_result(
        &WorkspaceGrepResponse {
            pattern: params.pattern,
            regex: !params.literal,
            total_files_matched,
            total_matches,
            truncated: byte_budget_hit,
            truncated_reason: byte_budget_hit.then_some(GrepTruncation::ByteBudget),
            budgeted,
            hits,
            next_cursor,
            skipped_files,
            cursor_invalidated: false,
            notice: state.lifecycle_notice(),
            elapsed_us: super::helpers::elapsed_us(started),
        },
        format,
    )
}

/// Operator kill switch for the trigram-bloom prefilter: `BASEMIND_GREP_BLOOM=0` forces the full
/// sweep. The prefilter never changes a result, so this exists for A/B timing and as an escape hatch.
fn bloom_prefilter_enabled() -> bool {
    std::env::var_os("BASEMIND_GREP_BLOOM").is_none_or(|v| v != "0")
}

/// The in-memory cursor carries one `u64` offset, but grep must resume at a HIT, not at a file: a
/// single file can hold more matches than `limit`. A file-granular cursor would either replay that
/// file's leading hits forever (no forward progress) or drop its tail (silent loss). Packing
/// `(candidate index, hit ordinal within that file)` into the halves of the existing offset makes
/// both impossible without changing the cursor wire shape.
fn pack_cursor(file_idx: usize, hit_ordinal: u32) -> u64 {
    ((file_idx as u64) << 32) | u64::from(hit_ordinal)
}

fn unpack_cursor(offset: u64) -> (usize, u32) {
    ((offset >> 32) as usize, offset as u32)
}

/// Trim the candidate window to the leading run whose indexed sizes fit [`GREP_BYTE_BUDGET`].
/// Returns the slice to scan and whether anything was cut. At least one file always survives, so a
/// paging caller can never stall on a cursor that refuses to advance.
fn apply_byte_budget<'a, 'b>(
    window: &'a [(&'b RelPath, &'b super::l1_cache::FileMeta)],
) -> (&'a [(&'b RelPath, &'b super::l1_cache::FileMeta)], bool) {
    let mut used: u64 = 0;
    let mut end = window.len();
    for (i, (_, meta)) in window.iter().enumerate() {
        used = used.saturating_add(meta.size_bytes);
        if used > GREP_BYTE_BUDGET {
            end = i.max(1);
            break;
        }
    }
    (&window[..end], end < window.len())
}

/// Count the matches in every scanned file, in parallel. Order-preserving: rayon's `map`/`collect`
/// keeps the input order, which the cursor arithmetic downstream relies on.
///
/// `skip_hits` discounts the matches the caller already consumed inside the file it resumed in, so
/// the totals mean "matches remaining from the cursor position", not "matches in the repo".
///
/// Also returns how many files could not be searched: gone, leading out of the repository, or
/// opened and found unreadable or not UTF-8.
fn count_all(
    sweep: &Sweep<'_>,
    scanned: &[&RelPath],
    skip: Option<&[bool]>,
    re: &LineRegex,
    literal: Option<&Finder<'static>>,
    skip_hits: u32,
) -> (Vec<u32>, usize) {
    let searched: Vec<Option<u32>> = scanned
        .par_iter()
        .enumerate()
        .map(|(i, path)| {
            if skip.is_some_and(|s| s.get(i).copied().unwrap_or(false)) {
                return Some(0);
            }
            with_indexed_bytes(sweep.path(path), sweep.max_file_bytes(), |bytes| {
                count_matches(bytes, re, literal)
            })
            .flatten()
        })
        .collect();
    let skipped = searched.iter().filter(|count| count.is_none()).count();
    let mut counts: Vec<u32> = searched.into_iter().map(Option::unwrap_or_default).collect();
    if let Some(first) = counts.first_mut() {
        *first = first.saturating_sub(skip_hits);
    }
    (counts, skipped)
}

/// Largest read buffer a worker thread keeps between files. A bigger file still reads fine — its
/// buffer is dropped right after — so the sweep's resident scratch is bounded at
/// `threads x RETAINED_BUF_CAP` however large the corpus's biggest file is.
const RETAINED_BUF_CAP: usize = 1024 * 1024;

thread_local! {
    /// Per-thread read buffer: the sweep reads tens of thousands of files, and a fresh `String`
    /// allocation (plus a free) per file is pure allocator churn on the hot path.
    static READ_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Read an indexed file's raw bytes into the calling thread's reusable buffer and hand them to `f`.
/// Unreadable files (deleted since the scan, permission denied), a file that now leads out of
/// the repository (`file` is `None`) and one grown past `max_bytes` (`scan.max_file_bytes`, which
/// no indexed file exceeded when it was scanned) yield `None` and are skipped rather than failing
/// the whole grep. UTF-8 validity is the closure's concern, so a file that cannot match is rejected
/// on raw bytes without paying for validation.
fn with_indexed_bytes<R>(file: Option<std::path::PathBuf>, max_bytes: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    let abs = file?;
    let read = |buf: &mut Vec<u8>| -> std::io::Result<bool> {
        let file = std::fs::File::open(&abs)?;
        buf.clear();
        if let Ok(meta) = file.metadata() {
            if meta.len() > max_bytes {
                return Ok(false);
            }
            buf.reserve(usize::try_from(meta.len()).unwrap_or(0).saturating_add(1));
        }
        file.take(max_bytes.saturating_add(1)).read_to_end(buf)?;
        Ok(buf.len() as u64 <= max_bytes)
    };
    READ_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        let outcome = read(&mut buf);
        let result = match outcome {
            Ok(true) => Some(f(&buf)),
            Ok(false) => None,
            Err(e) => {
                tracing::debug!(path = %abs.display(), error = %e, "workspace_grep: skipping unreadable file");
                None
            }
        };
        if buf.capacity() > RETAINED_BUF_CAP {
            *buf = Vec::new();
        }
        result
    })
}

/// Matches of `re` in a file's bytes, or `None` for a file that is not UTF-8 and so cannot be
/// searched. A file the literal prefilter rules out is a zero whatever its encoding.
fn count_matches(bytes: &[u8], re: &LineRegex, literal: Option<&Finder<'static>>) -> Option<u32> {
    if let Some(finder) = literal
        && finder.find(bytes).is_none()
    {
        return Some(0);
    }
    let source = std::str::from_utf8(bytes).ok()?;
    let text = lines_end_at_lf(re, source);
    Some(line_matches(re, &text).count().min(u32::MAX as usize) as u32)
}

/// A byte that is whitespace to the regex engine, matches `.` and ends no line.
const LONE_CR_STAND_IN: u8 = 0x0B;

/// A grep's compiled pattern, and whether a lone `\r` in a file is masked before matching it:
/// decided once per call from the parsed pattern, not once per file.
pub(super) struct LineRegex {
    regex: Regex,
    masks_lone_cr: bool,
}

impl LineRegex {
    pub(super) fn new(regex: Regex) -> Self {
        let masks_lone_cr = !names_carriage_return(regex.as_str());
        Self { regex, masks_lone_cr }
    }
}

impl std::ops::Deref for LineRegex {
    type Target = Regex;

    fn deref(&self) -> &Regex {
        &self.regex
    }
}

/// `source` as the text `re` is matched against, with the same offsets.
///
/// A line ends at `\n`, and a `\r` right before it belongs to the ending: the rule `code_read`,
/// the outline and `git_blame` number lines by, and the one hit lines are computed with here. The
/// CRLF-aware regex also anchors `^` and `$` at a `\r` that no `\n` follows, which would report a
/// line start or end in the middle of a line. Such a `\r` is text inside its line, so it is
/// swapped for a byte that ends no line before matching. Only a file that has one pays for the
/// copy, and a pattern that itself names `\r` (or the stand-in) is matched against the bytes as
/// they are.
fn lines_end_at_lf<'s>(re: &LineRegex, source: &'s str) -> std::borrow::Cow<'s, str> {
    let bytes = source.as_bytes();
    let lone = |at: &usize| bytes.get(at + 1) != Some(&b'\n');
    if !re.masks_lone_cr || !memchr::memchr_iter(b'\r', bytes).any(|at| lone(&at)) {
        return std::borrow::Cow::Borrowed(source);
    }
    let mut masked = bytes.to_vec();
    for at in memchr::memchr_iter(b'\r', bytes).filter(lone) {
        masked[at] = LONE_CR_STAND_IN;
    }
    // One ASCII byte replaced by another: still UTF-8.
    std::borrow::Cow::Owned(String::from_utf8(masked).unwrap_or_else(|_| source.to_string()))
}

/// Whether `pattern` tells a carriage return from the stand-in: either character itself or any
/// escape of it (`\r`, `\x0D`, `\x{0D}`, `\u000D`, `\v`, `\x0B`), alone or in a class, or a class
/// range holding one and not the other. Masking a lone `\r` as the stand-in would change what such
/// a pattern matches: `a\vb` would match `a\rb`. Decided on the parsed pattern, since the spellings
/// are many. A pattern that does not parse here is read as naming one, so its file is searched as
/// it is.
fn names_carriage_return(pattern: &str) -> bool {
    use regex_syntax::ast::{self, Ast, ClassSetItem, Visitor};

    fn is_either(c: char) -> bool {
        c == '\r' || c == char::from(LONE_CR_STAND_IN)
    }
    struct Names(bool);
    impl Visitor for Names {
        type Output = bool;
        type Err = std::convert::Infallible;
        fn finish(self) -> Result<bool, Self::Err> {
            Ok(self.0)
        }
        fn visit_pre(&mut self, ast: &Ast) -> Result<(), Self::Err> {
            if let Ast::Literal(literal) = ast {
                self.0 |= is_either(literal.c);
            }
            Ok(())
        }
        fn visit_class_set_item_pre(&mut self, item: &ClassSetItem) -> Result<(), Self::Err> {
            match item {
                ClassSetItem::Literal(literal) => self.0 |= is_either(literal.c),
                ClassSetItem::Range(range) => {
                    let holds = |c: char| (range.start.c..=range.end.c).contains(&c);
                    self.0 |= holds('\r') != holds(char::from(LONE_CR_STAND_IN));
                }
                _ => {}
            }
            Ok(())
        }
    }
    ast::parse::Parser::new()
        .parse(pattern)
        .map_or(true, |parsed| ast::visit(&parsed, Names(false)).unwrap_or(true))
}

/// The matches of `re` that lie on a line of `source`.
///
/// `^` and `$` anchor at every line, so an empty match is also found at the very end of a file
/// that ends in a newline: after the last line, at a line number the file does not have and
/// `code_read` refuses. The counting pass and the page are both built from this, so the two agree.
pub(super) fn line_matches<'s>(re: &Regex, source: &'s str) -> impl Iterator<Item = regex::Match<'s>> {
    let past_the_last_line = source.ends_with('\n').then_some(source.len());
    re.find_iter(source)
        .filter(move |found| Some(found.start()) != past_the_last_line)
}

/// One file's contribution to the page: its index in the scanned window, how many of its matches the
/// cursor already consumed, and how many to emit.
struct Selection {
    file_idx: usize,
    hit_skip: u32,
    take: usize,
}

/// Walk the per-file counts in order and pick the files that fill the page, plus the packed cursor to
/// resume from when `limit` cut the result short.
fn select_hits(counts: &[u32], limit: usize, skip_files: usize, skip_hits: u32) -> (Vec<Selection>, Option<u64>) {
    let mut selected: Vec<Selection> = Vec::new();
    let mut remaining = limit;

    for (file_idx, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let hit_skip = if file_idx == 0 { skip_hits } else { 0 };
        if remaining == 0 {
            return (selected, Some(pack_cursor(skip_files + file_idx, hit_skip)));
        }
        let take = (count as usize).min(remaining);
        selected.push(Selection {
            file_idx,
            hit_skip,
            take,
        });
        remaining -= take;
        if take < count as usize {
            let consumed = hit_skip.saturating_add(take as u32);
            return (selected, Some(pack_cursor(skip_files + file_idx, consumed)));
        }
    }
    (selected, None)
}

/// Each file is resolved and must still lie inside the repository: a hit carries file content, and
/// an indexed file can have been swapped for a link since the scan.
///
/// Re-read only the files that actually contribute to this page (at most `limit` of them, already
/// warm in the page cache from the counting pass) and build their hits. Flattened in window order,
/// so the page is deterministic and the cursors are monotonic.
///
/// Returns the hits alongside, for each hit, the packed cursor that resumes AT that hit — the token
/// budget uses it to page a dropped tail exactly.
///
/// `is_fresh(file_idx, file, bytes)` says whether the bytes just read from `file` are the content
/// the index holds for it; only then may a hit be laid over the indexed outline.
fn materialize(
    files: IndexedFiles<'_>,
    scanned: &[&RelPath],
    selected: &[Selection],
    is_fresh: &(dyn Fn(usize, &std::path::Path, &[u8]) -> bool + Sync),
    re: &LineRegex,
    include_context: bool,
    skip_files: usize,
) -> (Vec<GrepHit>, Vec<u64>) {
    let per_file: Vec<Vec<(u64, GrepHit)>> = selected
        .par_iter()
        .map(|sel| {
            let path = scanned[sel.file_idx];
            let file = files.indexed_path(path);
            with_indexed_bytes(file.clone(), files.max_file_bytes, |bytes| {
                let Ok(source) = std::str::from_utf8(bytes) else {
                    return Vec::new();
                };
                let fresh = file.as_deref().is_some_and(|file| is_fresh(sel.file_idx, file, bytes));
                collect_hits(path, source, fresh, re, sel, include_context, skip_files + sel.file_idx)
            })
            .unwrap_or_default()
        })
        .collect();

    let total: usize = per_file.iter().map(Vec::len).sum();
    let mut hits = Vec::with_capacity(total);
    let mut cursors = Vec::with_capacity(total);
    for file_hits in per_file {
        for (cursor, hit) in file_hits {
            hits.push(hit);
            cursors.push(cursor);
        }
    }
    (hits, cursors)
}

fn collect_hits(
    path: &RelPath,
    source: &str,
    source_fresh: bool,
    re: &LineRegex,
    sel: &Selection,
    include_context: bool,
    global_file_idx: usize,
) -> Vec<(u64, GrepHit)> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(memchr::memchr_iter(b'\n', source.as_bytes()).map(|pos| pos + 1))
        .collect();

    let mut out = Vec::with_capacity(sel.take);
    let text = lines_end_at_lf(re, source);
    for (ordinal, mat) in line_matches(re, &text).enumerate().skip(sel.hit_skip as usize) {
        if out.len() >= sel.take {
            break;
        }
        let match_start = mat.start();
        let line_idx = line_starts.partition_point(|&ls| ls <= match_start).saturating_sub(1);
        let line_start_byte = line_starts[line_idx];

        let context_before =
            (include_context && line_idx > 0).then(|| clip_line(line_text(source, &line_starts, line_idx - 1), 0, 0));
        let context_after = (include_context && line_idx + 1 < line_starts.len())
            .then(|| clip_line(line_text(source, &line_starts, line_idx + 1), 0, 0));

        out.push((
            pack_cursor(global_file_idx, ordinal as u32),
            GrepHit {
                path: path.clone(),
                line: (line_idx as u32) + 1,
                column: (match_start - line_start_byte) as u32 + 1,
                text: clip_line(
                    line_text(source, &line_starts, line_idx),
                    match_start - line_start_byte,
                    mat.len(),
                ),
                symbol: None,
                context_before,
                context_after,
                start_byte: match_start as u32,
                stale: !source_fresh,
            },
        ));
    }
    out
}

#[cfg(test)]
#[path = "helpers_grep_tests.rs"]
mod tests;
