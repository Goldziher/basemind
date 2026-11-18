//! The one error type of the engine's operations.
//!
//! A variant says what the caller can do about the failure, not where it happened. Each carries the
//! message an agent reads unchanged: the MCP adapter reports it as an `isError` result and the CLI
//! prints it, so a message names the next call to make. The adapter maps the variants onto MCP error
//! codes, which the CLI turns into exit codes (`src/mcp/adapter.rs`).

/// Why an engine operation did not answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum EngineError {
    /// The arguments cannot be answered as given: a path outside the repository or not in the
    /// index, a stale or foreign cursor, an unknown revision. The caller fixes the call.
    #[error("{0}")]
    InvalidArgument(String),
    /// The operation is not available in this workspace or session: no git repository, a
    /// read-only store asked to write.
    #[error("{0}")]
    Unavailable(String),
    /// The index cannot be read right now (empty, or held by another process); retry later.
    #[error("{0}")]
    IndexUnavailable(String),
    /// Another process holds what the operation reads (the index, the writer lock): retry, or route
    /// the call through the daemon. The CLI exits `3` for it.
    #[error("{0}")]
    Busy(String),
    /// The caller cancelled the operation before it finished.
    #[error("request_cancelled")]
    Cancelled,
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Index(#[from] crate::index::IndexError),
    #[error(transparent)]
    Git(#[from] crate::git::GitError),
    #[error(transparent)]
    GitCache(#[from] crate::git_cache::CacheError),
    #[error(transparent)]
    Scan(#[from] crate::scanner::ScanError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A failure inside the engine the caller cannot fix (serialization, an invariant).
    #[error("{0}")]
    Internal(String),
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "the operations moved so far construct only some variants")
)]
impl EngineError {
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub(crate) fn busy(message: impl Into<String>) -> Self {
        Self::Busy(message.into())
    }

    pub(crate) fn index_unavailable(message: impl Into<String>) -> Self {
        Self::IndexUnavailable(message.into())
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// The refusal of a tool whose retrieval stack is not in this build: it is advertised in every
    /// build, and answers this rather than an empty result an agent would read as "nothing found".
    #[cfg_attr(
        all(feature = "code-search", feature = "documents", feature = "memory"),
        allow(dead_code, reason = "only the gated-off arms name a missing feature")
    )]
    pub(crate) fn feature_missing(tool: &str, feature: &str) -> Self {
        Self::Unavailable(format!(
            "`{tool}` needs the `{feature}` Cargo feature, which is not compiled into this basemind binary. \
             Rebuild with `--features {feature}`; the published release binary includes it."
        ))
    }
}
