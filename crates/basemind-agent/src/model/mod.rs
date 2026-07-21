//! Provider abstraction.
//!
//! The engine depends on a deliberately small, object-safe streaming-chat trait,
//! [`ModelClient`], rather than on liter-llm's eleven-method `LlmClient` directly. This keeps
//! test doubles tiny (two methods, see `MockModelClient`) and decouples the turn-loop from
//! provider specifics. Real providers are reached through [`LiterModelClient`], which adapts any
//! liter-llm client (`DefaultClient`, `ManagedClient`, ...) held as `Arc<dyn LlmClient>`.

// The scripted mocks are test-only: available under `cfg(test)` for this crate's own tests and
// under the `test-util` feature for downstream test builds, but never compiled into a release lib.
#[cfg(any(test, feature = "test-util"))]
mod mock;

#[cfg(any(test, feature = "test-util"))]
pub use mock::{MockModelClient, StallingModelClient};

use std::sync::Arc;

use liter_llm::{
    BoxFuture, BoxStream, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, LlmClient,
    Result as LlmResult,
};

/// The slice of an LLM client the engine needs: streaming chat plus one-shot chat. Object-safe
/// (methods return boxed futures/streams), so it is held as `Arc<dyn ModelClient>`.
pub trait ModelClient: Send + Sync {
    /// Open a streaming chat completion. The returned stream yields provider chunks until a
    /// terminal `finish_reason`; errors surface as `Err` items inside the stream.
    fn chat_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> BoxFuture<'_, LlmResult<BoxStream<'static, LlmResult<ChatCompletionChunk>>>>;

    /// A single non-streaming chat completion (used for cheap side tasks like titles/summaries).
    fn chat(&self, request: ChatCompletionRequest) -> BoxFuture<'_, LlmResult<ChatCompletionResponse>>;
}

/// Adapter making any liter-llm [`LlmClient`] usable as a [`ModelClient`].
#[derive(Clone)]
pub struct LiterModelClient {
    inner: Arc<dyn LlmClient>,
}

impl LiterModelClient {
    /// Wrap a liter-llm client (e.g. `DefaultClient` or `ManagedClient`) behind the engine's trait.
    pub fn new(inner: Arc<dyn LlmClient>) -> Self {
        Self { inner }
    }
}

impl ModelClient for LiterModelClient {
    fn chat_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> BoxFuture<'_, LlmResult<BoxStream<'static, LlmResult<ChatCompletionChunk>>>> {
        self.inner.chat_stream(request)
    }

    fn chat(&self, request: ChatCompletionRequest) -> BoxFuture<'_, LlmResult<ChatCompletionResponse>> {
        self.inner.chat(request)
    }
}
