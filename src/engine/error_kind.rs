//! The stable code and retry flag of a failed tool call.
//!
//! Every `isError` result carries `structuredContent: {"error": <code>, "retryable": <bool>}` next to
//! its message text, so a client tells the failures apart without parsing prose. The code is read
//! off the error a tool body returned: the code it was marked with ([`ErrorKind::marked`]), else its
//! JSON-RPC code (read by the MCP adapter).

use serde_json::Value;

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
    /// The index cannot be read right now: empty, damaged or unreadable.
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

    /// The kind an error's `data` was marked with ([`ErrorKind::marked`]), if any.
    pub(crate) fn marked(data: Option<&Value>) -> Option<Self> {
        data.and_then(|data| data.get(ERROR_KEY))
            .and_then(Value::as_str)
            .and_then(Self::from_code)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
    fn a_marked_error_data_keeps_its_kind() {
        for kind in ALL_KINDS {
            let data = json!({ ERROR_KEY: kind.code() });
            assert_eq!(ErrorKind::marked(Some(&data)), Some(kind), "{}", kind.code());
        }
        assert_eq!(ErrorKind::marked(None), None);
        assert_eq!(ErrorKind::marked(Some(&json!({ ERROR_KEY: "no_such_code" }))), None);
    }
}
