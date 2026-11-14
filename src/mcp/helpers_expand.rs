//! Body of the `code` domain's `expand` mode: resolve one symbol in a file's L1 outline and return
//! its raw source span.

use rmcp::ErrorData as McpError;

use super::ServerState;
use super::helpers::{json_result, kind_to_str, parse_kind};
use super::types_code::{ExpandParams, ExpandResponse};
use crate::query;

/// Maximum byte length returned by `expand`. Bodies larger than this are
/// truncated and `truncated = true` is set in the response.
///
/// 128 KiB is generous enough for any real function or class body while keeping
/// MCP response sizes sane. Agents that need more can read the file directly.
const EXPAND_BODY_CAP: usize = 128 * 1024;

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
        let store = state.shared.store.read().await;
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

    let abs = state.shared.root.join(params.path.to_path_buf());
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
