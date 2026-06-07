//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;

use super::ServerState;
use super::types::{BlameHunkView, BlameResponse, BlameSymbolResponse, CommitFileView, CommitView};
use crate::extract::SymbolKind;

pub(super) const SEARCH_LIMIT_DEFAULT: u32 = 100;
pub(super) const SEARCH_LIMIT_MAX: u32 = 1000;
pub(super) const LIST_LIMIT_DEFAULT: u32 = 200;
pub(super) const LIST_LIMIT_MAX: u32 = 5000;
pub(super) const LOG_LIMIT_DEFAULT: u32 = 20;
pub(super) const LOG_LIMIT_MAX: u32 = 100;

pub(super) fn kind_to_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Type => "type",
        SymbolKind::Const => "const",
        SymbolKind::Module => "module",
        SymbolKind::Macro => "macro",
        SymbolKind::Impl => "impl",
        SymbolKind::Unknown => "unknown",
    }
}

pub(super) fn parse_kind(s: &str) -> Result<SymbolKind, McpError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::Type,
        "const" => SymbolKind::Const,
        "module" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        "impl" => SymbolKind::Impl,
        other => {
            return Err(McpError::invalid_params(
                format!("unknown symbol kind: {other}"),
                None,
            ));
        }
    })
}

pub(super) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content = Content::json(value)
        .map_err(|e| McpError::internal_error(format!("serialize response: {e}"), None))?;
    Ok(CallToolResult::success(vec![content]))
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
        sha: c.sha,
        short_sha: c.short_sha,
        summary: c.summary,
        author: c.author,
        author_time_unix: c.author_time_unix,
        files,
    }
}

pub(super) fn require_git_repo(state: &ServerState) -> Result<&Arc<crate::git::Repo>, McpError> {
    state.repo.as_ref().ok_or_else(|| {
        McpError::invalid_request(
            "this tool requires `gitmind serve` to be run inside a git repository",
            None,
        )
    })
}

/// Extract a single symbol's bytes from a file revision and normalize them for stable
/// equality comparison across commits. Used by `symbol_history` to fingerprint the symbol's
/// body so we can diff successive revisions without false positives from auto-formatter
/// churn (prettier, black, gofmt, rustfmt) or comment-only edits.
///
/// Normalization strips line + block comments per language and collapses ASCII whitespace
/// runs to a single space. Caveat: whitespace inside string literals is also collapsed —
/// an acceptable false-negative rate vs the AST-structural alternative, which would couple
/// `symbol_history` stability to grammar/query evolution.
///
/// Returns `None` if extraction fails or the named symbol isn't in the file's outline.
pub(super) fn find_symbol_bytes(
    lang: crate::lang::Lang,
    file_bytes: &[u8],
    name: &str,
    kind: Option<SymbolKind>,
) -> Option<Vec<u8>> {
    let l1 = crate::extract::l1::extract_l1(lang, file_bytes).ok()?;
    let sym = l1
        .symbols
        .into_iter()
        .find(|s| s.name == name && kind.is_none_or(|k| s.kind == k))?;
    let s = sym.start_byte as usize;
    let e = (sym.end_byte as usize).min(file_bytes.len());
    if s >= e {
        return None;
    }
    Some(normalize_for_history(lang, &file_bytes[s..e]))
}

/// Byte-level normalization for symbol-history fingerprints. See `find_symbol_bytes` doc
/// for rationale and caveats. Pulled out so the unit tests below can target it directly.
pub(crate) fn normalize_for_history(lang: crate::lang::Lang, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        // Line comment: skip from marker through (and including) the trailing newline.
        // We don't emit anything — the surrounding-newline collapse below produces the
        // separator if needed.
        let lc_marker = line_comment_marker(lang);
        if !lc_marker.is_empty() && raw[i..].starts_with(lc_marker) {
            i += lc_marker.len();
            while i < raw.len() && raw[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: skip to `*/`. Languages without block comments (Python) report
        // `has_block_comments() == false` and we never enter this branch.
        if has_block_comments(lang) && raw[i..].starts_with(b"/*") {
            i += 2;
            while i + 1 < raw.len() && !(raw[i] == b'*' && raw[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(raw.len());
            continue;
        }
        // Whitespace run → single space (suppressed at the very start).
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
    // Trim trailing space introduced by a trailing whitespace run.
    while out.last() == Some(&b' ') {
        out.pop();
    }
    out
}

fn line_comment_marker(lang: crate::lang::Lang) -> &'static [u8] {
    use crate::lang::Lang;
    match lang {
        Lang::Python => b"#",
        Lang::Rust | Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Go => b"//",
    }
}

fn has_block_comments(lang: crate::lang::Lang) -> bool {
    use crate::lang::Lang;
    match lang {
        Lang::Python => false,
        Lang::Rust | Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Go => true,
    }
}

pub(super) fn blame_hunk_view(h: &crate::git::BlameHunk) -> BlameHunkView {
    BlameHunkView {
        commit_sha: h.commit_sha.clone(),
        short_sha: h.short_sha.clone(),
        start_line: h.start_line,
        len: h.len,
        source_start_line: h.source_start_line,
        author: h.author.clone(),
        author_time_unix: h.author_time_unix,
        summary: h.summary.clone(),
        source_path: h.source_path.clone(),
    }
}

/// Translate a tree-sitter symbol's byte range into a 1-based inclusive
/// `(start_line, end_line)` pair. We start from L1's `start_row` (0-based row) and
/// add the count of newlines in `(start_byte..end_byte)` for the end. Cheap: one
/// filesystem read, one memchr-count, no tree-sitter re-parse.
pub(super) fn symbol_line_range(
    repo: &crate::git::Repo,
    path: &str,
    sym: &crate::extract::Symbol,
) -> (u32, u32) {
    let start_line = sym.start_row + 1;
    // Prefer the working-tree file; fall back to the staged blob if the working copy is gone.
    let bytes = std::fs::read(repo.workdir().join(path))
        .ok()
        .or_else(|| repo.read_blob_staged(path).ok().flatten())
        .unwrap_or_default();
    let s = sym.start_byte as usize;
    let e = (sym.end_byte as usize).min(bytes.len());
    let slice = if s < e { &bytes[s..e] } else { &[][..] };
    let newlines = memchr::memchr_iter(b'\n', slice).count() as u32;
    let end_line = start_line + newlines;
    (start_line, end_line)
}

/// If `err` is a wrapped `GitError::BlameTooLarge`, return a graceful empty response
/// with `truncated_reason="too_large"` so the caller can ship it as a normal MCP success
/// instead of a server-side error. Returns `None` for any other error.
pub(super) fn blame_too_large_response(
    path: &str,
    suspect_sha: &str,
    err: &crate::git_cache::CacheError,
) -> Option<BlameResponse> {
    if matches!(
        err,
        crate::git_cache::CacheError::Git(crate::git::GitError::BlameTooLarge { .. })
    ) {
        Some(BlameResponse {
            path: path.to_string(),
            suspect_sha: suspect_sha.to_string(),
            hunks: Vec::new(),
            truncated: true,
            truncated_reason: Some("too_large"),
        })
    } else {
        None
    }
}

/// Same logic for `blame_symbol`, which carries symbol identity in its response shape.
pub(super) fn blame_symbol_too_large_response(
    path: &str,
    suspect_sha: &str,
    sym: &crate::extract::Symbol,
    line_start: u32,
    line_end: u32,
    err: &crate::git_cache::CacheError,
) -> Option<BlameSymbolResponse> {
    if matches!(
        err,
        crate::git_cache::CacheError::Git(crate::git::GitError::BlameTooLarge { .. })
    ) {
        Some(BlameSymbolResponse {
            path: path.to_string(),
            suspect_sha: suspect_sha.to_string(),
            name: sym.name.clone(),
            kind: kind_to_str(sym.kind).to_string(),
            line_start,
            line_end,
            hunks: Vec::new(),
            truncated: true,
            truncated_reason: Some("too_large"),
        })
    } else {
        None
    }
}

/// Resolve the current HEAD sha string — keys every HEAD-anchored cache entry.
pub(super) fn head_sha(repo: &crate::git::Repo) -> Result<String, McpError> {
    let info = repo
        .info()
        .map_err(|e| McpError::internal_error(format!("HEAD: {e}"), None))?;
    info.head_sha
        .ok_or_else(|| McpError::internal_error("repository has no HEAD", None))
}

#[cfg(test)]
mod tests {
    use super::normalize_for_history;
    use crate::lang::Lang;

    #[test]
    fn rust_whitespace_only_changes_normalize_equal() {
        let a = b"fn foo() {\n    let x = 1;\n}";
        let b = b"fn foo() {\r\n  let   x = 1;\n   }\n";
        assert_eq!(
            normalize_for_history(Lang::Rust, a),
            normalize_for_history(Lang::Rust, b),
            "autoformat-style whitespace changes should normalize to the same bytes"
        );
    }

    #[test]
    fn rust_line_comment_changes_normalize_equal() {
        let a = b"fn foo() { let x = 1; }";
        let b = b"fn foo() {\n    // explain x\n    let x = 1; // trailing\n}";
        assert_eq!(
            normalize_for_history(Lang::Rust, a),
            normalize_for_history(Lang::Rust, b),
            "adding line comments should not register as a symbol-body change"
        );
    }

    #[test]
    fn rust_block_comment_changes_normalize_equal() {
        let a = b"fn foo() { let x = 1; }";
        let b = b"fn foo() { /* docs */ let x = 1; /* trailing */ }";
        assert_eq!(
            normalize_for_history(Lang::Rust, a),
            normalize_for_history(Lang::Rust, b),
            "adding block comments should not register as a symbol-body change"
        );
    }

    #[test]
    fn semantic_change_still_differs() {
        let a = b"fn foo() { let x = 1; }";
        let b = b"fn foo() { let x = 2; }";
        assert_ne!(
            normalize_for_history(Lang::Rust, a),
            normalize_for_history(Lang::Rust, b),
            "a literal value change must still register as different"
        );
    }

    #[test]
    fn python_uses_hash_comments() {
        let a = b"def foo():\n    return 1";
        let b = b"def foo():\n    # comment\n    return 1";
        assert_eq!(
            normalize_for_history(Lang::Python, a),
            normalize_for_history(Lang::Python, b),
        );
    }
}
