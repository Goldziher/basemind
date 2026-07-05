//! Pure helper functions used by the tool methods. Kept out of `mod.rs` so the tool impl
//! block stays focused on dispatch logic. Everything here is `pub(super)`.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;

use super::types::{BlameHunkView, BlameResponse, BlameSymbolResponse, CommitFileView, CommitView};
use super::{OutlineCache, OutlineEntry, ServerState};
use crate::extract::SymbolKind;
use crate::lang::{LangId, ParseOutcome, parse_with_default_timeout, with_parser};

pub(super) use super::helpers_calls::{run_find_callers, run_find_references};
#[cfg(feature = "documents")]
pub(super) use super::helpers_documents::format_response;
pub(super) use super::helpers_graph::run_call_graph;
pub(super) use super::helpers_grep::run_workspace_grep;
pub(super) use super::helpers_impls::run_find_implementations;
pub(super) use super::helpers_telemetry::record_call;

pub(super) const SEARCH_LIMIT_DEFAULT: u32 = 100;
pub(super) const SEARCH_LIMIT_MAX: u32 = 1000;
pub(super) const LIST_LIMIT_DEFAULT: u32 = 200;
pub(super) const LIST_LIMIT_MAX: u32 = 5000;
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

/// Wrap a tool-shim body with telemetry instrumentation.
///
/// Captures `Instant::now()` before the body runs, serializes the params for a deterministic
/// hash, awaits the body, then records the resulting `CallToolResult` (or skips on `Err`) via
/// `record_call`. Each tool's shim becomes a one-liner.
///
/// Usage from `tools.rs` / `tools_memory.rs` / `tools_admin.rs`:
/// ```ignore
/// async fn outline(...) -> Result<CallToolResult, McpError> {
///     instrument_tool!(&self.state, "outline", params, run_outline(&self.state, params).await)
/// }
/// ```
#[macro_export]
macro_rules! instrument_tool {
    ($state:expr, $tool:literal, $params:expr, $body:expr) => {{
        let __started = ::std::time::Instant::now();
        let __params_json = ::serde_json::to_value(&$params).unwrap_or(::serde_json::Value::Null);
        let __result = $body;
        $crate::mcp::helpers::record_call($state, $tool, &__params_json, __started, &__result);
        __result
    }};
}

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
        SymbolKind::Field => "field",
        SymbolKind::Variable => "variable",
        SymbolKind::EnumVariant => "enum_variant",
        SymbolKind::Constructor => "constructor",
        SymbolKind::Decorator => "decorator",
        SymbolKind::Heading => "heading",
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
        "field" => SymbolKind::Field,
        "variable" => SymbolKind::Variable,
        "enum_variant" | "variant" => SymbolKind::EnumVariant,
        "constructor" => SymbolKind::Constructor,
        "decorator" => SymbolKind::Decorator,
        "heading" => SymbolKind::Heading,
        other => {
            return Err(McpError::invalid_params(format!("unknown symbol kind: {other}"), None));
        }
    })
}

pub(super) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let content =
        ContentBlock::json(value).map_err(|e| McpError::internal_error(format!("serialize response: {e}"), None))?;
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
            "this tool requires `basemind serve` to be run inside a git repository",
            None,
        )
    })
}

/// The git-history index, but only when it is present AND indexes exactly the current `head`.
/// History tools use it as a pure accelerator: a stale or absent index returns `None`, and the
/// caller falls back to the live walk, so a result is never served from a HEAD the index hasn't
/// caught up to (or from a rewritten history).
pub(super) fn git_history_if_fresh<'a>(
    state: &'a ServerState,
    head: &str,
) -> Option<&'a crate::git_history::GitHistoryIndex> {
    let index = state.git_history.as_deref()?;
    (index.last_indexed_head_hex().as_deref() == Some(head)).then_some(index)
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
        // Line comment: skip from marker through (and including) the trailing newline.
        if !lc_marker.is_empty() && raw[i..].starts_with(lc_marker) {
            i += lc_marker.len();
            i = memchr::memchr(b'\n', &raw[i..])
                .map(|off| i + off) // stop at the newline; the whitespace branch consumes it next
                .unwrap_or(raw.len());
            continue;
        }
        // Block comment: skip to `*/` via memmem.
        if has_block && raw[i..].starts_with(block_open) {
            i += block_open.len();
            if let Some(finder) = &block_close_finder
                && let Some(off) = finder.find(&raw[i..])
            {
                i = (i + off + block_close.len()).min(raw.len());
            } else {
                // Unterminated block comment — consume the rest of the buffer, matching the
                // original "walk to EOF then bump past the close marker" semantics.
                i = raw.len();
            }
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

/// Page a slice of blame hunks. Skips any hunk whose `start_line <= resume_after` (so the
/// cursor encodes the *last returned* `start_line` — the next page picks up strictly
/// after it). When `limit` is `None` returns every remaining hunk with no cursor (the
/// legacy un-paginated shape). When the page fills exactly at the limit and more hunks
/// follow, returns a cursor with `snapshot_id = 0`.
pub(super) fn paginate_blame_hunks<'a, I>(
    iter: I,
    resume_after: u32,
    limit: Option<u32>,
) -> (Vec<BlameHunkView>, Option<super::cursor::Cursor>)
where
    I: IntoIterator<Item = &'a crate::git::BlameHunk>,
{
    let cap = limit.map(|n| n.min(BLAME_LIMIT_MAX) as usize);
    let mut out: Vec<BlameHunkView> = Vec::new();
    let mut last_line: u32 = 0;
    let mut has_more = false;
    for h in iter {
        if h.start_line <= resume_after {
            continue;
        }
        if let Some(c) = cap
            && out.len() >= c
        {
            has_more = true;
            break;
        }
        last_line = h.start_line;
        out.push(blame_hunk_view(h));
    }
    let next_cursor = if has_more {
        Some(super::cursor::Cursor::encode_in_memory(last_line as u64, 0))
    } else {
        None
    };
    (out, next_cursor)
}

/// Translate a tree-sitter symbol's byte range into a 1-based inclusive
/// `(start_line, end_line)` pair against a specific source blob. We start from L1's
/// `start_row` (0-based row) and add the count of newlines in `(start_byte..end_byte)`
/// for the end. Pure + cheap: one memchr-count, no tree-sitter re-parse.
fn line_range_in_blob(sym: &crate::extract::Symbol, bytes: &[u8]) -> (u32, u32) {
    let start_line = sym.start_row + 1;
    let s = sym.start_byte as usize;
    let e = (sym.end_byte as usize).min(bytes.len());
    let slice = if s < e { &bytes[s..e] } else { &[][..] };
    let newlines = memchr::memchr_iter(b'\n', slice).count() as u32;
    (start_line, start_line + newlines)
}

/// Symbol's 1-based inclusive `(start_line, end_line)` against the *committed HEAD blob* —
/// the blob `blame_file` reads by default, so the range must be derived from the same blob to
/// attribute lines correctly. On a clean tree HEAD equals the working copy and the result is
/// exact; on a dirty tree, reading the HEAD blob (and clamping the end byte to its length)
/// keeps the range bounded to the blamed blob instead of over-attributing on-disk-only lines.
/// Fallbacks for an unborn HEAD: staged blob, then the working-tree file.
pub(super) fn symbol_line_range(
    repo: &crate::git::Repo,
    path: &crate::path::RelPath,
    sym: &crate::extract::Symbol,
) -> (u32, u32) {
    let bytes = path
        .as_str()
        .and_then(|s| repo.read_blob_at_rev("HEAD", s).ok().flatten())
        .or_else(|| path.as_str().and_then(|s| repo.read_blob_staged(s).ok().flatten()))
        // `..`-safety: `path` is a `RelPath` produced by the scanner's strip_prefix(root) or
        // git tree enumeration — neither source ever emits `..` components. A repo-relative key
        // joins under `workdir()`; an external `scan.extra_roots` key is absolute, so
        // `workdir().join(abs)` yields the absolute path unchanged — which correctly reads the
        // real external file (that path is what it was indexed under). Either way, no `..` escape.
        .or_else(|| std::fs::read(repo.workdir().join(path.to_path_buf())).ok())
        .unwrap_or_default();
    line_range_in_blob(sym, &bytes)
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
            next_cursor: None,
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
            next_cursor: None,
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
                format!("unknown hash_mode: {other} (expected normalized|structural|structural_loose)"),
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
    lang: LangId,
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
    lang: LangId,
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
            structural_hash_of_symbol(lang, &entry.source, (s, e), include_literals).map(|h| h.to_vec())
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
    lang: LangId,
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

fn find_node_for_range(root: tree_sitter::Node, start: usize, end: usize) -> Option<tree_sitter::Node> {
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
    lang: LangId,
    hasher: &mut blake3::Hasher,
) {
    if node.is_extra() {
        return;
    }
    let kind_name = node.kind();
    hasher.update(&(kind_name.len() as u32).to_le_bytes());
    hasher.update(kind_name.as_bytes());

    // Iterate named children by index (Node is Copy) — no Vec staging; extras skipped inline.
    let nc = node.named_child_count() as u32;
    let named_count: u32 = (0..nc)
        .filter(|&i| node.named_child(i).is_some_and(|c| !c.is_extra()))
        .count() as u32;

    if named_count == 0 {
        // Leaf-shaped node: emit identifier or (optionally) literal text.
        let emit_text = is_identifier_kind(kind_name) || (include_literals && is_literal_kind(lang, kind_name));
        if emit_text && let Ok(text) = node.utf8_text(source) {
            hasher.update(&(text.len() as u32).to_le_bytes());
            hasher.update(text.as_bytes());
        } else {
            hasher.update(&0u32.to_le_bytes());
        }
        return;
    }
    hasher.update(&named_count.to_le_bytes());
    for i in 0..nc {
        if let Some(child) = node.named_child(i)
            && !child.is_extra()
        {
            walk_structural(child, source, include_literals, lang, hasher);
        }
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

fn is_literal_kind(lang: LangId, kind: &str) -> bool {
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
        "rust" => matches!(
            kind,
            "char_literal"
                | "string_literal"
                | "byte_string_literal"
                | "raw_string_literal"
                | "integer_literal"
                | "float_literal"
                | "boolean_literal"
        ),
        "go" => matches!(
            kind,
            "interpreted_string_literal"
                | "raw_string_literal"
                | "rune_literal"
                | "int_literal"
                | "float_literal"
                | "imaginary_literal"
        ),
        // Conservative default for languages we haven't enumerated: rely on the cross-language
        // literal table above. Adding a language to the override set is the right way to
        // extend this — basemind-style structural hashing is meaningless without per-grammar
        // knowledge of literal node names anyway.
        _ => false,
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

/// Run the scanner in-process and refresh the in-RAM caches, returning the raw
/// [`crate::scanner::ScanReport`]. Shared by the `rescan` MCP tool and the startup
/// auto-scan in [`super::BasemindServer::new`] so both go through the exact same scan +
/// cache-swap path against the server's own `Store` — never releasing or colliding with
/// the Fjall lock the `serve` process already holds.
///
/// Holds the `state.store` write lock for the duration of the scan; MCP query tools block
/// while it runs (acceptable for small/medium repos — rescan is agent-triggered, not
/// per-request). `scoped_paths` limits the scan to those repo-relative paths; `None` runs a
/// full working-tree scan.
pub(super) async fn scan_and_refresh(
    state: Arc<ServerState>,
    scoped_paths: Option<Vec<std::path::PathBuf>>,
    embed: crate::scanner::EmbedMode,
) -> Result<crate::scanner::ScanReport, McpError> {
    // This serve fell back to read-only because another serve owns the write lock for this repo
    // (issue #27). It cannot scan without the lock — return a clean, actionable error instead of
    // attempting a write. The lock-holding serve's watcher keeps the shared index fresh; this
    // serve's reads pick that up via the passive view watcher.
    if state.read_only {
        return Err(McpError::invalid_request(
            "this basemind serve is read-only: another serve process holds the write lock for \
             this repo, so it owns index refresh. Reads are served from the shared index; run \
             rescans from the lock-holding serve (or close it and retry).",
            None,
        ));
    }
    let root = state.root.clone();
    let config = Arc::clone(&state.config);

    // Run the scanner on a blocking thread — fully isolated from the MCP server
    // runtime's TLS so LanceStore's owned tokio runtime can `block_on` without
    // tripping tokio's "runtime within a runtime" check. Xberg + rayon handle
    // their own parallelism here; tokio is intentionally out of this hot path.
    // A scoped batch (watcher) gets an incremental cache delta; a full scan (manual `rescan` tool /
    // boot) gets a full rebuild because it also purges stale keys.
    let was_scoped = scoped_paths.is_some();
    let state_for_scan = Arc::clone(&state);
    let report = tokio::task::spawn_blocking(move || {
        let mut store = state_for_scan.store.blocking_write();
        if let Some(paths) = scoped_paths {
            crate::scanner::scan_paths(&root, &mut store, &config, &paths, embed)
        } else {
            crate::scanner::scan(
                &root,
                &mut store,
                &config,
                crate::scanner::ScanSource::WorkingTree,
                embed,
            )
        }
    })
    .await
    .map_err(|e| McpError::internal_error(format!("scan join: {e}"), None))?
    .map_err(|e| McpError::internal_error(format!("rescan: {e}"), None))?;

    // Watcher batch that changed nothing the scanner indexes — the existing cache already reflects
    // the store. Skip the whole-corpus MapCache rebuild AND the `cache_generation` bump (bumping
    // needlessly resets every paginating client's cursor). This is the hot no-op path the watcher
    // hits on gitignored / nested-`.basemind` churn (issue #33). An explicit `rescan` tool call
    // (`!was_scoped`) is left to fall through so it always refreshes and rolls the snapshot id,
    // preserving the documented "rescan invalidates cursors" contract.
    if was_scoped && report.stats.updated == 0 && report.stats.removed == 0 {
        return Ok(report);
    }

    // The precise delta to apply to the cache, read off the per-file verdicts.
    let updated: Vec<crate::path::RelPath> = report
        .results
        .iter()
        .filter(|r| matches!(r.status, crate::scanner::FileStatus::Updated { .. }))
        .map(|r| crate::path::RelPath::from(r.path.as_str()))
        .collect();
    let removed: Vec<crate::path::RelPath> = report
        .results
        .iter()
        .filter(|r| matches!(r.status, crate::scanner::FileStatus::Removed))
        .map(|r| crate::path::RelPath::from(r.path.as_str()))
        .collect();

    // Refresh MapCache immediately so the next query sees fresh data; don't race the watcher.
    let new_cache = {
        let store = state.store.read().await;
        let corpus_bytes: u64 = store.index.files.values().map(|e| e.size_bytes).sum();
        state
            .corpus_bytes
            .store(corpus_bytes, std::sync::atomic::Ordering::Relaxed);
        let cache = if was_scoped {
            // Incremental: re-read only the changed blobs instead of the whole corpus.
            state.cache.load().with_delta(&store, &updated, &removed)
        } else {
            super::MapCache::build(&store)
        };
        std::sync::Arc::new(cache)
    };
    state.cache.store(new_cache);
    state
        .cache_generation
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    #[cfg(feature = "memory")]
    super::helpers_governance::audit_scope_on_rescan(&state).await;

    Ok(report)
}

/// Body for the `telemetry_summary` MCP tool. Thin wrapper — the aggregation logic
/// lives in [`super::telemetry::summarize`] so this module stays under the line cap.
pub(super) async fn run_telemetry_summary(
    state: &ServerState,
    params: super::types::TelemetrySummaryParams,
) -> Result<CallToolResult, McpError> {
    let response = super::telemetry::summarize(state.telemetry.path(), params).await?;
    json_result(&response)
}

#[cfg(test)]
mod tests {
    use super::normalize_for_history;
    use crate::lang::LangId;
    const RUST: LangId = "rust";
    const PYTHON: LangId = "python";

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

    // ─── structural hash + outline cache (Stage 2) ───────────────────────────

    use super::{HashMode, OutlineCache, outline_entry_for_blob, symbol_fingerprint};
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    fn fresh_cache() -> OutlineCache {
        Mutex::new(lru::LruCache::new(NonZeroUsize::new(8).unwrap()))
    }

    fn fingerprint_for(source: &[u8], lang: LangId, mode: HashMode) -> Vec<u8> {
        let cache = fresh_cache();
        // Synthetic OID — we just need *a* key; real gix::ObjectId from a known sha.
        let oid: gix::ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("synthetic oid");
        let entry = outline_entry_for_blob(&cache, oid, lang, source.to_vec()).expect("outline entry");
        symbol_fingerprint(&entry, "alpha", None, lang, mode).expect("fingerprint")
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
    fn line_range_counts_newlines_against_the_given_blob() {
        use super::line_range_in_blob;
        use crate::extract::{Symbol, SymbolKind};
        let blob = b"// hdr\n\nfn alpha() {\n    body;\n}\nfn beta() {}\n";
        let start = blob.windows(8).position(|w| w == b"fn alpha").unwrap();
        // alpha's body ends at the closing brace; +1 to include the `}` itself, excluding the
        // trailing newline that separates it from `fn beta`.
        let end = blob.iter().position(|&b| b == b'}').unwrap() + 1;
        let sym = Symbol {
            name: "alpha".into(),
            kind: SymbolKind::Function,
            start_byte: start as u32,
            end_byte: end as u32,
            start_row: 2,
            start_col: 0,
            signature: None,
            decorators: Vec::new(),
        };
        assert_eq!(line_range_in_blob(&sym, blob), (3, 5));
        // A dirty working copy that inserted lines must not stretch the range past the blob.
        assert_eq!(line_range_in_blob(&sym, &blob[..start + 4]), (3, 3));
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
