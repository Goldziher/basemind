//! `run_workspace_grep` helper — kept in its own file so `helpers.rs` stays under the
//! 1000-line cap as the MCP surface grows.

use std::cell::RefCell;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::Ordering;

use memchr::memmem::Finder;
use rayon::prelude::*;
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::ServerState;
use super::cursor::Cursor;
use super::helpers::{SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX};
use super::types::{GrepHit, GrepTruncation, WorkspaceGrepParams, WorkspaceGrepResponse};
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
/// in parallel — reads each as UTF-8 (non-UTF-8 and unreadable files are skipped), and applies the
/// compiled regex. `limit` caps returned HITS, never files scanned, so `total_matches` and
/// `total_files_matched` are exact for the scanned window. Supports in-memory pagination via
/// `cursor` / `next_cursor` on the same `encode_in_memory(offset, generation)` scheme as
/// `list_files`.
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
                        total_files_matched: 0,
                        total_matches: 0,
                        truncated: false,
                        truncation_reason: None,
                        budgeted: false,
                        hits: Vec::new(),
                        next_cursor: None,
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

    let re = Regex::new(&params.pattern).map_err(|e| McpError::invalid_params(format!("invalid regex: {e}"), None))?;

    let literal =
        (regex::escape(&params.pattern) == params.pattern).then(|| Finder::new(params.pattern.as_bytes()).into_owned());

    let path_finder = params.path_contains.as_deref().map(|n| Finder::new(n.as_bytes()));
    let lang_filter = params.language.as_deref();

    let cache = state.shared.cache.load_full();

    // Grep needs only path, language and indexed size — every one of them in the symbol-free file
    // view — so the candidate scan never decodes an outline however large the corpus is.
    let candidates: Vec<(&RelPath, &super::l1_cache::FileMeta)> = cache
        .file_metas()
        .filter(|(path, meta)| {
            path_finder.as_ref().is_none_or(|f| f.find(path.as_bytes()).is_some())
                && lang_filter.is_none_or(|l| *meta.language == *l)
        })
        .collect();

    let window = candidates.get(skip_files.min(candidates.len())..).unwrap_or(&[]);
    let (scanned, byte_budget_hit) = apply_byte_budget(window);

    let root = state.shared.root.as_path();
    let paths: Vec<&RelPath> = scanned.iter().map(|(path, _)| *path).collect();
    // Files the per-file trigram blooms prove cannot match are never opened. They keep their slot in
    // `paths` (counted as zero), so window order, `file_idx` and every cursor are those of the full sweep.
    let skip = if bloom_prefilter_enabled() {
        super::index_route::IndexRoute::resolve(state)
            .await
            .grep_skip(root, &params.pattern, &paths)
            .await
    } else {
        None
    };
    let counts = count_all(root, &paths, skip.as_deref(), &re, literal.as_ref(), skip_hits);

    let total_matches = counts.iter().fold(0u32, |acc, &c| acc.saturating_add(c));
    let total_files_matched = counts.iter().filter(|&&c| c > 0).count();

    let (selected, limit_resume) = select_hits(&counts, limit, skip_files, skip_hits);
    let (hits, hit_cursors) = materialize(root, &paths, &selected, &re, params.include_context, skip_files);

    let (truncated, truncation_reason, next_cursor) = match (limit_resume, byte_budget_hit) {
        (Some(offset), _) => (
            true,
            Some(GrepTruncation::Limit),
            Some(Cursor::encode_in_memory(offset, generation)),
        ),
        (None, true) => (
            true,
            Some(GrepTruncation::ByteBudget),
            Some(Cursor::encode_in_memory(
                pack_cursor(skip_files + scanned.len(), 0),
                generation,
            )),
        ),
        (None, false) => (false, None, None),
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
            total_files_matched,
            total_matches,
            truncated,
            truncation_reason,
            budgeted,
            hits,
            next_cursor,
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
fn count_all(
    root: &Path,
    scanned: &[&RelPath],
    skip: Option<&[bool]>,
    re: &Regex,
    literal: Option<&Finder<'static>>,
    skip_hits: u32,
) -> Vec<u32> {
    let mut counts: Vec<u32> = scanned
        .par_iter()
        .enumerate()
        .map(|(i, path)| {
            if skip.is_some_and(|s| s.get(i).copied().unwrap_or(false)) {
                return 0;
            }
            with_indexed_bytes(root, path, |bytes| count_matches(bytes, re, literal)).unwrap_or(0)
        })
        .collect();
    if let Some(first) = counts.first_mut() {
        *first = first.saturating_sub(skip_hits);
    }
    counts
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
/// Unreadable files (deleted since the scan, permission denied) yield `None` and are skipped rather
/// than failing the whole grep. UTF-8 validity is the closure's concern, so a file that cannot match
/// is rejected on raw bytes without paying for validation.
fn with_indexed_bytes<R>(root: &Path, path: &RelPath, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    let abs = root.join(path);
    let read = |buf: &mut Vec<u8>| -> std::io::Result<()> {
        let mut file = std::fs::File::open(&abs)?;
        buf.clear();
        if let Ok(meta) = file.metadata() {
            buf.reserve(usize::try_from(meta.len()).unwrap_or(0).saturating_add(1));
        }
        file.read_to_end(buf).map(|_| ())
    };
    READ_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        let outcome = read(&mut buf);
        let result = match outcome {
            Ok(()) => Some(f(&buf)),
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

/// Matches of `re` in a file's bytes; non-UTF-8 files count zero (they were never greppable).
fn count_matches(bytes: &[u8], re: &Regex, literal: Option<&Finder<'static>>) -> u32 {
    if let Some(finder) = literal
        && finder.find(bytes).is_none()
    {
        return 0;
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return 0;
    };
    re.find_iter(source).count().min(u32::MAX as usize) as u32
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

/// Re-read only the files that actually contribute to this page (at most `limit` of them, already
/// warm in the page cache from the counting pass) and build their hits. Flattened in window order,
/// so the page is deterministic and the cursors are monotonic.
///
/// Returns the hits alongside, for each hit, the packed cursor that resumes AT that hit — the token
/// budget uses it to page a dropped tail exactly.
fn materialize(
    root: &Path,
    scanned: &[&RelPath],
    selected: &[Selection],
    re: &Regex,
    include_context: bool,
    skip_files: usize,
) -> (Vec<GrepHit>, Vec<u64>) {
    let per_file: Vec<Vec<(u64, GrepHit)>> = selected
        .par_iter()
        .map(|sel| {
            let path = scanned[sel.file_idx];
            with_indexed_bytes(root, path, |bytes| {
                std::str::from_utf8(bytes).map_or_else(
                    |_| Vec::new(),
                    |source| collect_hits(path, source, re, sel, include_context, skip_files + sel.file_idx),
                )
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
    re: &Regex,
    sel: &Selection,
    include_context: bool,
    global_file_idx: usize,
) -> Vec<(u64, GrepHit)> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(memchr::memchr_iter(b'\n', source.as_bytes()).map(|pos| pos + 1))
        .collect();

    let mut out = Vec::with_capacity(sel.take);
    for (ordinal, mat) in re.find_iter(source).enumerate().skip(sel.hit_skip as usize) {
        if out.len() >= sel.take {
            break;
        }
        let match_start = mat.start();
        let line_idx = line_starts.partition_point(|&ls| ls <= match_start).saturating_sub(1);
        let line_start_byte = line_starts[line_idx];

        let context_before = if include_context && line_idx > 0 {
            Some(extract_line(source, &line_starts, line_idx - 1))
        } else {
            None
        };
        let context_after = if include_context && line_idx + 1 < line_starts.len() {
            Some(extract_line(source, &line_starts, line_idx + 1))
        } else {
            None
        };

        out.push((
            pack_cursor(global_file_idx, ordinal as u32),
            GrepHit {
                path: path.clone(),
                line_num: (line_idx as u32) + 1,
                column: (match_start - line_start_byte) as u32,
                matched_text: mat.as_str().to_owned(),
                context_before,
                context_after,
            },
        ));
    }
    out
}

/// Extract the content of line `line_idx` from `source`, stripping the trailing
/// `\n` / `\r\n`. Returns an empty string when the line is empty or the index is
/// out of range.
fn extract_line(source: &str, line_starts: &[usize], line_idx: usize) -> String {
    let start = line_starts[line_idx];
    let end = line_starts.get(line_idx + 1).copied().unwrap_or(source.len());
    let raw = &source[start..end];
    raw.trim_end_matches('\n').trim_end_matches('\r').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_unpacks_a_file_and_hit_ordinal() {
        assert_eq!(unpack_cursor(pack_cursor(0, 0)), (0, 0));
        assert_eq!(unpack_cursor(pack_cursor(68_291, 7)), (68_291, 7));
        assert_eq!(unpack_cursor(pack_cursor(1, u32::MAX)), (1, u32::MAX));
    }

    #[test]
    fn selects_whole_files_until_the_limit_is_filled() {
        let (selected, resume) = select_hits(&[2, 0, 3], 5, 0, 0);
        assert_eq!(selected.len(), 2, "both matching files fit the limit");
        assert_eq!(selected[0].take, 2);
        assert_eq!(selected[1].file_idx, 2);
        assert_eq!(selected[1].take, 3);
        assert!(resume.is_none(), "nothing left over, so no resume cursor");
    }

    #[test]
    fn resumes_inside_a_file_whose_matches_exceed_the_limit() {
        let (selected, resume) = select_hits(&[10], 4, 0, 0);
        assert_eq!(selected[0].take, 4);
        assert_eq!(
            unpack_cursor(resume.expect("limit cut the file short")),
            (0, 4),
            "resume at the 5th match of file 0 — no replay, no loss"
        );
    }

    #[test]
    fn resume_cursor_skips_the_hits_already_returned_from_the_first_file() {
        let (selected, resume) = select_hits(&[6], 4, 3, 4);
        assert_eq!(selected[0].hit_skip, 4, "the first file resumes past 4 consumed hits");
        assert_eq!(selected[0].take, 4);
        assert_eq!(
            unpack_cursor(resume.expect("still more matches in the file")),
            (3, 8),
            "next page starts at the 9th match of candidate file 3"
        );
    }

    #[test]
    fn a_literal_pattern_is_prefiltered_and_a_metacharacter_pattern_is_not() {
        assert_eq!(regex::escape("OptimizationStatus"), "OptimizationStatus");
        assert_ne!(regex::escape("fn (spawn|block)"), "fn (spawn|block)");
    }

    #[test]
    fn counts_every_match_and_the_prefilter_never_changes_the_count() {
        let re = Regex::new("needle").expect("regex");
        let finder = Finder::new("needle".as_bytes()).into_owned();
        let source = b"needle\nhay\nneedle needle\n";
        assert_eq!(count_matches(source, &re, None), 3);
        assert_eq!(count_matches(source, &re, Some(&finder)), 3);
        assert_eq!(count_matches(b"hay only", &re, Some(&finder)), 0);
        assert_eq!(
            count_matches(b"needle \xff", &re, Some(&finder)),
            0,
            "non-UTF-8 is never greppable"
        );
    }

    // ---- equivalence against the pre-optimisation sweep -------------------------------------

    /// The sweep exactly as it was before the buffer-reuse rewrite: `read_to_string` per file, count
    /// pass, then a second read of the selected files. Kept verbatim as the oracle.
    mod reference {
        use super::*;

        pub(super) fn read(root: &Path, path: &RelPath) -> Option<String> {
            std::fs::read_to_string(root.join(path.to_path_buf())).ok()
        }

        pub(super) fn count(
            root: &Path,
            scanned: &[&RelPath],
            re: &Regex,
            literal: Option<&Finder<'static>>,
            skip_hits: u32,
        ) -> Vec<u32> {
            let mut counts: Vec<u32> = scanned
                .iter()
                .map(|p| match read(root, p) {
                    Some(source) => {
                        if let Some(f) = literal
                            && f.find(source.as_bytes()).is_none()
                        {
                            0
                        } else {
                            re.find_iter(&source).count().min(u32::MAX as usize) as u32
                        }
                    }
                    None => 0,
                })
                .collect();
            if let Some(first) = counts.first_mut() {
                *first = first.saturating_sub(skip_hits);
            }
            counts
        }

        pub(super) fn materialize(
            root: &Path,
            scanned: &[&RelPath],
            selected: &[Selection],
            re: &Regex,
            ctx: bool,
            skip_files: usize,
        ) -> (Vec<GrepHit>, Vec<u64>) {
            let mut hits = Vec::new();
            let mut cursors = Vec::new();
            for sel in selected {
                let path = scanned[sel.file_idx];
                let Some(source) = read(root, path) else { continue };
                for (c, h) in collect_hits(path, &source, re, sel, ctx, skip_files + sel.file_idx) {
                    hits.push(h);
                    cursors.push(c);
                }
            }
            (hits, cursors)
        }
    }

    /// Deterministic xorshift so the generated corpus is identical on every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    const WORDS: &[&str] = &[
        "needle",
        "Needle",
        "NEEDLE",
        "hay",
        "fn",
        "def",
        "class",
        "post_processing",
        "naïve",
        "日本語",
        "x",
        "foo_bar",
        "\t",
        "  ",
        "😀",
        "needle needle",
    ];

    fn generate_corpus(dir: &Path) -> Vec<RelPath> {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut paths = Vec::new();
        for i in 0..120 {
            let rel = format!("d{}/f{i}.txt", i % 5);
            std::fs::create_dir_all(dir.join(format!("d{}", i % 5))).expect("mkdir");
            let mut body = String::new();
            // Some files are deliberately dense (more matches than any page limit), some empty.
            let lines = match i % 7 {
                0 => 0,
                1 => 60,
                _ => (rng.next() % 12) as usize,
            };
            let eol = if i % 3 == 0 { "\r\n" } else { "\n" };
            for _ in 0..lines {
                for _ in 0..(1 + rng.next() % 5) {
                    body.push_str(rng.pick(WORDS));
                    body.push(' ');
                }
                body.push_str(eol);
            }
            let bytes = match i {
                // Invalid UTF-8: must count zero in both implementations.
                10 | 11 => {
                    let mut b = body.into_bytes();
                    b.extend_from_slice(b"needle \xff\xfe");
                    b
                }
                _ => body.into_bytes(),
            };
            std::fs::write(dir.join(&rel), bytes).expect("write");
            paths.push(RelPath::from(rel));
        }
        // Indexed but gone from disk, as after a delete between scan and query.
        paths.push(RelPath::from("d0/deleted.txt"));
        paths
    }

    #[test]
    fn the_optimised_sweep_is_byte_identical_to_the_original_over_a_generated_corpus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = generate_corpus(dir.path());
        let refs: Vec<&RelPath> = paths.iter().collect();

        let patterns = [
            "needle",
            "NEEDLE",
            "(?i)needle",
            "needle|hay",
            r"fn\s+\w+",
            "post_processing",
            "naïve",
            "日本語",
            "😀",
            r"\bx\b",
            "^needle",
            "needle$",
            "ne+dle",
            "zzz_absent",
            "(?m)^$",
            "",
        ];
        for pattern in patterns {
            let re = Regex::new(pattern).expect("regex");
            let literal = (regex::escape(pattern) == pattern).then(|| Finder::new(pattern.as_bytes()).into_owned());
            for skip_hits in [0u32, 3] {
                let new_counts = count_all(dir.path(), &refs, None, &re, literal.as_ref(), skip_hits);
                let old_counts = reference::count(dir.path(), &refs, &re, literal.as_ref(), skip_hits);
                assert_eq!(
                    new_counts, old_counts,
                    "counts diverge for {pattern:?} skip_hits={skip_hits}"
                );

                // Walk every page of every limit, comparing hits, cursors and resume points.
                for limit in [25usize, 1000] {
                    for ctx in [false, true] {
                        if skip_hits != 0 && limit != 1000 {
                            continue; // keep the paging walk affordable in debug builds
                        }
                        let (mut skip_files, mut skip_hits) = (0usize, skip_hits);
                        loop {
                            let window = &refs[skip_files.min(refs.len())..];
                            let counts = count_all(dir.path(), window, None, &re, literal.as_ref(), skip_hits);
                            let (sel, resume) = select_hits(&counts, limit, skip_files, skip_hits);
                            let new = materialize(dir.path(), window, &sel, &re, ctx, skip_files);
                            let old_counts = reference::count(dir.path(), window, &re, literal.as_ref(), skip_hits);
                            assert_eq!(counts, old_counts);
                            let old = reference::materialize(dir.path(), window, &sel, &re, ctx, skip_files);
                            assert_eq!(new.1, old.1, "cursors diverge for {pattern:?} limit={limit}");
                            assert_eq!(
                                serde_json::to_string(&new.0).expect("json"),
                                serde_json::to_string(&old.0).expect("json"),
                                "hits diverge for {pattern:?} limit={limit} ctx={ctx}"
                            );
                            let Some(next) = resume else { break };
                            (skip_files, skip_hits) = unpack_cursor(next);
                        }
                    }
                }
            }
        }
    }

    // ---- trigram-bloom prefilter equivalence -------------------------------------------------

    /// Index the current bytes and stat of every readable file, as a scan would.
    fn scan_blooms(db: &crate::index::IndexDb, dir: &Path, paths: &[RelPath]) {
        let mut writer = db.writer();
        for rel in paths {
            let abs = dir.join(rel.to_path_buf());
            let (Ok(bytes), Ok(meta)) = (std::fs::read(&abs), std::fs::metadata(&abs)) else {
                continue;
            };
            let row = crate::index::grep_bloom::build_row(&bytes, meta.len(), crate::scanner_file::mtime_nanos(&meta));
            writer.upsert_grep_bloom(rel, row);
        }
        writer.commit().expect("commit blooms");
    }

    fn set_mtime_ahead(path: &Path, secs: u64) {
        let file = std::fs::OpenOptions::new().write(true).open(path).expect("open");
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(secs))
            .expect("set mtime");
    }

    #[test]
    fn the_bloom_prefilter_never_changes_a_result_even_after_edits_deletes_and_new_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx_dir = tempfile::tempdir().expect("index dir");
        let db = crate::index::IndexDb::open(idx_dir.path()).expect("index");
        let mut paths = generate_corpus(dir.path());

        // A long single line with the needle only at the very end, and a unique token in one file.
        let mut long_line = "abc def ghi ".repeat(20_000);
        long_line.push_str("tail_marker_xyz");
        std::fs::write(dir.path().join("d1/long.txt"), long_line).expect("write long");
        paths.push(RelPath::from("d1/long.txt"));
        std::fs::write(dir.path().join("d2/unique.txt"), "alpha unique_token_qq beta\n").expect("write");
        paths.push(RelPath::from("d2/unique.txt"));

        scan_blooms(&db, dir.path(), &paths);

        // After the scan: an edit that adds a token (size changes), a same-size edit with a bumped
        // mtime, a deletion, and a file the scan never saw.
        std::fs::write(dir.path().join("d3/f3.txt"), "now holds unique_token_qq and needle\n").expect("edit");
        let same_size = dir.path().join("d4/f4.txt");
        let before = std::fs::read(&same_size).expect("read");
        let mut edited = vec![b'q'; before.len()];
        let marker = b"unique_token_qq";
        if edited.len() >= marker.len() {
            edited[..marker.len()].copy_from_slice(marker);
        }
        std::fs::write(&same_size, &edited).expect("same-size edit");
        set_mtime_ahead(&same_size, 5);
        std::fs::remove_file(dir.path().join("d1/f1.txt")).expect("delete");
        std::fs::write(dir.path().join("d0/new.txt"), "brand new unique_token_qq needle\n").expect("new");
        paths.push(RelPath::from("d0/new.txt"));

        let refs: Vec<&RelPath> = paths.iter().collect();
        let patterns = [
            "needle",
            "NEEDLE",
            "(?i)needle",
            "(?i)NeEdLe",
            "needle|hay",
            "post_processing",
            "post_proc",
            "naïve",
            "日本語",
            "😀",
            "unique_token_qq",
            "unique_token_qq|tail_marker_xyz",
            "tail_marker_xyz$",
            "^alpha unique",
            r"unique_token_\w+",
            r"tail_marker_xyz\b",
            "zzz_absent_literal",
            "zzz_absent_literal|needle",
            "(?i)zzz_absent",
            r"fn\s+\w+",
            ".*",
            "ab",
            "",
        ];
        let mut skipped_somewhere = false;
        for pattern in patterns {
            let re = Regex::new(pattern).expect("regex");
            let literal = (regex::escape(pattern) == pattern).then(|| Finder::new(pattern.as_bytes()).into_owned());
            let skip = crate::index::grep_bloom::Needle::compile(pattern)
                .map(|needle| db.grep_skip_verdicts(dir.path(), &refs, &needle));
            skipped_somewhere |= skip.as_ref().is_some_and(|s| s.iter().any(|&x| x));
            for skip_hits in [0u32, 2] {
                let filtered = count_all(dir.path(), &refs, skip.as_deref(), &re, literal.as_ref(), skip_hits);
                let oracle = reference::count(dir.path(), &refs, &re, literal.as_ref(), skip_hits);
                assert_eq!(filtered, oracle, "counts diverge for {pattern:?} skip_hits={skip_hits}");
                for limit in [7usize, 1000] {
                    let (sel_f, resume_f) = select_hits(&filtered, limit, 0, skip_hits);
                    let (sel_o, resume_o) = select_hits(&oracle, limit, 0, skip_hits);
                    assert_eq!(resume_f, resume_o, "cursor diverges for {pattern:?}");
                    let hits_f = materialize(dir.path(), &refs, &sel_f, &re, true, 0);
                    let hits_o = reference::materialize(dir.path(), &refs, &sel_o, &re, true, 0);
                    assert_eq!(hits_f.1, hits_o.1);
                    assert_eq!(
                        serde_json::to_string(&hits_f.0).expect("json"),
                        serde_json::to_string(&hits_o.0).expect("json"),
                        "hits diverge for {pattern:?} limit={limit}"
                    );
                }
            }
        }
        assert!(skipped_somewhere, "the prefilter must actually skip files on this corpus");

        // The stale and new files must stay candidates for the token they now hold.
        let needle = crate::index::grep_bloom::Needle::compile("unique_token_qq").expect("needle");
        let skip = db.grep_skip_verdicts(dir.path(), &refs, &needle);
        for (rel, skipped) in paths.iter().zip(&skip) {
            if ["d3/f3.txt", "d4/f4.txt", "d0/new.txt", "d2/unique.txt"].contains(&rel.to_string().as_str()) {
                assert!(!skipped, "{rel} holds the token and must not be skipped");
            }
        }
        assert!(
            paths.iter().zip(&skip).any(|(_, skipped)| *skipped),
            "files without the token are skipped"
        );
    }

    #[test]
    fn a_missing_or_foreign_bloom_row_is_a_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx_dir = tempfile::tempdir().expect("index dir");
        let db = crate::index::IndexDb::open(idx_dir.path()).expect("index");
        std::fs::write(dir.path().join("a.txt"), "nothing relevant here\n").expect("write");
        let rel = RelPath::from("a.txt");
        let needle = crate::index::grep_bloom::Needle::compile("absent_literal").expect("needle");
        assert_eq!(db.grep_skip_verdicts(dir.path(), &[&rel], &needle), vec![false], "no row");
        let mut writer = db.writer();
        writer.upsert_grep_bloom(&rel, vec![99; 40]);
        writer.commit().expect("commit");
        assert_eq!(db.grep_skip_verdicts(dir.path(), &[&rel], &needle), vec![false], "foreign row");
        scan_blooms(&db, dir.path(), std::slice::from_ref(&rel));
        assert_eq!(db.grep_skip_verdicts(dir.path(), &[&rel], &needle), vec![true], "current row");
        let mut writer = db.writer();
        writer.remove_file(&rel).expect("remove");
        writer.commit().expect("commit");
        assert!(db.grep_bloom_row(&rel).is_none(), "removing the file removes its row");
    }

    #[test]
    fn the_read_buffer_does_not_retain_a_giant_files_capacity() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("big.txt"), vec![b'a'; RETAINED_BUF_CAP * 3]).expect("write");
        let len = with_indexed_bytes(dir.path(), &RelPath::from("big.txt"), <[u8]>::len);
        assert_eq!(len, Some(RETAINED_BUF_CAP * 3));
        READ_BUF.with(|b| assert!(b.borrow().capacity() <= RETAINED_BUF_CAP, "scratch stays bounded"));
    }
}
