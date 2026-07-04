//! Telemetry-row recording for tool calls — the `record_call` hook every `#[tool]` shim invokes.
//!
//! Split out of `helpers.rs` to keep that file within the per-file size budget. `record_call`
//! stays reachable as `super::helpers::record_call` via a re-export in `helpers.rs`, so every
//! tool call site is unchanged.

use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use super::ServerState;

/// The response text of a tool result, so telemetry can route it through the real tokenizer; the
/// byte length is recovered as `.len()` for the rows that report raw response bytes. Image /
/// resource / link content is skipped — basemind tools only ever return text.
///
/// Borrows on the common single-`Content::Text` path (every tool that goes through
/// `json_result` returns exactly one item), so `record_call` allocates nothing there; only a
/// multi-content result pays one concatenation.
fn result_text(result: &CallToolResult) -> std::borrow::Cow<'_, str> {
    let mut texts = result.content.iter().filter_map(|c| match c {
        ContentBlock::Text(t) => Some(t.text.as_str()),
        _ => None,
    });
    let Some(first) = texts.next() else {
        return std::borrow::Cow::Borrowed("");
    };
    match texts.next() {
        None => std::borrow::Cow::Borrowed(first),
        Some(second) => {
            let mut text = String::with_capacity(first.len() + second.len());
            text.push_str(first);
            text.push_str(second);
            for rest in texts {
                text.push_str(rest);
            }
            std::borrow::Cow::Owned(text)
        }
    }
}

/// Record one tool-call row to `.basemind/telemetry.jsonl`. Best-effort:
/// errors are logged via `tracing::warn!` and swallowed so a misbehaving
/// telemetry write can never break a tool response. Only successful calls
/// produce rows — error responses don't carry a meaningful "saved" number.
pub(super) fn record_call(
    state: &ServerState,
    tool: &'static str,
    params: &Value,
    started: Instant,
    result: &Result<CallToolResult, McpError>,
) {
    let Ok(r) = result else { return };
    let elapsed_ms: u64 = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    // Full response text in hand → route token counts through the real tokenizer (o200k under
    // `documents`, bytes/4 otherwise). The raw response-byte figure stays the UTF-8 length.
    let resp_text = result_text(r);
    let resp_bytes = resp_text.len() as u64;
    let corpus = state.corpus_bytes.load(std::sync::atomic::Ordering::Relaxed);
    let savings = super::savings::estimate_from_text(tool, corpus, resp_text.as_ref());
    state.telemetry.record(tool, params, resp_bytes, elapsed_ms, &savings);
}
