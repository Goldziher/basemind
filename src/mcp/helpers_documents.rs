//! Documents-tier helpers — wire-format selection for the `search_documents` family.
//!
//! Extracted from `src/mcp/helpers.rs` so the parent module stays under the per-file
//! line cap. Re-exported via `pub(super) use helpers_documents::*;` in `helpers.rs`.

#[cfg(feature = "documents")]
use rmcp::ErrorData as McpError;
#[cfg(feature = "documents")]
use rmcp::model::{CallToolResult, ContentBlock};

/// Serialize `value` into a `CallToolResult` using the requested wire format.
///
/// `Json` delegates to the existing [`super::helpers::json_result`] helper
/// (Content-type json). `Toon` serializes with `serde_toon::to_string` and wraps
/// the body in a plain `ContentBlock::text` item so agents receive human-readable
/// TOON on the wire.
///
/// Feature-gated behind `documents` because TOON output is only meaningful for
/// document-tier tools; the gate also ensures `serde_toon` is only pulled in
/// when the `documents` feature is active.
#[cfg(feature = "documents")]
pub(crate) fn format_response<T: serde::Serialize>(
    value: &T,
    fmt: crate::config::OutputFormat,
) -> Result<CallToolResult, McpError> {
    match fmt {
        crate::config::OutputFormat::Json => super::helpers::json_result(value),
        crate::config::OutputFormat::Toon => {
            let body =
                serde_toon::to_string(value).map_err(|e| McpError::internal_error(format!("toon: {e}"), None))?;
            Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
        }
    }
}
