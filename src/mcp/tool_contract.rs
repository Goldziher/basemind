//! The contract every verb tool shares (ADR-0018): how a call is answered, how a failure is
//! reported, and what every advertised tool definition carries.

use std::future::Future;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{CallToolResult, ContentBlock, ErrorCode, MetaObject};
use serde_json::{Value, json};

use super::BasemindServer;
use super::strict::{Strict, ToolArgs};
use crate::engine::error_kind::{ERROR_KEY, ErrorKind, RETRYABLE_KEY};
use crate::engine::redaction::Redaction;

/// `_meta` key that keeps a tool's schema loaded in hosts that defer MCP tools and load only names.
pub(crate) const ALWAYS_LOAD_META_KEY: &str = "anthropic/alwaysLoad";

/// The `data` that marks an error as `kind`, for a kind its JSON-RPC code does not tell apart.
pub(super) fn kind_data(kind: ErrorKind) -> Value {
    json!({ ERROR_KEY: kind.code() })
}

/// The `structuredContent` of an `isError` result of `kind`.
pub(super) fn kind_structured(kind: ErrorKind) -> Value {
    json!({ ERROR_KEY: kind.code(), RETRYABLE_KEY: kind.retryable() })
}

/// Redact against the repository `root`, this process's data home and the user's home directory.
fn redaction_for(root: &std::path::Path) -> Redaction {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    Redaction::new(root, crate::data_root::DataRoot::current().path(), home.as_deref())
}

/// A failure the caller can act on, as a tool result rather than a JSON-RPC error: the message as
/// text, and the failure's code and retry flag as `structuredContent`.
pub(super) fn tool_error(kind: ErrorKind, message: impl Into<String>) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(message.into())]);
    result.structured_content = Some(kind_structured(kind));
    result
}

/// The kind of a tool body's error: the code its `data` was marked with, else the one its
/// JSON-RPC code stands for.
pub(super) fn kind_of(error: &McpError) -> ErrorKind {
    let data = error.data.as_ref();
    if let Some(kind) = ErrorKind::marked(data) {
        return kind;
    }
    if data
        .and_then(|data| data.get(super::BUSY_DATA))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return ErrorKind::Busy;
    }
    if error.code == ErrorCode::INVALID_PARAMS {
        ErrorKind::InvalidArgument
    } else if error.code == ErrorCode::INVALID_REQUEST {
        ErrorKind::Unavailable
    } else if error.code == ErrorCode(super::tasks::REQUEST_CANCELLED_CODE) {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Internal
    }
}

/// `error` with its message redacted, the unredacted message logged when it named a host path.
pub(super) fn scrub(redaction: &Redaction, mut error: McpError) -> McpError {
    let redacted = redaction.apply(&error.message);
    if redacted != error.message {
        tracing::warn!(error = %error.message, "a tool error named host paths; its result names them redacted");
        error.message = redacted.into();
    }
    error
}

/// Report a tool body's error as an `isError` result. JSON-RPC errors are left to the protocol
/// layer (unknown tool, malformed request, cancellation), which never reaches a tool body.
pub(super) fn into_tool_result(result: Result<CallToolResult, McpError>) -> CallToolResult {
    match result {
        Ok(result) => result,
        Err(error) => tool_error(kind_of(&error), error.message.into_owned()),
    }
}

impl BasemindServer {
    /// Answer one verb-tool call: reject inexact arguments, run the body, and report any failure as
    /// an `isError` result whose text names no absolute host path (`crate::engine::redaction`).
    pub(super) async fn answer<T, F, Fut>(&self, args: Strict<T>, body: F) -> Result<CallToolResult, McpError>
    where
        T: ToolArgs,
        F: FnOnce(T) -> Fut,
        Fut: Future<Output = Result<CallToolResult, McpError>>,
    {
        // ~keep A CLI session keeps the error as an error: its code is the CLI's exit status.
        match args.0 {
            Ok(params) if self.cli => body(params).await,
            Ok(params) => {
                Ok(into_tool_result(body(params).await.map_err(|error| {
                    scrub(&redaction_for(&self.state.shared.root), error)
                })))
            }
            Err(message) if self.cli => Err(McpError::invalid_params(message, None)),
            Err(message) => Ok(tool_error(ErrorKind::InvalidArgument, message)),
        }
    }
}

/// Finish the assembled router: mark every verb tool always-loaded and strip schema keywords that
/// cost bytes on every session without telling a model anything.
///
/// The marker goes on the verb tools only. `agents` is still a mode-dispatched domain tool and
/// stays deferred, so a session that never coordinates with another agent does not pay for it.
/// Applied to the router rather than per
/// `#[tool(meta = …)]` so a verb tool added later cannot forget the marker;
/// `tests/mcp_schema_wire.rs` asserts both halves.
pub(super) fn finalize_router(mut router: ToolRouter<BasemindServer>) -> ToolRouter<BasemindServer> {
    for route in router.map.values_mut() {
        if super::types_tools::VERB_TOOLS.contains(&route.attr.name.as_ref()) {
            let mut meta = route.attr.meta.take().unwrap_or_else(MetaObject::new);
            meta.0.insert(ALWAYS_LOAD_META_KEY.to_string(), Value::Bool(true));
            route.attr.meta = Some(meta);
        }

        let mut schema = Value::Object((*route.attr.input_schema).clone());
        compact_schema(&mut schema, true);
        if let Value::Object(object) = schema {
            route.attr.input_schema = std::sync::Arc::new(object);
        }
    }
    router
}

/// Drop `$schema` / the root `title`, integer `format` / `minimum`, the `null` alternative of an
/// optional field and its `"default": null`. Optional fields are already absent from `required`, which
/// is what a model reads.
pub(super) fn compact_schema(value: &mut Value, root: bool) {
    let Value::Object(object) = value else {
        return;
    };
    if root {
        object.remove("$schema");
        object.remove("title");
    }
    if object.get("type").is_some_and(|kind| kind == "integer") || type_includes(object, "integer") {
        object.remove("format");
        object.remove("minimum");
    }
    if object.get("default").is_some_and(Value::is_null) {
        object.remove("default");
    }
    if let Some(Value::Array(kinds)) = object.get("type") {
        let concrete: Vec<Value> = kinds.iter().filter(|kind| *kind != "null").cloned().collect();
        if concrete.len() == 1 && kinds.len() == 2 {
            object.insert("type".to_string(), concrete[0].clone());
        }
    }
    if let Some(Value::Array(variants)) = object.get_mut("enum") {
        variants.retain(|variant| !variant.is_null());
    }
    for (key, child) in object.iter_mut() {
        match child {
            Value::Object(children) if key == "properties" => {
                for property in children.values_mut() {
                    compact_schema(property, false);
                }
            }
            Value::Object(_) if key == "items" => compact_schema(child, false),
            _ => {}
        }
    }
}

fn type_includes(object: &serde_json::Map<String, Value>, wanted: &str) -> bool {
    matches!(object.get("type"), Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compacting_keeps_what_a_model_reads_and_drops_the_rest() {
        let mut schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "CodeGrepParams",
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string", "description": "Text to find." },
                "limit": { "type": ["integer", "null"], "format": "uint32", "minimum": 0, "description": "Max.", "default": null },
                "tags": { "type": ["array", "null"], "items": { "type": "string" }, "default": null },
                "lane": { "type": ["string", "null"], "enum": ["hybrid", null] },
                "context": { "type": "integer", "default": 2 }
            }
        });
        compact_schema(&mut schema, true);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string", "description": "Text to find." },
                    "limit": { "type": "integer", "description": "Max." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "lane": { "type": "string", "enum": ["hybrid"] },
                    "context": { "type": "integer", "default": 2 }
                }
            })
        );
    }

    #[test]
    fn a_body_error_becomes_an_is_error_result_carrying_its_message_and_code() {
        let result = into_tool_result(Err(McpError::invalid_params("run `index_rescan`", None)));
        assert_eq!(result.is_error, Some(true));
        let text = match result.content.first() {
            Some(ContentBlock::Text(text)) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert_eq!(text, "run `index_rescan`");
        assert_eq!(
            result.structured_content,
            Some(json!({ "error": "invalid_argument", "retryable": false }))
        );
    }

    /// One engine error per variant, through the adapter's mapping and the shim's report, comes back
    /// as an `isError` result carrying that variant's code.
    #[test]
    fn each_engine_error_variant_is_reported_with_its_code() {
        use crate::engine::EngineError;

        let cases = [
            (EngineError::invalid_argument("bad path"), "invalid_argument", false),
            (EngineError::unavailable("no git"), "unavailable", false),
            (
                EngineError::index_unavailable("the index is empty"),
                "index_unavailable",
                true,
            ),
            (EngineError::busy("held"), "busy", true),
            (EngineError::Cancelled, "cancelled", true),
            (EngineError::internal("broken"), "internal", false),
        ];
        for (error, code, retryable) in cases {
            let message = error.to_string();
            let result = into_tool_result(Err(McpError::from(error)));
            assert_eq!(result.is_error, Some(true), "{code}");
            assert_eq!(
                result.structured_content,
                Some(json!({ "error": code, "retryable": retryable })),
                "{code}"
            );
            let text = match result.content.first() {
                Some(ContentBlock::Text(text)) => text.text.clone(),
                other => panic!("expected text content, got {other:?}"),
            };
            assert_eq!(text, message, "{code}");
        }
    }
}
