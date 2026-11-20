//! The stable code and retry flag of a failed tool call.
//!
//! Every `isError` result carries `structuredContent: {"error": <code>, "retryable": <bool>}` next to
//! its message text, so a client tells the failures apart without parsing prose. The code is read
//! off the error a tool body returned: the code it was marked with ([`ErrorKind::data`]), else its
//! JSON-RPC code.

use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};

/// Key of the code: in an error's `data` and in an `isError` result's `structuredContent`.
pub(crate) const ERROR_KEY: &str = "error";

/// Key of the retry flag in an `isError` result's `structuredContent`.
pub(crate) const RETRYABLE_KEY: &str = "retryable";

/// What a failed tool call says the caller can do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// The call cannot be answered as given; the caller changes the arguments.
    InvalidArgument,
    /// The operation is not available in this workspace, session or build.
    Unavailable,
    /// The index cannot be read right now: empty, damaged, or held elsewhere.
    IndexUnavailable,
    /// Another call or process holds what this one needs.
    Busy,
    /// The request was cancelled before it finished.
    Cancelled,
    /// A failure inside basemind the caller cannot fix.
    Internal,
}

/// Every kind, in the order the documentation lists them.
pub(crate) const ALL_KINDS: [ErrorKind; 6] = [
    ErrorKind::InvalidArgument,
    ErrorKind::Unavailable,
    ErrorKind::IndexUnavailable,
    ErrorKind::Busy,
    ErrorKind::Cancelled,
    ErrorKind::Internal,
];

impl ErrorKind {
    /// The wire code.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::Unavailable => "unavailable",
            Self::IndexUnavailable => "index_unavailable",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }

    /// Whether the same call, unchanged, can succeed later: nothing was wrong with the call itself.
    pub(crate) const fn retryable(self) -> bool {
        matches!(self, Self::IndexUnavailable | Self::Busy | Self::Cancelled)
    }

    fn from_code(code: &str) -> Option<Self> {
        ALL_KINDS.into_iter().find(|kind| kind.code() == code)
    }

    /// The kind of a tool body's error: the code its `data` was marked with, else the one its
    /// JSON-RPC code stands for.
    pub(crate) fn of(error: &McpError) -> Self {
        let data = error.data.as_ref();
        if let Some(kind) = data
            .and_then(|data| data.get(ERROR_KEY))
            .and_then(Value::as_str)
            .and_then(Self::from_code)
        {
            return kind;
        }
        if data
            .and_then(|data| data.get(super::BUSY_DATA))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Self::Busy;
        }
        if error.code == ErrorCode::INVALID_PARAMS {
            Self::InvalidArgument
        } else if error.code == ErrorCode::INVALID_REQUEST {
            Self::Unavailable
        } else if error.code == ErrorCode(super::tasks::REQUEST_CANCELLED_CODE) {
            Self::Cancelled
        } else {
            Self::Internal
        }
    }

    /// The `data` that marks an error as this kind, for a kind its JSON-RPC code does not tell apart.
    pub(crate) fn data(self) -> Value {
        json!({ ERROR_KEY: self.code() })
    }

    /// The `structuredContent` of an `isError` result of this kind.
    pub(crate) fn structured(self) -> Value {
        json!({ ERROR_KEY: self.code(), RETRYABLE_KEY: self.retryable() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_has_its_documented_code_and_retry_flag() {
        let table: Vec<(&str, bool)> = ALL_KINDS.iter().map(|kind| (kind.code(), kind.retryable())).collect();
        assert_eq!(
            table,
            [
                ("invalid_argument", false),
                ("unavailable", false),
                ("index_unavailable", true),
                ("busy", true),
                ("cancelled", true),
                ("internal", false),
            ]
        );
    }

    #[test]
    fn a_marked_error_keeps_its_kind_whatever_its_json_rpc_code() {
        for kind in ALL_KINDS {
            let error = McpError::invalid_request("marked", Some(kind.data()));
            assert_eq!(ErrorKind::of(&error), kind, "{}", kind.code());
        }
    }

    #[test]
    fn an_unmarked_error_is_read_off_its_json_rpc_code() {
        let cases = [
            (McpError::invalid_params("bad", None), ErrorKind::InvalidArgument),
            (McpError::invalid_request("no git", None), ErrorKind::Unavailable),
            (
                McpError::invalid_request("held", Some(json!({ super::super::BUSY_DATA: true }))),
                ErrorKind::Busy,
            ),
            (super::super::tasks::request_cancelled(), ErrorKind::Cancelled),
            (McpError::internal_error("broken", None), ErrorKind::Internal),
            (McpError::new(ErrorCode(-32002), "timeout", None), ErrorKind::Internal),
            (
                McpError::internal_error("unknown mark", Some(json!({ ERROR_KEY: "no_such_code" }))),
                ErrorKind::Internal,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(ErrorKind::of(&error), kind, "{}", error.message);
        }
    }

    #[test]
    fn the_structured_content_names_the_code_and_the_retry_flag() {
        assert_eq!(
            ErrorKind::IndexUnavailable.structured(),
            json!({ "error": "index_unavailable", "retryable": true })
        );
        assert_eq!(
            ErrorKind::InvalidArgument.structured(),
            json!({ "error": "invalid_argument", "retryable": false })
        );
    }
}
