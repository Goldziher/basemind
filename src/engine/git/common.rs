//! What the git operations share: page sizes, the commit and blame-hunk views, a workspace's
//! repository and fresh git-history index, HEAD's snapshot id, and the comment-blind normalizer
//! symbol-history fingerprints compare.

use std::sync::Arc;

use crate::engine::error::EngineError;
use crate::engine::timing::elapsed_us;
use crate::engine::types::git::{
    BlameHunkView, BlameHunks, BlameResponse, BlameSymbolResponse, CommitFileView, CommitView,
};
use crate::engine::workspace::Workspace;
use crate::lang::LangId;

pub(super) const LOG_LIMIT_DEFAULT: u32 = 20;
pub(super) const LOG_LIMIT_MAX: u32 = 100;
/// Hard ceiling on the underlying commit walk depth when paginating commit-iterator tools.
/// `limit` (page size) ≤ `LOG_LIMIT_MAX` and the cursor offset is bounded by this constant
/// minus one page, so an agent cannot drive the walk arbitrarily deep through pagination.
pub(super) const LOG_WALK_MAX: usize = 10_000;
/// Default page size for the blame helpers. Selected to fit a screenful of hunks while
/// keeping the response under ~32 KiB. Opt-in: blame tools only paginate when `limit` is
/// explicitly set.
pub(super) const BLAME_LIMIT_MAX: u32 = 1000;
/// Blame hunks per page when the caller passes no `limit`.
pub(super) const BLAME_LIMIT_DEFAULT: u32 = 100;

/// Hex digits of a commit sha in a `git_log` list. Twelve is the abbreviation the Linux kernel
/// settled on: a prefix that long names one commit in any repository of a realistic size, and every
/// tool that takes a revision resolves it.
pub(super) const SHA_ABBREV: usize = 12;

/// `sha` cut to [`SHA_ABBREV`] hex digits.
pub(super) fn abbreviated_sha(mut sha: String) -> String {
    sha.truncate(SHA_ABBREV);
    sha
}

pub(super) fn commit_to_view(c: crate::git::CommitInfo, include_files: bool) -> CommitView {
    let files = if include_files {
        Some(
            c.files
                .into_iter()
                .map(|(path, kind)| CommitFileView {
                    path,
                    change: kind.as_str(),
                })
                .collect(),
        )
    } else {
        None
    };
    CommitView {
        sha: abbreviated_sha(c.sha),
        summary: c.summary,
        author: c.author,
        with_author: false,
        author_time_unix: c.author_time_unix,
        files,
    }
}

pub(super) fn require_git_repo(ws: &Workspace) -> Result<&Arc<crate::git::Repo>, EngineError> {
    ws.repo.as_ref().ok_or_else(|| {
        EngineError::unavailable("this tool requires `basemind serve` to be run inside a git repository")
    })
}

/// The git-history index, but only when it is present AND indexes exactly the current `head`.
/// History tools use it as a pure accelerator: a stale or absent index returns `None`, and the
/// caller falls back to the live walk, so a result is never served from a HEAD the index hasn't
/// caught up to (or from a rewritten history). An index built in a shallow clone since deepened is
/// stale too, though it indexes `head`: it ends at the old boundary. A stale index asks for a sync
/// ([`Workspace::request_history_sync`]), so a commit made while the server runs is indexed for the
/// calls after it.
pub(super) fn git_history_if_fresh<'a>(
    ws: &'a Workspace,
    head: &str,
) -> Option<&'a crate::git_history::GitHistoryIndex> {
    let index = ws.git_history.as_deref()?;
    let deepened = || {
        ws.repo
            .as_deref()
            .is_some_and(|repo| crate::git_history::builder::root_deepened(index, repo))
    };
    if index.last_indexed_head_hex().as_deref() == Some(head) && !deepened() {
        return Some(index);
    }
    ws.request_history_sync();
    None
}

/// Derive a stable 32-bit snapshot id from a HEAD sha. Used as the in-memory cursor's
/// `snapshot_id` for git-iterator tools — mismatch on resume means HEAD moved between
/// pages, so the caller must restart pagination.
///
/// Reads the first 4 bytes of the hex-encoded sha as a big-endian `u32`. Any non-hex or
/// short sha falls back to 0; the worst-case collision rate on a real sha space is
/// ~1/2^32, well below the noise floor of "rescan happened between calls".
pub(super) fn head_snapshot_id(head_sha: &str) -> u32 {
    let bytes = head_sha.as_bytes();
    if bytes.len() < 8 {
        return 0;
    }
    let mut out: u32 = 0;
    for &b in &bytes[..8] {
        let nibble = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return 0,
        };
        out = (out << 4) | (nibble as u32);
    }
    out
}

/// Byte-level normalization for symbol-history fingerprints. Strips line + block comments
/// per language and collapses ASCII whitespace runs to a single space. Caveat: whitespace
/// inside string literals is also collapsed — accepted trade-off for the `Normalized`
/// hash mode. The AST-structural modes (`structural_hash_of_symbol`) avoid the issue.
///
/// Hot loop: the per-language `lc_marker` / `has_block_comments` lookups are hoisted out of
/// the byte loop (they depend on `lang` only). Once we hit a comment-opening token we use
/// `memchr` (line comments → next `\n`) and `memmem::find` (block comments → next `*/`) to
/// skip the body in one cache-friendly call rather than walking a byte at a time. On warm
/// bodies of a few KiB this turns a per-byte branch loop into a SIMD-able scan.
pub(crate) fn normalize_for_history(lang: LangId, raw: &[u8]) -> Vec<u8> {
    let lc_marker = line_comment_marker(lang);
    let block_open: &[u8] = b"/*";
    let block_close: &[u8] = b"*/";
    let has_block = has_block_comments(lang);
    let block_close_finder = if has_block {
        Some(memchr::memmem::Finder::new(block_close))
    } else {
        None
    };

    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if !lc_marker.is_empty() && raw[i..].starts_with(lc_marker) {
            i += lc_marker.len();
            i = memchr::memchr(b'\n', &raw[i..]).map(|off| i + off).unwrap_or(raw.len());
            continue;
        }
        if has_block && raw[i..].starts_with(block_open) {
            i += block_open.len();
            if let Some(finder) = &block_close_finder
                && let Some(off) = finder.find(&raw[i..])
            {
                i = (i + off + block_close.len()).min(raw.len());
            } else {
                i = raw.len();
            }
            continue;
        }
        if raw[i].is_ascii_whitespace() {
            if !out.is_empty() && out.last() != Some(&b' ') {
                out.push(b' ');
            }
            while i < raw.len() && raw[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    while out.last() == Some(&b' ') {
        out.pop();
    }
    out
}

/// Line-comment marker for symbol-history normalization. Returns `b""` for languages we
/// don't model — the caller's check `!lc_marker.is_empty()` then short-circuits the
/// line-stripping branch and the raw bytes flow through, which is the right behavior for
/// languages outside the override set (we don't know their comment syntax).
fn line_comment_marker(lang: LangId) -> &'static [u8] {
    match lang {
        "python" | "ruby" | "shell" | "bash" | "yaml" | "toml" | "make" => b"#",
        "rust" | "typescript" | "tsx" | "javascript" | "go" | "cpp" | "c" | "java" | "csharp" | "kotlin" | "swift"
        | "scala" | "zig" => b"//",
        _ => b"",
    }
}

/// Whether `/* … */` block comments apply. Returns `false` conservatively for languages
/// we haven't enumerated — the normalizer then leaves block-comment-looking byte runs alone.
fn has_block_comments(lang: LangId) -> bool {
    matches!(
        lang,
        "rust"
            | "typescript"
            | "tsx"
            | "javascript"
            | "go"
            | "cpp"
            | "c"
            | "java"
            | "csharp"
            | "kotlin"
            | "swift"
            | "scala"
            | "css"
            | "json"
    )
}

pub(super) fn blame_hunk_view(h: &crate::git::BlameHunk) -> BlameHunkView {
    BlameHunkView {
        commit_sha: h.commit_sha.clone(),
        start_line: h.start_line,
        end_line: h.start_line + h.len.saturating_sub(1),
        author: h.author.clone(),
        author_time_unix: h.author_time_unix,
        summary: h.summary.clone(),
        source_path: h.source_path.clone(),
    }
}

/// Whether `next` continues `last`: the next lines, from the same commit and source path. git
/// splits a commit's lines where they came from different places in it, which a reader of "who
/// changed these lines" does not need, so such a run is one hunk.
fn continues(last: &BlameHunkView, next: &BlameHunkView) -> bool {
    last.commit_sha == next.commit_sha && last.source_path == next.source_path && last.end_line + 1 == next.start_line
}

/// One page of blame hunks.
pub(super) struct BlamePage {
    pub hunks: Vec<BlameHunkView>,
    pub next_cursor: Option<crate::engine::cursor::Cursor>,
    /// The token budget, not `limit`, ended the page.
    pub budgeted: bool,
}

/// Page a run of blame hunks. Skips any hunk whose `start_line <= resume_after` (the cursor encodes
/// the *last returned* hunk's `end_line`, so the next page picks up strictly after it), merges each
/// run of hunks one commit wrote ([`continues`]), keeps at most `limit` merged hunks (default
/// [`BLAME_LIMIT_DEFAULT`], max [`BLAME_LIMIT_MAX`]) and trims them to `max_tokens`. `snapshot` is
/// stamped into the cursor so a page of another revision refuses it.
pub(super) fn paginate_blame_hunks<'a, I>(
    iter: I,
    resume_after: u32,
    limit: Option<u32>,
    max_tokens: Option<u32>,
    snapshot: u32,
) -> BlamePage
where
    I: IntoIterator<Item = &'a crate::git::BlameHunk>,
{
    let cap = limit.unwrap_or(BLAME_LIMIT_DEFAULT).min(BLAME_LIMIT_MAX) as usize;
    let mut out: Vec<BlameHunkView> = Vec::new();
    let mut has_more = false;
    for h in iter {
        if h.start_line <= resume_after {
            continue;
        }
        let hunk = blame_hunk_view(h);
        if let Some(last) = out.last_mut()
            && continues(last, &hunk)
        {
            last.end_line = hunk.end_line;
        } else if out.len() >= cap {
            has_more = true;
            break;
        } else {
            out.push(hunk);
        }
    }
    // A hunk costs its row, plus its commit's author and summary the first time the page names it.
    let mut named: ahash::AHashSet<String> = ahash::AHashSet::new();
    let budget = crate::engine::budget::keep_prefix(out, max_tokens, |hunk| {
        let commit = if named.insert(hunk.commit_sha.clone()) {
            hunk.author.len() + hunk.summary.len() + 48
        } else {
            0
        };
        crate::engine::tokens::bytes_to_tokens((hunk.row().len() + 3 + commit) as u64)
    });
    let next_cursor = (has_more || budget.budgeted)
        .then(|| budget.items.last().map(|hunk| hunk.end_line))
        .flatten()
        .map(|last_line| crate::engine::cursor::Cursor::encode_in_memory(u64::from(last_line), snapshot));
    BlamePage {
        hunks: budget.items,
        next_cursor,
        budgeted: budget.budgeted,
    }
}

/// Where a blame page resumes: the `start_line` its cursor carries, or an error when the cursor was
/// issued for another revision of the file.
pub(super) fn blame_resume_after(
    cursor: Option<&crate::engine::cursor::Cursor>,
    snapshot: u32,
) -> Result<u32, EngineError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (offset, issued_for) = cursor.decode_in_memory()?;
    if issued_for != snapshot {
        return Err(crate::engine::cursor::stale_cursor("git_blame"));
    }
    Ok(offset.min(u64::from(u32::MAX)) as u32)
}

/// If `err` is a wrapped `GitError::BlameTooLarge`, return a graceful empty response
/// with `truncated_reason="too_large"` so the caller can ship it as a normal MCP success
/// instead of a server-side error. Returns `None` for any other error.
pub(super) fn blame_too_large_response(
    err: &crate::git_cache::CacheError,
    started: std::time::Instant,
) -> Option<BlameResponse> {
    if matches!(
        err,
        crate::git_cache::CacheError::Git(crate::git::GitError::BlameTooLarge { .. })
    ) {
        Some(BlameResponse {
            hunks: BlameHunks::default(),
            budgeted: false,
            truncated: true,
            truncated_reason: Some("too_large"),
            next_cursor: None,
            elapsed_us: elapsed_us(started),
        })
    } else {
        None
    }
}

/// Same logic for `blame_symbol`, which carries the symbol's line span in its response shape.
pub(super) fn blame_symbol_too_large_response(
    line_start: u32,
    line_end: u32,
    err: &crate::git_cache::CacheError,
    started: std::time::Instant,
) -> Option<BlameSymbolResponse> {
    if matches!(
        err,
        crate::git_cache::CacheError::Git(crate::git::GitError::BlameTooLarge { .. })
    ) {
        Some(BlameSymbolResponse {
            line_start,
            line_end,
            hunks: BlameHunks::default(),
            budgeted: false,
            truncated: true,
            truncated_reason: Some("too_large"),
            next_cursor: None,
            elapsed_us: elapsed_us(started),
        })
    } else {
        None
    }
}

/// Resolve the current HEAD sha string — keys every HEAD-anchored cache entry.
pub(super) fn head_sha(repo: &crate::git::Repo) -> Result<String, EngineError> {
    let info = repo.info().map_err(|e| EngineError::internal(format!("HEAD: {e}")))?;
    info.head_sha
        .ok_or_else(|| EngineError::internal("repository has no HEAD"))
}

#[cfg(test)]
mod tests {
    use super::normalize_for_history;
    use crate::lang::LangId;
    const RUST: LangId = "rust";
    const PYTHON: LangId = "python";

    fn hunk(sha: &str, start_line: u32, end_line: u32, source: Option<&str>) -> crate::git::BlameHunk {
        crate::git::BlameHunk {
            commit_sha: sha.repeat(40 / sha.len()),
            short_sha: sha.repeat(7 / sha.len()),
            start_line,
            len: end_line - start_line + 1,
            source_start_line: start_line,
            author: "a".into(),
            author_time_unix: 1,
            summary: "s".into(),
            source_path: source.map(Into::into),
        }
    }

    fn sample() -> Vec<crate::git::BlameHunk> {
        vec![
            hunk("a", 1, 1, None),
            hunk("a", 2, 3, None),
            hunk("b", 4, 4, None),
            hunk("a", 5, 5, None),
            hunk("a", 6, 6, Some("old.rs")),
            hunk("a", 8, 8, Some("old.rs")),
        ]
    }

    /// Adjacent lines of one commit are one hunk; a gap, another commit or another source path
    /// starts a new one. Each commit's author, time and summary are written once.
    #[test]
    fn adjacent_hunks_of_one_commit_merge_and_commits_are_written_once() {
        let page = super::paginate_blame_hunks(&sample(), 0, None, None, 0);
        let written = serde_json::to_string(&super::BlameHunks(page.hunks)).unwrap();
        assert_eq!(
            written,
            r#"{"hunks":["1-3 aaaaaaaaaaaa","4 bbbbbbbbbbbb","5 aaaaaaaaaaaa","6 aaaaaaaaaaaa old.rs","8 aaaaaaaaaaaa old.rs"],"commits":{"aaaaaaaaaaaa":{"author":"a","time":1,"summary":"s"},"bbbbbbbbbbbb":{"author":"a","time":1,"summary":"s"}}}"#
        );
        assert!(page.next_cursor.is_none());
    }

    /// `limit` counts the hunks as written, after merging, and the next page starts past the last
    /// line of the last one, not inside it.
    #[test]
    fn limit_counts_merged_hunks_and_the_cursor_resumes_past_them() {
        let hunks = sample();
        let first = super::paginate_blame_hunks(&hunks, 0, Some(2), None, 0);
        let rows: Vec<String> = first.hunks.iter().map(super::BlameHunkView::row).collect();
        assert_eq!(rows, ["1-3 aaaaaaaaaaaa", "4 bbbbbbbbbbbb"]);
        let (resume, _) = first.next_cursor.expect("more hunks").decode_in_memory().unwrap();
        assert_eq!(resume, 4);
        let second = super::paginate_blame_hunks(&hunks, resume as u32, Some(2), None, 0);
        let rows: Vec<String> = second.hunks.iter().map(super::BlameHunkView::row).collect();
        assert_eq!(rows, ["5 aaaaaaaaaaaa", "6 aaaaaaaaaaaa old.rs"]);
        let (resume, _) = second.next_cursor.expect("one more").decode_in_memory().unwrap();
        let third = super::paginate_blame_hunks(&hunks, resume as u32, Some(2), None, 0);
        let rows: Vec<String> = third.hunks.iter().map(super::BlameHunkView::row).collect();
        assert_eq!(rows, ["8 aaaaaaaaaaaa old.rs"]);
        assert!(third.next_cursor.is_none());
    }

    #[test]
    fn rust_whitespace_only_changes_normalize_equal() {
        let a = b"fn foo() {\n    let x = 1;\n}";
        let b = b"fn foo() {\r\n  let   x = 1;\n   }\n";
        assert_eq!(
            normalize_for_history(RUST, a),
            normalize_for_history(RUST, b),
            "autoformat-style whitespace changes should normalize to the same bytes"
        );
    }

    #[test]
    fn rust_line_comment_changes_normalize_equal() {
        let a = b"fn foo() { let x = 1; }";
        let b = b"fn foo() {\n    // explain x\n    let x = 1; // trailing\n}";
        assert_eq!(
            normalize_for_history(RUST, a),
            normalize_for_history(RUST, b),
            "adding line comments should not register as a symbol-body change"
        );
    }

    #[test]
    fn rust_block_comment_changes_normalize_equal() {
        let a = b"fn foo() { let x = 1; }";
        let b = b"fn foo() { /* docs */ let x = 1; /* trailing */ }";
        assert_eq!(
            normalize_for_history(RUST, a),
            normalize_for_history(RUST, b),
            "adding block comments should not register as a symbol-body change"
        );
    }

    #[test]
    fn semantic_change_still_differs() {
        let a = b"fn foo() { let x = 1; }";
        let b = b"fn foo() { let x = 2; }";
        assert_ne!(
            normalize_for_history(RUST, a),
            normalize_for_history(RUST, b),
            "a literal value change must still register as different"
        );
    }

    #[test]
    fn python_uses_hash_comments() {
        let a = b"def foo():\n    return 1";
        let b = b"def foo():\n    # comment\n    return 1";
        assert_eq!(normalize_for_history(PYTHON, a), normalize_for_history(PYTHON, b),);
    }

    use super::super::fingerprint::{HashMode, outline_entry_for_blob, symbol_fingerprint};
    use crate::engine::outline_cache::OutlineCache;
    use std::sync::Arc;

    fn fresh_cache() -> OutlineCache {
        OutlineCache::with_bounds(8, u64::MAX)
    }

    fn fingerprint_for(source: &[u8], lang: LangId, mode: HashMode) -> Vec<u8> {
        let cache = fresh_cache();
        let oid: gix::ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("synthetic oid");
        let entry = outline_entry_for_blob(&cache, oid, lang, source.to_vec()).expect("outline entry");
        symbol_fingerprint(&entry, "alpha", None, 0, lang, mode).expect("fingerprint")
    }

    #[test]
    fn structural_hash_ignores_formatter_and_comments() {
        let a = b"pub fn alpha() {\n    let x = 1;\n    x + 1\n}\n";
        let b = b"pub fn   alpha() { /* doc */\n    let  x  =  1;  // explain\n    x + 1\n}\n";
        assert_eq!(
            fingerprint_for(a, RUST, HashMode::Structural),
            fingerprint_for(b, RUST, HashMode::Structural),
            "structural hash must be stable under formatting + comment edits"
        );
    }

    #[test]
    fn structural_hash_catches_literal_change() {
        let a = b"pub fn alpha() {\n    let x = 1;\n    x + 1\n}\n";
        let b = b"pub fn alpha() {\n    let x = 2;\n    x + 1\n}\n";
        assert_ne!(
            fingerprint_for(a, RUST, HashMode::Structural),
            fingerprint_for(b, RUST, HashMode::Structural),
            "Structural mode must register a literal value change as a body change"
        );
    }

    #[test]
    fn structural_loose_ignores_literal_change() {
        let a = b"pub fn alpha() {\n    let x = 1;\n    x + 1\n}\n";
        let b = b"pub fn alpha() {\n    let x = 2;\n    x + 1\n}\n";
        assert_eq!(
            fingerprint_for(a, RUST, HashMode::StructuralLoose),
            fingerprint_for(b, RUST, HashMode::StructuralLoose),
            "StructuralLoose must ignore literal value churn"
        );
    }

    #[test]
    fn structural_loose_still_catches_identifier_rename() {
        let a = b"pub fn alpha() {\n    let original = 1;\n    original + 1\n}\n";
        let b = b"pub fn alpha() {\n    let renamed = 1;\n    renamed + 1\n}\n";
        assert_ne!(
            fingerprint_for(a, RUST, HashMode::StructuralLoose),
            fingerprint_for(b, RUST, HashMode::StructuralLoose),
            "StructuralLoose must still catch identifier renames"
        );
    }

    #[test]
    fn outline_cache_returns_same_arc_for_same_oid() {
        let cache = fresh_cache();
        let oid: gix::ObjectId = "0000000000000000000000000000000000000002".parse().unwrap();
        let src = b"pub fn alpha() {}\n".to_vec();
        let a = outline_entry_for_blob(&cache, oid, RUST, src.clone()).unwrap();
        let b = outline_entry_for_blob(&cache, oid, RUST, src).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second lookup must return the same cached Arc");
    }
}
