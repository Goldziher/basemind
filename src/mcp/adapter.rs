//! The MCP side of the engine: how an [`EngineError`] is reported to an MCP client.

use rmcp::ErrorData as McpError;

use crate::engine::EngineError;

/// Each variant keeps its message; the code is what the CLI turns into an exit status.
impl From<EngineError> for McpError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::InvalidArgument(message) => McpError::invalid_params(message, None),
            EngineError::Unavailable(message) | EngineError::IndexUnavailable(message) => {
                McpError::invalid_request(message, None)
            }
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::ErrorCode;

    use super::*;

    #[test]
    fn each_engine_error_keeps_its_message_and_maps_to_the_code_the_cli_reads() {
        let cases = [
            (EngineError::invalid_argument("bad path"), ErrorCode::INVALID_PARAMS),
            (EngineError::unavailable("no git"), ErrorCode::INVALID_REQUEST),
            (EngineError::index_unavailable("busy"), ErrorCode::INVALID_REQUEST),
            (EngineError::internal("broken"), ErrorCode::INTERNAL_ERROR),
        ];
        for (error, code) in cases {
            let message = error.to_string();
            let mapped = McpError::from(error);
            assert_eq!(mapped.code, code, "{message}");
            assert_eq!(mapped.message, message);
            assert!(mapped.data.is_none());
        }
        let io = McpError::from(EngineError::from(std::io::Error::other("disk gone")));
        assert_eq!(io.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(io.message, "disk gone");
    }
}
