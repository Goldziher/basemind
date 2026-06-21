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
//! The token count is estimated as `bytes / 4` (the same `bytes_to_tokens`
//! heuristic used in `src/mcp/savings.rs`). The response carries a `tokens_note`
//! field disclosing this.
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
use super::helpers::{json_result, kind_to_str};
use super::types_compress::{CompressParams, CompressResponse};
use crate::query;

// ─── Token estimate ─────────────────────────────────────────────────────────

/// `bytes / 4` token estimate, matching the heuristic in `src/mcp/savings.rs`.
fn bytes_to_tokens(bytes: usize) -> u64 {
    (bytes / 4) as u64
}

// ─── Lexical-pass regexes (compiled once) ───────────────────────────────────

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
        // Case-insensitive; anchored to word boundaries so we don't clip identifiers.
        // The list is intentionally conservative — prose signal words are never in here.
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
    // Step 1: collapse horizontal whitespace runs within each line.
    let text = spaces_re().replace_all(text, " ");

    // Step 2: collapse runs of blank lines.
    let text = blank_lines_re().replace_all(&text, "\n\n");

    // Step 3: strip common filler phrases.
    let text = fillers_re().replace_all(&text, "");

    // Step 4: dedup identical paragraphs (split on double-newline).
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

// ─── Main entry point ────────────────────────────────────────────────────────

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
            return Err(McpError::invalid_params(
                "supply exactly one of `text` or `path`",
                None,
            ));
        }
        _ => {}
    }

    if let Some(path) = &params.path {
        run_structural(state, path).await
    } else {
        // Safety: we've matched (Some(_), None) above.
        let text = params.text.as_deref().unwrap_or("");
        run_prose(text, &params)
    }
}

// ─── Structural (code file) path ─────────────────────────────────────────────

async fn run_structural(
    state: &ServerState,
    path: &crate::path::RelPath,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let store = state.store.read().await;
    let l1 = query::file_outline(&store, path).map_err(|e| {
        McpError::invalid_params(format!("compress: file_outline({path}): {e}"), None)
    })?;

    // Read the original source bytes to compute the original size.
    let original_bytes = l1.size_bytes as usize;

    // Build the structural output: imports then symbols (name, kind, signature).
    // This mirrors what the `outline` tool returns but in a compact text form
    // rather than the full structured JSON — the agent needs a navigable skeleton,
    // not the original bodies.
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

    let ratio = if original_bytes == 0 {
        1.0_f32
    } else {
        compressed_bytes as f32 / original_bytes as f32
    };

    let response = CompressResponse {
        original_bytes,
        original_tokens_est: bytes_to_tokens(original_bytes),
        compressed_bytes,
        compressed_tokens_est: bytes_to_tokens(compressed_bytes),
        ratio,
        strategy: "structural".to_string(),
        output,
        tokens_note: "estimate (bytes/4); accurate tokenizer pending".to_string(),
    };

    json_result(&response)
}

// ─── Prose path ──────────────────────────────────────────────────────────────

fn run_prose(
    text: &str,
    _params: &CompressParams,
) -> Result<rmcp::model::CallToolResult, McpError> {
    let original_bytes = text.len();
    let output = lexical_pass(text);
    let compressed_bytes = output.len();

    let ratio = if original_bytes == 0 {
        1.0_f32
    } else {
        compressed_bytes as f32 / original_bytes as f32
    };

    let response = CompressResponse {
        original_bytes,
        original_tokens_est: bytes_to_tokens(original_bytes),
        compressed_bytes,
        compressed_tokens_est: bytes_to_tokens(compressed_bytes),
        ratio,
        strategy: "lexical".to_string(),
        output,
        tokens_note: "estimate (bytes/4); accurate tokenizer pending".to_string(),
    };

    json_result(&response)
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_pass_collapses_whitespace() {
        let input = "hello   world\n\n\n\nextra blank lines";
        let out = lexical_pass(input);
        assert!(!out.contains("   "), "triple space should be collapsed");
        assert!(
            !out.contains("\n\n\n"),
            "triple newline should be collapsed"
        );
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
        // The second "Hello world." paragraph should be dropped.
        let count = out.matches("Hello world.").count();
        assert_eq!(
            count, 1,
            "duplicate paragraph must appear only once: {out:?}"
        );
        assert!(
            out.contains("Different paragraph"),
            "unique paragraph must survive: {out:?}"
        );
    }

    #[test]
    fn bytes_to_tokens_matches_savings_heuristic() {
        assert_eq!(bytes_to_tokens(400), 100);
        assert_eq!(bytes_to_tokens(4000), 1000);
        assert_eq!(bytes_to_tokens(0), 0);
    }
}
