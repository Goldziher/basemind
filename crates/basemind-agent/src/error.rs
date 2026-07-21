//! Engine error type.

use thiserror::Error;

/// Errors surfaced by the agent engine.
///
/// LLM transport errors from liter-llm are wrapped via [`AgentError::Llm`]; note that
/// `liter_llm::LiterLlmError` is not `Clone`, so neither is `AgentError`.
#[derive(Debug, Error)]
pub enum AgentError {
    /// An error from the underlying liter-llm client (network, provider, auth, streaming).
    #[error("llm: {0}")]
    Llm(#[from] liter_llm::LiterLlmError),

    /// A tool's JSON arguments failed to deserialize into its typed `Args`.
    #[error("tool `{tool}` arguments: {source}")]
    ToolArgs {
        /// The tool whose arguments were malformed.
        tool: &'static str,
        /// The underlying deserialization error.
        #[source]
        source: serde_json::Error,
    },

    /// A basemind code-map tool returned an error.
    #[error("code-map tool `{tool}`: {message}")]
    CodeMap {
        /// The tool that failed.
        tool: &'static str,
        /// The underlying message.
        message: String,
    },

    /// The in-flight turn or tool call was cancelled.
    #[error("cancelled")]
    Cancelled,

    /// The session's cost budget was exhausted.
    #[error("cost budget exceeded")]
    BudgetExceeded,

    /// A configuration problem (e.g. a role resolved to an empty model).
    #[error("config: {0}")]
    Config(String),

    /// An I/O error (session persistence, transport).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A tool execution failure carrying a human-readable message fed back to the model.
    #[error("{0}")]
    Tool(String),
}

/// Engine result alias.
pub type Result<T> = std::result::Result<T, AgentError>;
