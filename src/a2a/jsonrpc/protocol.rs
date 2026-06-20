//! JSON-RPC 2.0 envelope types and A2A error-code mapping.
//!
//! This module defines the wire envelope ([`JsonRpcRequest`] /
//! [`JsonRpcResponse`] / [`JsonRpcError`]) shared by every A2A JSON-RPC method,
//! plus the canonical mapping from basemind's core domain errors
//! ([`FacadeError`](crate::a2a::core::task_facade::FacadeError),
//! [`PushNotificationError`](crate::a2a::core::push_notifications::PushNotificationError),
//! and the DTO-layer [`ConvertError`](crate::a2a::jsonrpc::convert::ConvertError))
//! onto JSON-RPC error codes.
//!
//! The mapping mirrors the *semantics* of the gRPC `Status` mapping in
//! [`crate::a2a::grpc::service`]: a missing task is `TASK_NOT_FOUND`
//! (gRPC `NOT_FOUND`); an illegal/terminal transition is `TASK_NOT_CANCELABLE`
//! (gRPC `FAILED_PRECONDITION`); invalid client input is `INVALID_PARAMS`
//! (gRPC `INVALID_ARGUMENT`); everything else is `INTERNAL_ERROR`.

use serde::{Deserialize, Serialize};

// ── JSON-RPC error codes ──────────────────────────────────────────────────────

// Standard JSON-RPC 2.0 reserved codes (spec §5.1).

/// Invalid JSON was received by the server.
pub(crate) const PARSE_ERROR: i32 = -32700;
/// The JSON sent is not a valid Request object.
pub(crate) const INVALID_REQUEST: i32 = -32600;
/// The requested method does not exist or is not available.
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
/// Invalid method parameter(s).
pub(crate) const INVALID_PARAMS: i32 = -32602;
/// Internal JSON-RPC error.
pub(crate) const INTERNAL_ERROR: i32 = -32603;

// A2A-specific codes (A2A spec, `-32000`..`-32099` server-error range).

/// The referenced task could not be found.
pub(crate) const TASK_NOT_FOUND: i32 = -32001;
/// The task is in a state that does not permit cancellation.
pub(crate) const TASK_NOT_CANCELABLE: i32 = -32002;
/// The agent does not support push notifications.
pub(crate) const PUSH_NOTIFICATION_NOT_SUPPORTED: i32 = -32003;
/// The requested operation is not supported by the agent.
pub(crate) const UNSUPPORTED_OPERATION: i32 = -32004;
/// The requested content type is not supported.
pub(crate) const CONTENT_TYPE_NOT_SUPPORTED: i32 = -32005;
/// The agent returned a response that does not conform to the spec.
pub(crate) const INVALID_AGENT_RESPONSE: i32 = -32006;

// ── Envelope types ────────────────────────────────────────────────────────────

/// An inbound JSON-RPC 2.0 request envelope.
///
/// `id` is kept opaque as a [`serde_json::Value`] (it may be a string, a
/// number, or `null`) and is echoed back verbatim on the matching response.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JsonRpcRequest {
    /// Protocol marker; clients send `"2.0"`.
    #[serde(default)]
    pub(crate) jsonrpc: String,
    /// Opaque correlation id — echoed back on the response unchanged.
    #[serde(default)]
    pub(crate) id: serde_json::Value,
    /// The RPC method name (e.g. `"tasks/get"`).
    pub(crate) method: String,
    /// Method parameters; shape is method-specific.
    #[serde(default)]
    pub(crate) params: serde_json::Value,
}

/// An outbound JSON-RPC 2.0 response envelope.
///
/// Exactly one of `result` / `error` is populated; the other is omitted from
/// the serialized form. `jsonrpc` is always `"2.0"`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcResponse {
    /// Protocol marker; always `"2.0"`.
    pub(crate) jsonrpc: String,
    /// Correlation id echoed from the request.
    pub(crate) id: serde_json::Value,
    /// Success payload, present only on a successful call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
    /// Error payload, present only on a failed call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Build a successful response echoing `id` and carrying `result`.
    pub(crate) fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response echoing `id` and carrying `error`.
    pub(crate) fn failure(id: serde_json::Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcError {
    /// Numeric error code (see the `*_ERROR` / `TASK_*` constants).
    pub(crate) code: i32,
    /// Human-readable, single-sentence error description.
    pub(crate) message: String,
    /// Optional structured detail attached to the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<serde_json::Value>,
}

impl JsonRpcError {
    /// Build an error with `code` and `message` and no `data`.
    pub(crate) fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Build an error with `code`, `message`, and structured `data`.
    pub(crate) fn with_data(
        code: i32,
        message: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

// ── Convenience constructors ──────────────────────────────────────────────────

/// `METHOD_NOT_FOUND` for an unknown `method`.
pub(crate) fn method_not_found(method: &str) -> JsonRpcError {
    JsonRpcError::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
}

/// `INVALID_PARAMS` carrying a human-readable `reason`.
pub(crate) fn invalid_params(reason: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(INVALID_PARAMS, reason)
}

/// `PARSE_ERROR` for malformed JSON in the request body.
pub(crate) fn parse_error() -> JsonRpcError {
    JsonRpcError::new(PARSE_ERROR, "parse error")
}

/// `INTERNAL_ERROR` carrying a human-readable `reason`.
pub(crate) fn internal(reason: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(INTERNAL_ERROR, reason)
}

// ── Domain-error mapping ──────────────────────────────────────────────────────

/// Map a [`FacadeError`](crate::a2a::core::task_facade::FacadeError) to a
/// [`JsonRpcError`], mirroring the gRPC `Status` mapping semantics:
///
/// - [`TaskError::TaskNotFound`](crate::a2a::core::task_manager::TaskError::TaskNotFound)
///   → `TASK_NOT_FOUND` (gRPC `NOT_FOUND`).
/// - [`TaskError::TaskAlreadyTerminal`](crate::a2a::core::task_manager::TaskError::TaskAlreadyTerminal)
///   / [`TaskError::TaskInvalidTransition`](crate::a2a::core::task_manager::TaskError::TaskInvalidTransition)
///   → `TASK_NOT_CANCELABLE` (gRPC `FAILED_PRECONDITION`); these are the
///   client-induced "cannot transition/cancel" cases, and `TaskNotCancelable`
///   is the closest A2A code.
/// - everything else → `INTERNAL_ERROR`.
pub(crate) fn facade_error_to_jsonrpc(
    err: &crate::a2a::core::task_facade::FacadeError,
) -> JsonRpcError {
    use crate::a2a::core::task_facade::FacadeError;
    use crate::a2a::core::task_manager::TaskError;

    match err {
        FacadeError::Task(TaskError::TaskNotFound { .. }) => {
            JsonRpcError::new(TASK_NOT_FOUND, err.to_string())
        }
        FacadeError::Task(
            TaskError::TaskAlreadyTerminal { .. } | TaskError::TaskInvalidTransition { .. },
        ) => JsonRpcError::new(TASK_NOT_CANCELABLE, err.to_string()),
        _ => JsonRpcError::new(INTERNAL_ERROR, err.to_string()),
    }
}

/// Map a
/// [`PushNotificationError`](crate::a2a::core::push_notifications::PushNotificationError)
/// to a [`JsonRpcError`]. `InvalidInput` is client-induced bad input, so it maps
/// to `INVALID_PARAMS` (mirroring gRPC `INVALID_ARGUMENT`).
pub(crate) fn push_error_to_jsonrpc(
    err: &crate::a2a::core::push_notifications::PushNotificationError,
) -> JsonRpcError {
    use crate::a2a::core::push_notifications::PushNotificationError;

    match err {
        PushNotificationError::InvalidInput { reason } => invalid_params(reason.clone()),
    }
}

/// Map a DTO-layer
/// [`ConvertError`](crate::a2a::jsonrpc::convert::ConvertError) to a
/// [`JsonRpcError`]. A conversion failure is malformed client input, so it maps
/// to `INVALID_PARAMS`.
pub(crate) fn convert_error_to_jsonrpc(
    err: &crate::a2a::jsonrpc::convert::ConvertError,
) -> JsonRpcError {
    invalid_params(err.to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn success_serializes_without_error_key() {
        let resp = JsonRpcResponse::success(json!(1), json!({"ok": true}));
        let value = serde_json::to_value(&resp).expect("serialize success");

        assert_eq!(value["jsonrpc"], json!("2.0"));
        assert_eq!(value["id"], json!(1));
        assert_eq!(value["result"], json!({"ok": true}));
        assert!(
            value.get("error").is_none(),
            "success response must omit the error key"
        );
    }

    #[test]
    fn failure_serializes_without_result_key() {
        let resp = JsonRpcResponse::failure(json!("abc"), method_not_found("foo/bar"));
        let value = serde_json::to_value(&resp).expect("serialize failure");

        assert_eq!(value["jsonrpc"], json!("2.0"));
        assert_eq!(value["id"], json!("abc"));
        assert_eq!(value["error"]["code"], json!(METHOD_NOT_FOUND));
        assert!(
            value.get("result").is_none(),
            "failure response must omit the result key"
        );
        assert!(
            value["error"].get("data").is_none(),
            "errors without data must omit the data key"
        );
    }

    #[test]
    fn numeric_id_round_trips_request_to_response() {
        let req: JsonRpcRequest =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": 42, "method": "tasks/get"}))
                .expect("deserialize request");
        assert_eq!(req.id, json!(42));

        let resp = JsonRpcResponse::success(req.id.clone(), json!(null));
        assert_eq!(resp.id, json!(42), "numeric id must echo back unchanged");
    }

    #[test]
    fn string_id_round_trips_request_to_response() {
        let req: JsonRpcRequest =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": "req-7", "method": "tasks/get"}))
                .expect("deserialize request");
        assert_eq!(req.id, json!("req-7"));

        let resp = JsonRpcResponse::failure(req.id.clone(), internal("boom"));
        assert_eq!(
            resp.id,
            json!("req-7"),
            "string id must echo back unchanged"
        );
    }

    #[test]
    fn facade_task_not_found_maps_to_minus_32001() {
        use crate::a2a::core::task_facade::FacadeError;
        use crate::a2a::core::task_manager::TaskError;

        let err = FacadeError::Task(TaskError::TaskNotFound { id: "x".into() });
        let jsonrpc = facade_error_to_jsonrpc(&err);
        assert_eq!(jsonrpc.code, TASK_NOT_FOUND);
        assert_eq!(jsonrpc.code, -32001);
    }

    #[test]
    fn facade_already_terminal_maps_to_minus_32002() {
        use crate::a2a::core::task_facade::FacadeError;
        use crate::a2a::core::task_manager::TaskError;

        let err = FacadeError::Task(TaskError::TaskAlreadyTerminal {
            task_id: "x".into(),
            state: "Completed".into(),
        });
        let jsonrpc = facade_error_to_jsonrpc(&err);
        assert_eq!(jsonrpc.code, TASK_NOT_CANCELABLE);
        assert_eq!(jsonrpc.code, -32002);
    }

    #[test]
    fn facade_invalid_transition_maps_to_not_cancelable() {
        use crate::a2a::core::task_facade::FacadeError;
        use crate::a2a::core::task_manager::TaskError;

        let err = FacadeError::Task(TaskError::TaskInvalidTransition {
            task_id: "x".into(),
            from: "Submitted".into(),
            to: "Completed".into(),
        });
        assert_eq!(facade_error_to_jsonrpc(&err).code, TASK_NOT_CANCELABLE);
    }

    #[test]
    fn method_not_found_has_minus_32601() {
        assert_eq!(method_not_found("foo/bar").code, -32601);
    }

    #[test]
    fn push_invalid_input_maps_to_invalid_params() {
        use crate::a2a::core::push_notifications::PushNotificationError;

        let err = PushNotificationError::InvalidInput {
            reason: "bad url".into(),
        };
        let jsonrpc = push_error_to_jsonrpc(&err);
        assert_eq!(jsonrpc.code, INVALID_PARAMS);
        assert!(jsonrpc.message.contains("bad url"));
    }

    #[test]
    fn with_data_attaches_structured_detail() {
        let err = JsonRpcError::with_data(INVALID_PARAMS, "nope", json!({"field": "id"}));
        let value = serde_json::to_value(&err).expect("serialize error");
        assert_eq!(value["data"], json!({"field": "id"}));
    }
}
