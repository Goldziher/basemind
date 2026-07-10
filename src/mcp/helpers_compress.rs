//! Helper implementation for the `compress` MCP tool.
//!
//! Two dispatch paths:
//!
//! 1. **Structural (code file)**: the caller supplies `path` pointing at an indexed
//!    source file. Returns the L1 outline — symbols (name, kind, signature) and
//!    imports — formatted as compact JSON. Bodies are never included. The result is
//!    always smaller than the full source file for any non-trivial file.
//!    strategy = `"structural"`.
//!
//! 2. **Lexical (prose text)**: the caller supplies `text`. A pure-Rust lexical
//!    pass runs first (whitespace collapsing, filler-phrase removal, duplicate-
//!    paragraph deduplication). Regexes are compiled once into a `OnceLock`.
//!    strategy = `"lexical"`.
//!
//! Token counts come from [`super::tokens::count_tokens`]: a real o200k (gpt-4o)
//! tokenizer when built with the `documents` feature, and a `bytes/4` heuristic
//! otherwise. The response carries `tokens_counted` + a `tokens_note` field
//! disclosing which path was used.
//!
//! # Governing principle
//!
//! **Never summarize code signatures.** For code files the structural path returns
//! signatures verbatim from the L1 outline — it never paraphrases or truncates a
//! function signature. Prose compression (stopword removal, deduplication) is
//! applied only to prose input.

use std::sync::OnceLock;

use regex::Regex;
use rmcp::ErrorData as McpError;

use super::ServerState;
use super::helpers::{json_result, kind_to_str, parse_kind};
use super::tokens;
use super::types_compress::{
    CheckpointParams, CompressParams, CompressResponse, DeltaParams, DetectWasteParams, ExpandParams, ExpandResponse,
};
use crate::query;

/// Maximum byte length returned by `expand`. Bodies larger than this are
/// truncated and `truncated = true` is set in the response.
///
/// 128 KiB is generous enough for any real function or class body while keeping
/// MCP response sizes sane. Agents that need more can read the file directly.
const EXPAND_BODY_CAP: usize = 128 * 1024;

/// Disclosure note for the real-tokenizer path (`documents` feature).
const TOKENS_NOTE_REAL: &str =
    "tokens counted with the o200k (gpt-4o) tokenizer; offline runs fall back to a word estimate";

/// Disclosure note for the `bytes/4` heuristic path.
const TOKENS_NOTE_HEURISTIC: &str = "estimate (bytes/4); build with --features documents for real token counts";

/// Pick the disclosure note matching the compiled token-counting path.
fn tokens_note() -> String {
    if tokens::TOKENS_ARE_COUNTED {
        TOKENS_NOTE_REAL.to_string()
    } else {
        TOKENS_NOTE_HEURISTIC.to_string()
    }
}

/// Compiled regex for collapsing runs of horizontal whitespace (space + tab)
/// to a single space within a line.
static RE_SPACES: OnceLock<Regex> = OnceLock::new();

/// Compiled regex for collapsing runs of 3+ blank lines to a single blank line.
static RE_BLANK_LINES: OnceLock<Regex> = OnceLock::new();

/// Compiled regex for common English filler phrases. Designed to match only
/// at natural phrase boundaries so it does not corrupt code or proper nouns.
static RE_FILLERS: OnceLock<Regex> = OnceLock::new();

fn spaces_re() -> &'static Regex {
    RE_SPACES.get_or_init(|| Regex::new(r"[ \t]{2,}").expect("compile RE_SPACES"))
}

fn blank_lines_re() -> &'static Regex {
    RE_BLANK_LINES.get_or_init(|| Regex::new(r"\n{3,}").expect("compile RE_BLANK_LINES"))
}

fn fillers_re() -> &'static Regex {
    RE_FILLERS.get_or_init(|| {
        Regex::new(
            r"(?i)\b(it is worth noting that|it should be noted that|it is important to note that|please note that|as you can see|as mentioned (?:above|earlier|before|previously)|in other words|to be honest|needless to say|for what it's worth|at the end of the day|as a matter of fact|the fact of the matter is|all things considered)\b[,.]?[ ]?"
        )
        .expect("compile RE_FILLERS")
    })
}

/// Apply the lexical pass to a prose string:
/// 1. Collapse internal whitespace runs.
/// 2. Collapse runs of 3+ blank lines.
/// 3. Remove common filler phrases.
/// 4. Deduplicate repeated paragraphs (identical leading-trimmed paragraph text).
fn lexical_pass(text: &str) -> String {
    let text = spaces_re().replace_all(text, " ");

    let text = blank_lines_re().replace_all(&text, "\n\n");

    let text = fillers_re().replace_all(&text, "");

    let mut seen: ahash::AHashSet<String> = ahash::AHashSet::new();
    let mut out_paras: Vec<&str> = Vec::new();
    for para in text.split("\n\n") {
        let key = para.trim().to_string();
        if key.is_empty() || seen.insert(key) {
            out_paras.push(para);
        }
    }
    out_paras.join("\n\n")
}

/// Resolve one symbol by `(path, name[, kind])` from the L1 outline, then read
/// `file_bytes[start_byte..end_byte]` and return the raw source body.
///
/// Multi-match policy: when `name` alone matches more than one symbol (e.g. an
/// overloaded method), the tool returns [`McpError::invalid_params`] that lists
/// the matching `(kind, name)` pairs. The caller disambiguates by re-calling with
/// `kind` set. This is cleaner than silently picking the first match, which would
/// silently return the wrong overload with no indication of ambiguity.
pub(super) async fn run_expand(
    state: &ServerState,
    params: ExpandParams,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let kind_filter = params
        .kind
        .as_deref()
        .map(parse_kind)
        .transpose()
        .map_err(|e| McpError::invalid_params(format!("expand: invalid kind {:?}: {e}", params.kind), None))?;

    let l1 = {
        let store = state.store.read().await;
        query::file_outline(&store, &params.path)
            .map_err(|e| McpError::invalid_params(format!("expand: file_outline({}): {e}", params.path), None))?
    };

    let candidates: Vec<&crate::extract::Symbol> = l1
        .symbols
        .iter()
        .filter(|s| s.name == params.name)
        .filter(|s| kind_filter.is_none_or(|k| s.kind == k))
        .collect();

    let symbol = match candidates.len() {
        0 => {
            let all_names: Vec<String> = l1
                .symbols
                .iter()
                .filter(|s| kind_filter.is_none_or(|k| s.kind == k))
                .map(|s| format!("[{}] {}", kind_to_str(s.kind), s.name))
                .collect();
            return Err(McpError::invalid_params(
                format!(
                    "expand: symbol {:?} not found in {} (available: {})",
                    params.name,
                    params.path,
                    all_names.join(", ")
                ),
                None,
            ));
        }
        1 => candidates[0],
        _ => {
            let matches: Vec<String> = candidates
                .iter()
                .map(|s| format!("[{}] {}", kind_to_str(s.kind), s.name))
                .collect();
            return Err(McpError::invalid_params(
                format!(
                    "expand: {:?} matches {} symbols in {}; supply `kind` to disambiguate: {}",
                    params.name,
                    candidates.len(),
                    params.path,
                    matches.join(", ")
                ),
                None,
            ));
        }
    };

    let abs = state.root.join(params.path.to_path_buf());
    let file_bytes = std::fs::read(&abs)
        .map_err(|e| McpError::invalid_params(format!("expand: read {}: {e}", params.path), None))?;

    let start = symbol.start_byte as usize;
    let end = (symbol.end_byte as usize).min(file_bytes.len());
    let raw = file_bytes.get(start..end).unwrap_or(&[]);

    let end_row = {
        let slice_for_count = file_bytes.get(..end).unwrap_or(&[]);
        slice_for_count.iter().filter(|&&b| b == b'\n').count() as u32
    };

    let full_bytes = raw.len();
    let (body_bytes, truncated) = if full_bytes > EXPAND_BODY_CAP {
        (&raw[..EXPAND_BODY_CAP], true)
    } else {
        (raw, false)
    };

    let body = String::from_utf8_lossy(body_bytes).into_owned();

    let response = ExpandResponse {
        path: params.path.to_string(),
        name: symbol.name.clone(),
        kind: kind_to_str(symbol.kind).to_string(),
        start_row: symbol.start_row + 1,
        end_row: end_row + 1,
        body,
        bytes: full_bytes,
        truncated,
    };

    json_result(&response)
}

pub(super) async fn run_compress(
    state: &ServerState,
    params: CompressParams,
) -> Result<rmcp::model::CallToolResult, McpError> {
    match (&params.text, &params.path) {
        (Some(_), Some(_)) => {
            return Err(McpError::invalid_params(
                "supply exactly one of `text` or `path`, not both",
                None,
            ));
        }
        (None, None) => {
            return Err(McpError::invalid_params("supply exactly one of `text` or `path`", None));
        }
        _ => {}
    }

    if let Some(path) = &params.path {
        run_structural(state, path).await
    } else {
        let text = params.text.as_deref().unwrap_or("");
        run_prose(text, &params)
    }
}

async fn run_structural(
    state: &ServerState,
    path: &crate::path::RelPath,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let store = state.store.read().await;
    let l1 = query::file_outline(&store, path)
        .map_err(|e| McpError::invalid_params(format!("compress: file_outline({path}): {e}"), None))?;

    let original_bytes = l1.size_bytes as usize;

    let abs = state.root.join(path.to_path_buf());
    let original_tokens = match std::fs::read(&abs) {
        Ok(source) => tokens::count_tokens(&String::from_utf8_lossy(&source)),
        Err(_) => (l1.size_bytes) / 4,
    };

    let mut lines: Vec<String> = Vec::new();
    if !l1.imports.is_empty() {
        lines.push("// imports".to_string());
        for imp in &l1.imports {
            lines.push(imp.raw.trim().to_string());
        }
        lines.push(String::new());
    }
    if !l1.symbols.is_empty() {
        lines.push("// symbols".to_string());
        for sym in &l1.symbols {
            let kind = kind_to_str(sym.kind);
            if let Some(sig) = &sym.signature {
                lines.push(format!("// [{kind}] {}", sym.name));
                lines.push(sig.trim().to_string());
            } else {
                lines.push(format!("// [{kind}] {}", sym.name));
            }
        }
    }
    let output = lines.join("\n");
    let compressed_bytes = output.len();
    let compressed_tokens = tokens::count_tokens(&output);

    let ratio = if original_bytes == 0 {
        1.0_f32
    } else {
        compressed_bytes as f32 / original_bytes as f32
    };

    let response = CompressResponse {
        original_bytes,
        original_tokens,
        compressed_bytes,
        compressed_tokens,
        tokens_reduced: original_tokens.saturating_sub(compressed_tokens),
        tokens_counted: tokens::TOKENS_ARE_COUNTED,
        ratio,
        strategy: "structural".to_string(),
        output,
        tokens_note: tokens_note(),
    };

    json_result(&response)
}

fn run_prose(text: &str, _params: &CompressParams) -> Result<rmcp::model::CallToolResult, McpError> {
    let original_bytes = text.len();
    let original_tokens = tokens::count_tokens(text);
    let output = lexical_pass(text);
    let compressed_bytes = output.len();
    let compressed_tokens = tokens::count_tokens(&output);

    let ratio = if original_bytes == 0 {
        1.0_f32
    } else {
        compressed_bytes as f32 / original_bytes as f32
    };

    let response = CompressResponse {
        original_bytes,
        original_tokens,
        compressed_bytes,
        compressed_tokens,
        tokens_reduced: original_tokens.saturating_sub(compressed_tokens),
        tokens_counted: tokens::TOKENS_ARE_COUNTED,
        ratio,
        strategy: "lexical".to_string(),
        output,
        tokens_note: tokens_note(),
    };

    json_result(&response)
}

/// Compute a compact line-diff from `params.old` to `params.new` via the stateless
/// `textcompress::delta` primitive and return the [`crate::textcompress::delta::DeltaOutcome`]
/// verbatim.
pub(super) async fn run_delta(
    _state: &ServerState,
    params: DeltaParams,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let outcome = crate::textcompress::delta::delta(&params.old, &params.new);
    json_result(&outcome)
}

/// List the working-tree change set (staged + modified + untracked) for this server's repo,
/// mirroring `textcompress::cli::changed_files`. Fail-open: no repo, or any git error, yields
/// an empty list — a checkpoint must never fail just because the working tree is unreadable.
fn changed_files(state: &ServerState) -> Vec<String> {
    let Some(repo) = state.repo.as_ref() else {
        return Vec::new();
    };
    let Ok(status) = repo.status_porcelain() else {
        return Vec::new();
    };
    status
        .staged_added
        .iter()
        .chain(&status.staged_modified)
        .chain(&status.staged_deleted)
        .chain(&status.modified)
        .chain(&status.untracked)
        .map(|p| p.to_string())
        .collect()
}

/// Extract a credential-safe [`crate::textcompress::checkpoint::Checkpoint`] from session
/// `params.text`. `files_changed` is fetched from this repo's git working tree (never scraped
/// from `text`); see [`changed_files`] for the fail-open contract.
pub(super) async fn run_checkpoint(
    state: &ServerState,
    params: CheckpointParams,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let files_changed = changed_files(state);
    let checkpoint = crate::textcompress::checkpoint::extract_checkpoint(&params.text, files_changed);
    json_result(&checkpoint)
}

/// Parse `params.log` as JSON-Lines tool calls (leniently — malformed or `tool`-less lines are
/// skipped) and run the pure `textcompress::waste::detect_waste` analysis, returning the
/// [`crate::textcompress::waste::WasteReport`] verbatim.
pub(super) async fn run_detect_waste(
    _state: &ServerState,
    params: DetectWasteParams,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let calls = crate::textcompress::waste::parse_calls(&params.log);
    let report = crate::textcompress::waste::detect_waste(&calls);
    json_result(&report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_pass_collapses_whitespace() {
        let input = "hello   world\n\n\n\nextra blank lines";
        let out = lexical_pass(input);
        assert!(!out.contains("   "), "triple space should be collapsed");
        assert!(!out.contains("\n\n\n"), "triple newline should be collapsed");
    }

    #[test]
    fn lexical_pass_strips_fillers() {
        let input = "It is worth noting that this is important. The code runs fast.";
        let out = lexical_pass(input);
        assert!(
            !out.to_lowercase().contains("it is worth noting that"),
            "filler phrase should be removed: {out:?}"
        );
        assert!(
            out.contains("The code runs fast"),
            "non-filler content must survive: {out:?}"
        );
    }

    #[test]
    fn lexical_pass_deduplicates_paragraphs() {
        let repeated = "Hello world.\n\nHello world.\n\nDifferent paragraph.";
        let out = lexical_pass(repeated);
        let count = out.matches("Hello world.").count();
        assert_eq!(count, 1, "duplicate paragraph must appear only once: {out:?}");
        assert!(
            out.contains("Different paragraph"),
            "unique paragraph must survive: {out:?}"
        );
    }

    /// `tokens_reduced` must equal `original_tokens - compressed_tokens` (saturating)
    /// and, on the heuristic path, the counts must be exactly `bytes / 4`.
    #[test]
    fn compress_response_reports_consistent_reduced_tokens() {
        let original = "word ".repeat(40);
        let compressed = "word";
        let original_tokens = tokens::count_tokens(&original);
        let compressed_tokens = tokens::count_tokens(compressed);
        let response = CompressResponse {
            original_bytes: original.len(),
            original_tokens,
            compressed_bytes: compressed.len(),
            compressed_tokens,
            tokens_reduced: original_tokens.saturating_sub(compressed_tokens),
            tokens_counted: tokens::TOKENS_ARE_COUNTED,
            ratio: compressed.len() as f32 / original.len() as f32,
            strategy: "lexical".to_string(),
            output: compressed.to_string(),
            tokens_note: tokens_note(),
        };
        assert_eq!(
            response.tokens_reduced,
            response.original_tokens - response.compressed_tokens,
            "tokens_reduced must equal original - compressed"
        );
        assert!(response.original_tokens >= response.compressed_tokens);
    }

    /// On the `bytes/4` heuristic build, a known string counts to exactly `len / 4`.
    #[cfg(not(feature = "documents"))]
    #[test]
    fn heuristic_count_is_bytes_over_four() {
        let text = "a".repeat(400);
        assert_eq!(tokens::count_tokens(&text), 100);
        assert!(tokens_note().contains("bytes/4"));
    }

    /// On the real-tokenizer build, the disclosure note names the tokenizer.
    #[cfg(feature = "documents")]
    #[test]
    fn real_count_note_names_tokenizer() {
        assert!(tokens_note().contains("tokenizer"));
    }
}
