//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;

use super::ServerState;
use super::types::{BlameHunkView, CommitFileView, CommitView};
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

/// Extract a single symbol's bytes from a file revision. Used by `symbol_history` to
/// fingerprint the symbol's body at a given commit so we can diff successive revisions.
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
    Some(file_bytes[s..e].to_vec())
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

/// Resolve the current HEAD sha string — keys every HEAD-anchored cache entry.
pub(super) fn head_sha(repo: &crate::git::Repo) -> Result<String, McpError> {
    let info = repo
        .info()
        .map_err(|e| McpError::internal_error(format!("HEAD: {e}"), None))?;
    info.head_sha
        .ok_or_else(|| McpError::internal_error("repository has no HEAD", None))
}
