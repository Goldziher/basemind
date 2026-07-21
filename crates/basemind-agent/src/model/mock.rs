//! A scripted [`ModelClient`] for tests.
//!
//! Each call to [`ModelClient::chat_stream`] returns the next pre-baked sequence of chunks, in
//! order. No network, fully deterministic. Construct chunks with the [`MockModelClient::text`],
//! [`MockModelClient::tool_call`] and [`MockModelClient::finish`] helpers.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures::{StreamExt, stream};
use liter_llm::{
    BoxFuture, BoxStream, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, FinishReason,
    Result as LlmResult, StreamChoice, StreamDelta, StreamFunctionCall, StreamToolCall,
};

use super::ModelClient;

/// A test double that replays scripted streaming turns in order. One inner `Vec` per
/// `chat_stream` call; when the script is exhausted, further calls stream nothing.
pub struct MockModelClient {
    turns: Mutex<VecDeque<Vec<LlmResult<ChatCompletionChunk>>>>,
}

impl MockModelClient {
    /// Build from a list of turns, each a list of chunks to stream for one `chat_stream` call.
    pub fn new(turns: Vec<Vec<ChatCompletionChunk>>) -> Self {
        let turns = turns
            .into_iter()
            .map(|chunks| chunks.into_iter().map(Ok).collect())
            .collect();
        Self {
            turns: Mutex::new(turns),
        }
    }

    /// A chunk carrying a single text delta.
    pub fn text(delta: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    content: Some(delta.to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A chunk carrying a (possibly partial) tool-call delta at `index`. `id`/`name` typically
    /// arrive in the first fragment for a call; `args_fragment` is concatenated across fragments.
    pub fn tool_call(index: u32, id: Option<&str>, name: Option<&str>, args_fragment: &str) -> ChatCompletionChunk {
        ChatCompletionChunk {
            choices: vec![StreamChoice {
                delta: StreamDelta {
                    tool_calls: Some(vec![StreamToolCall {
                        index,
                        id: id.map(str::to_owned),
                        call_type: None,
                        function: Some(StreamFunctionCall {
                            name: name.map(str::to_owned),
                            arguments: Some(args_fragment.to_owned()),
                        }),
                    }]),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A terminal chunk carrying only a finish reason.
    pub fn finish(reason: FinishReason) -> ChatCompletionChunk {
        ChatCompletionChunk {
            choices: vec![StreamChoice {
                finish_reason: Some(reason),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A terminal chunk with `finish_reason = stop` (no more tools wanted).
    pub fn finish_stop() -> ChatCompletionChunk {
        Self::finish(FinishReason::Stop)
    }

    /// A terminal chunk with `finish_reason = tool_calls` (the model wants tools run).
    pub fn finish_tool_calls() -> ChatCompletionChunk {
        Self::finish(FinishReason::ToolCalls)
    }
}

/// A [`ModelClient`] whose stream emits one text delta and then never completes — a stand-in for a
/// slow provider. Used to test mid-stream cancellation deterministically: the turn-loop can only
/// advance past the stall by observing a cancel on the command channel.
pub struct StallingModelClient;

impl ModelClient for StallingModelClient {
    fn chat_stream(
        &self,
        _request: ChatCompletionRequest,
    ) -> BoxFuture<'_, LlmResult<BoxStream<'static, LlmResult<ChatCompletionChunk>>>> {
        Box::pin(async move {
            let head = stream::iter(vec![Ok(MockModelClient::text("thinking… "))]);
            let tail = stream::pending::<LlmResult<ChatCompletionChunk>>();
            let stream: BoxStream<'static, LlmResult<ChatCompletionChunk>> = Box::pin(head.chain(tail));
            Ok(stream)
        })
    }

    fn chat(&self, _request: ChatCompletionRequest) -> BoxFuture<'_, LlmResult<ChatCompletionResponse>> {
        Box::pin(std::future::pending())
    }
}

impl ModelClient for MockModelClient {
    fn chat_stream(
        &self,
        _request: ChatCompletionRequest,
    ) -> BoxFuture<'_, LlmResult<BoxStream<'static, LlmResult<ChatCompletionChunk>>>> {
        let turn = self
            .turns
            .lock()
            .expect("mock lock poisoned")
            .pop_front()
            .unwrap_or_default();
        Box::pin(async move {
            let stream: BoxStream<'static, LlmResult<ChatCompletionChunk>> = Box::pin(stream::iter(turn));
            Ok(stream)
        })
    }

    fn chat(&self, _request: ChatCompletionRequest) -> BoxFuture<'_, LlmResult<ChatCompletionResponse>> {
        Box::pin(async move {
            Ok(ChatCompletionResponse {
                id: "mock".into(),
                object: "chat.completion".into(),
                created: 0,
                model: "mock".into(),
                choices: Vec::new(),
                usage: None,
                system_fingerprint: None,
                service_tier: None,
            })
        })
    }
}
