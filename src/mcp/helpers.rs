//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;

use super::types::{BlameHunkView, BlameResponse, BlameSymbolResponse, CommitFileView, CommitView};
use super::{OutlineCache, OutlineEntry, ServerState};
use crate::extract::SymbolKind;
use crate::lang::{Lang, ParseOutcome, parse_with_default_timeout, with_parser};

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
        SymbolKind::Namespace => "namespace",
        SymbolKind::Getter => "getter",
        SymbolKind::Setter => "setter",
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
        "namespace" => SymbolKind::Namespace,
        "getter" => SymbolKind::Getter,
        "setter" => SymbolKind::Setter,
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

/// Byte-level normalization for symbol-history fingerprints. Strips line + block comments
/// per language and collapses ASCII whitespace runs to a single space. Caveat: whitespace
/// inside string literals is also collapsed — accepted trade-off for the `Normalized`
/// hash mode. The AST-structural modes (`structural_hash_of_symbol`) avoid the issue.
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
    path: &crate::path::RelPath,
    sym: &crate::extract::Symbol,
) -> (u32, u32) {
    let start_line = sym.start_row + 1;
    // Prefer the working-tree file; fall back to the staged blob if the working copy is gone.
    let bytes = std::fs::read(repo.workdir().join(path.to_path_buf()))
        .ok()
        .or_else(|| {
            path.as_str()
                .and_then(|s| repo.read_blob_staged(s).ok().flatten())
        })
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
    path: &crate::path::RelPath,
    suspect_sha: &str,
    err: &crate::git_cache::CacheError,
) -> Option<BlameResponse> {
    if matches!(
        err,
        crate::git_cache::CacheError::Git(crate::git::GitError::BlameTooLarge { .. })
    ) {
        Some(BlameResponse {
            path: path.clone(),
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
    path: &crate::path::RelPath,
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
            path: path.clone(),
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

/// `symbol_history` fingerprint mode. Picked per-call via the `hash_mode` request param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HashMode {
    /// Default. Byte-level fingerprint after `normalize_for_history` strips comments and
    /// collapses whitespace runs. Cheap, language-aware, but couples to string-literal
    /// whitespace (collapsing inside strings is documented as an accepted trade-off).
    Normalized,
    /// AST-shape fingerprint over the symbol's tree-sitter subtree. Formatter-stable,
    /// comment-stable. Includes literal *contents* so e.g. swapping a string-literal value
    /// still registers as a body change.
    Structural,
    /// Same as `Structural` but ignores literal contents — string/number leaves contribute
    /// only their node kind, not their text. Useful for "did the logic change, ignoring
    /// i18n / docstring churn" workflows. Will produce false negatives for changes that
    /// only modify literal values.
    StructuralLoose,
}

impl HashMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            HashMode::Normalized => "normalized",
            HashMode::Structural => "structural",
            HashMode::StructuralLoose => "structural_loose",
        }
    }
}

pub(super) fn parse_hash_mode(s: &str) -> Result<HashMode, McpError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "normalized" => HashMode::Normalized,
        "structural" => HashMode::Structural,
        "structural_loose" => HashMode::StructuralLoose,
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "unknown hash_mode: {other} (expected normalized|structural|structural_loose)"
                ),
                None,
            ));
        }
    })
}

/// Look up `(oid, lang)` in the outline cache; on miss, parse `source` via `extract_l1` and
/// insert a freshly built `OutlineEntry`. Returns the cached entry either way. Errors
/// surface as `None` so the caller can treat parse failures the same as "blob missing".
pub(super) fn outline_entry_for_blob(
    cache: &OutlineCache,
    oid: gix::ObjectId,
    lang: Lang,
    source: Vec<u8>,
) -> Option<Arc<OutlineEntry>> {
    let key = (oid, lang);
    {
        let mut guard = cache.lock().ok()?;
        if let Some(entry) = guard.get(&key) {
            return Some(Arc::clone(entry));
        }
    }
    // Cache miss: parse outside the lock so a slow extract doesn't block concurrent lookups.
    let map = Arc::new(crate::extract::l1::extract_l1(lang, &source).ok()?);
    let entry = Arc::new(OutlineEntry {
        map,
        source: Arc::new(source),
    });
    let mut guard = cache.lock().ok()?;
    guard.put(key, Arc::clone(&entry));
    Some(entry)
}

/// Compute a symbol-history fingerprint from an outline cache entry, choosing the strategy
/// based on `mode`. Returns the fingerprint as a `Vec<u8>` so the caller can compare
/// successive results with `==` regardless of which mode produced them.
pub(super) fn symbol_fingerprint(
    entry: &OutlineEntry,
    name: &str,
    kind: Option<SymbolKind>,
    lang: Lang,
    mode: HashMode,
) -> Option<Vec<u8>> {
    let sym = entry
        .map
        .symbols
        .iter()
        .find(|s| s.name == name && kind.is_none_or(|k| s.kind == k))?;
    let s = sym.start_byte as usize;
    let e = (sym.end_byte as usize).min(entry.source.len());
    if s >= e {
        return None;
    }
    match mode {
        HashMode::Normalized => Some(normalize_for_history(lang, &entry.source[s..e])),
        HashMode::Structural | HashMode::StructuralLoose => {
            let include_literals = matches!(mode, HashMode::Structural);
            structural_hash_of_symbol(lang, &entry.source, (s, e), include_literals)
                .map(|h| h.to_vec())
        }
    }
}

/// AST-structural fingerprint for a symbol's subtree.
///
/// Re-parses `source` with tree-sitter, finds the node whose byte range matches `range`,
/// then DFS-walks the subtree feeding `(node_kind_name, identifier_or_literal_text)` pairs
/// into a blake3 hasher. Comments and other `is_extra()` nodes are skipped entirely,
/// non-named nodes (anonymous tokens like `{`, `(`) contribute nothing — this is what makes
/// the hash formatter-stable.
///
/// When `include_literals` is false, literal-leaf nodes (strings, numbers, booleans)
/// contribute only their kind name, not their text. Identifiers always contribute their
/// text — renaming a local variable always moves the hash.
fn structural_hash_of_symbol(
    lang: Lang,
    source: &[u8],
    range: (usize, usize),
    include_literals: bool,
) -> Option<[u8; 32]> {
    let outcome = with_parser(lang, |p| parse_with_default_timeout(p, source)).ok()?;
    let tree = match outcome {
        ParseOutcome::Ok(t) => t,
        _ => return None,
    };
    let node = find_node_for_range(tree.root_node(), range.0, range.1)?;
    let mut hasher = blake3::Hasher::new();
    walk_structural(node, source, include_literals, lang, &mut hasher);
    Some(*hasher.finalize().as_bytes())
}

fn find_node_for_range(
    root: tree_sitter::Node,
    start: usize,
    end: usize,
) -> Option<tree_sitter::Node> {
    // Iterative DFS: descend into the smallest enclosing subtree that covers (start, end)
    // exactly, falling back to the smallest enclosing node when no exact match exists.
    let mut best: Option<tree_sitter::Node> = None;
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.start_byte() == start && node.end_byte() == end {
            return Some(node);
        }
        if node.start_byte() <= start && node.end_byte() >= end {
            // Track the smallest covering ancestor as a fallback.
            if best
                .map(|b| (node.end_byte() - node.start_byte()) < (b.end_byte() - b.start_byte()))
                .unwrap_or(true)
            {
                best = Some(node);
            }
            for child in node.children(&mut cursor) {
                if child.start_byte() <= start && child.end_byte() >= end {
                    stack.push(child);
                }
            }
        }
    }
    best
}

fn walk_structural(
    node: tree_sitter::Node,
    source: &[u8],
    include_literals: bool,
    lang: Lang,
    hasher: &mut blake3::Hasher,
) {
    if node.is_extra() {
        return;
    }
    let kind_name = node.kind();
    hasher.update(&(kind_name.len() as u32).to_le_bytes());
    hasher.update(kind_name.as_bytes());

    let mut named_children: Vec<tree_sitter::Node> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() && !child.is_extra() {
            named_children.push(child);
        }
    }
    if named_children.is_empty() {
        // Leaf-shaped node: emit identifier or (optionally) literal text.
        let emit_text =
            is_identifier_kind(kind_name) || (include_literals && is_literal_kind(lang, kind_name));
        if emit_text && let Ok(text) = node.utf8_text(source) {
            hasher.update(&(text.len() as u32).to_le_bytes());
            hasher.update(text.as_bytes());
        } else {
            hasher.update(&0u32.to_le_bytes());
        }
        return;
    }
    hasher.update(&(named_children.len() as u32).to_le_bytes());
    for child in named_children {
        walk_structural(child, source, include_literals, lang, hasher);
    }
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "property_identifier"
            | "type_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "field_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "namespace_identifier"
    )
}

fn is_literal_kind(lang: Lang, kind: &str) -> bool {
    // Cross-language literal node names. Strings dominate.
    if matches!(
        kind,
        "string"
            | "string_fragment"
            | "string_content"
            | "template_string"
            | "template_substitution"
            | "number"
            | "integer"
            | "float"
            | "true"
            | "false"
            | "null"
            | "none"
    ) {
        return true;
    }
    match lang {
        Lang::Rust => matches!(
            kind,
            "char_literal"
                | "string_literal"
                | "byte_string_literal"
                | "raw_string_literal"
                | "integer_literal"
                | "float_literal"
                | "boolean_literal"
        ),
        Lang::Go => matches!(
            kind,
            "interpreted_string_literal"
                | "raw_string_literal"
                | "rune_literal"
                | "int_literal"
                | "float_literal"
                | "imaginary_literal"
        ),
        Lang::Python | Lang::TypeScript | Lang::Tsx | Lang::JavaScript => false,
    }
}

/// Point-lookup a `Call` value in the index by `(path, start_byte)` and return its
/// `(line, column)` as 1-based / 0-based respectively. Falls back to `(0, 0)` when the
/// row/col fields aren't populated (older L2 blobs predating the field's introduction).
pub(super) fn resolve_call_line_col(
    idx: &crate::index::IndexDb,
    rel: &crate::path::RelPath,
    start_byte: u32,
) -> (u32, u32) {
    let key = crate::index::keys::call_by_path(rel, start_byte);
    let value = match idx.calls_by_path.get(key) {
        Ok(Some(v)) => v,
        _ => return (0, 0),
    };
    let call: crate::extract::Call = match rmp_serde::from_slice(&value) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    // `start_row` is 0-based; emit 1-based for line numbers per editor convention.
    (call.start_row + 1, call.start_col)
}

/// Body of the `find_references` MCP tool — pulled out so the `#[tool]` wrapper in
/// `tools.rs` stays small. Takes a snapshot of the IndexDb (cheap clone) so the caller
/// can release the store lock before iterating.
pub(super) fn run_find_references(
    idx: Option<&crate::index::IndexDb>,
    params: super::types::FindReferencesParams,
) -> Result<CallToolResult, McpError> {
    use super::types::FindReferencesResponse;
    let limit = params
        .limit
        .unwrap_or(SEARCH_LIMIT_DEFAULT)
        .min(SEARCH_LIMIT_MAX) as usize;
    let Some(idx) = idx else {
        return json_result(&FindReferencesResponse {
            name: params.name,
            total: 0,
            total_is_partial: false,
            hits: Vec::new(),
        });
    };
    let (total, total_is_partial, hits) = scan_calls_by_name(idx, &params.name, limit)?;
    json_result(&FindReferencesResponse {
        name: params.name,
        total,
        total_is_partial,
        hits,
    })
}

/// Body of the `find_callers` MCP tool. Resolves the definition via the in-RAM cache (the
/// same source `outline` uses) for context, then delegates to the same callee-prefix scan.
pub(super) fn run_find_callers(
    idx: Option<&crate::index::IndexDb>,
    params: super::types::FindCallersParams,
    cache: &super::MapCache,
) -> Result<CallToolResult, McpError> {
    use super::types::{DefinitionView, FindCallersResponse};
    let limit = params
        .limit
        .unwrap_or(SEARCH_LIMIT_DEFAULT)
        .min(SEARCH_LIMIT_MAX) as usize;
    let kind_filter = params.kind.as_deref().map(parse_kind).transpose()?;
    let Some(idx) = idx else {
        return json_result(&FindCallersResponse {
            definition: None,
            total: 0,
            total_is_partial: false,
            hits: Vec::new(),
        });
    };
    let definition: Option<DefinitionView> = cache
        .by_path
        .get(&params.path)
        .and_then(|l1| {
            l1.symbols
                .iter()
                .find(|s| s.name == params.name && kind_filter.is_none_or(|k| s.kind == k))
        })
        .map(|sym| DefinitionView {
            path: params.path.clone(),
            name: sym.name.clone(),
            kind: kind_to_str(sym.kind),
            start_row: sym.start_row,
            start_col: sym.start_col,
        });
    let (total, total_is_partial, hits) = scan_calls_by_name(idx, &params.name, limit)?;
    json_result(&FindCallersResponse {
        definition,
        total,
        total_is_partial,
        hits,
    })
}

/// Shared inner loop for `find_references` / `find_callers`: range-scan the
/// `calls_by_callee` partition with a `name`-prefix, materialize up to `limit` hits, and
/// track the `total` count separately. Caps the scan at `limit * 8` to bound work on
/// extremely common names.
fn scan_calls_by_name(
    idx: &crate::index::IndexDb,
    name: &str,
    limit: usize,
) -> Result<(u32, bool, Vec<super::types::ReferenceHit>), McpError> {
    use super::types::ReferenceHit;
    let prefix = crate::index::keys::calls_by_callee_prefix(name);
    let mut hits: Vec<ReferenceHit> = Vec::with_capacity(limit.min(64));
    let mut total: u32 = 0;
    let mut total_is_partial = false;
    let scan_cap = limit.saturating_mul(8).max(2_000);
    for guard in idx.calls_by_callee.prefix(prefix) {
        let (k, _) = guard
            .into_inner()
            .map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
        let Some((callee, rel, start)) = crate::index::keys::parse_call_by_callee(&k) else {
            continue;
        };
        total += 1;
        if hits.len() < limit {
            let (line, column) = resolve_call_line_col(idx, &rel, start);
            hits.push(ReferenceHit {
                path: rel,
                line,
                column,
                callee,
            });
        }
        if total as usize >= scan_cap {
            total_is_partial = true;
            break;
        }
    }
    Ok((total, total_is_partial, hits))
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

    // ─── structural hash + outline cache (Stage 2) ───────────────────────────

    use super::{HashMode, OutlineCache, outline_entry_for_blob, symbol_fingerprint};
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    fn fresh_cache() -> OutlineCache {
        Mutex::new(lru::LruCache::new(NonZeroUsize::new(8).unwrap()))
    }

    fn fingerprint_for(source: &[u8], lang: Lang, mode: HashMode) -> Vec<u8> {
        let cache = fresh_cache();
        // Synthetic OID — we just need *a* key; real gix::ObjectId from a known sha.
        let oid: gix::ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("synthetic oid");
        let entry =
            outline_entry_for_blob(&cache, oid, lang, source.to_vec()).expect("outline entry");
        symbol_fingerprint(&entry, "alpha", None, lang, mode).expect("fingerprint")
    }

    #[test]
    fn structural_hash_ignores_formatter_and_comments() {
        let a = b"pub fn alpha() {\n    let x = 1;\n    x + 1\n}\n";
        let b = b"pub fn   alpha() { /* doc */\n    let  x  =  1;  // explain\n    x + 1\n}\n";
        assert_eq!(
            fingerprint_for(a, Lang::Rust, HashMode::Structural),
            fingerprint_for(b, Lang::Rust, HashMode::Structural),
            "structural hash must be stable under formatting + comment edits"
        );
    }

    #[test]
    fn structural_hash_catches_literal_change() {
        let a = b"pub fn alpha() {\n    let x = 1;\n    x + 1\n}\n";
        let b = b"pub fn alpha() {\n    let x = 2;\n    x + 1\n}\n";
        assert_ne!(
            fingerprint_for(a, Lang::Rust, HashMode::Structural),
            fingerprint_for(b, Lang::Rust, HashMode::Structural),
            "Structural mode must register a literal value change as a body change"
        );
    }

    #[test]
    fn structural_loose_ignores_literal_change() {
        let a = b"pub fn alpha() {\n    let x = 1;\n    x + 1\n}\n";
        let b = b"pub fn alpha() {\n    let x = 2;\n    x + 1\n}\n";
        assert_eq!(
            fingerprint_for(a, Lang::Rust, HashMode::StructuralLoose),
            fingerprint_for(b, Lang::Rust, HashMode::StructuralLoose),
            "StructuralLoose must ignore literal value churn"
        );
    }

    #[test]
    fn structural_loose_still_catches_identifier_rename() {
        let a = b"pub fn alpha() {\n    let original = 1;\n    original + 1\n}\n";
        let b = b"pub fn alpha() {\n    let renamed = 1;\n    renamed + 1\n}\n";
        assert_ne!(
            fingerprint_for(a, Lang::Rust, HashMode::StructuralLoose),
            fingerprint_for(b, Lang::Rust, HashMode::StructuralLoose),
            "StructuralLoose must still catch identifier renames"
        );
    }

    #[test]
    fn outline_cache_returns_same_arc_for_same_oid() {
        let cache = fresh_cache();
        let oid: gix::ObjectId = "0000000000000000000000000000000000000002".parse().unwrap();
        let src = b"pub fn alpha() {}\n".to_vec();
        let a = outline_entry_for_blob(&cache, oid, Lang::Rust, src.clone()).unwrap();
        let b = outline_entry_for_blob(&cache, oid, Lang::Rust, src).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "second lookup must return the same cached Arc"
        );
    }
}
