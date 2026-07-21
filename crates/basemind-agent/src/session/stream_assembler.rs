//! Reassembles a streamed chat completion into a finished assistant turn.
//!
//! liter-llm's `chat_stream` yields OpenAI-shaped chunks in which tool calls arrive fragmented:
//! `id` and `name` land in an early chunk for a given `index`, and `arguments` stream as JSON
//! string fragments across later chunks. [`StreamAssembler`] folds those deltas back into whole
//! [`ToolCall`]s (ordered by index), concatenates assistant text, and captures usage + finish
//! reason. The turn-loop drives it one chunk at a time.

use std::collections::BTreeMap;

use liter_llm::{ChatCompletionChunk, FinishReason, FunctionCall, ToolCall, ToolType, Usage};

/// A tool call being built up from streamed fragments.
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// The finished product of assembling a stream.
#[derive(Debug, Default)]
pub struct AssembledTurn {
    /// The concatenated assistant text (reasoning/thinking stripped by the provider).
    pub text: String,
    /// Fully-assembled tool calls, ordered by their stream index.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage, if the final chunk carried it (requires `stream_options.include_usage`).
    pub usage: Option<Usage>,
    /// Why the model stopped, if a terminal chunk reported it.
    pub finish_reason: Option<FinishReason>,
}

/// Folds streamed [`ChatCompletionChunk`]s into an [`AssembledTurn`].
#[derive(Default)]
pub struct StreamAssembler {
    text: String,
    tool_calls: BTreeMap<u32, PartialToolCall>,
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
}

impl StreamAssembler {
    /// A fresh assembler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one chunk into the running state. Returns the text delta contained in this chunk (if
    /// any), so the caller can emit a `TextDelta` event without re-inspecting the chunk.
    pub fn push_chunk(&mut self, chunk: &ChatCompletionChunk) -> Option<String> {
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }
        // We only drive single-completion requests (`n` unset ⇒ 1), so the sole choice is ~keep
        // index 0. Select it explicitly rather than blindly taking `[0]`, so a provider that ~keep
        // orders choices differently (or a future `n > 1` caller) never silently loses deltas. ~keep
        let choice = chunk
            .choices
            .iter()
            .find(|choice| choice.index == 0)
            .or_else(|| chunk.choices.first())?;
        if let Some(reason) = &choice.finish_reason {
            self.finish_reason = Some(reason.clone());
        }
        let delta = &choice.delta;
        for tool_call in delta.tool_calls.iter().flatten() {
            let entry = self.tool_calls.entry(tool_call.index).or_default();
            if let Some(id) = &tool_call.id {
                entry.id = id.clone();
            }
            if let Some(function) = &tool_call.function {
                if let Some(name) = &function.name {
                    entry.name = name.clone();
                }
                if let Some(arguments) = &function.arguments {
                    entry.arguments.push_str(arguments);
                }
            }
        }
        match &delta.content {
            Some(text) if !text.is_empty() => {
                self.text.push_str(text);
                Some(text.clone())
            }
            _ => None,
        }
    }

    /// The finish reason seen so far, if any.
    pub fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason.clone()
    }

    /// Whether any tool-call fragments have been seen.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Consume the assembler, producing the finished turn with tool calls ordered by index.
    pub fn into_turn(self) -> AssembledTurn {
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|partial| ToolCall {
                id: partial.id,
                call_type: ToolType::Function,
                function: FunctionCall {
                    name: partial.name,
                    arguments: partial.arguments,
                },
            })
            .collect();
        AssembledTurn {
            text: self.text,
            tool_calls,
            usage: self.usage,
            finish_reason: self.finish_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MockModelClient as M;

    fn feed(chunks: &[ChatCompletionChunk]) -> AssembledTurn {
        let mut asm = StreamAssembler::new();
        for chunk in chunks {
            asm.push_chunk(chunk);
        }
        asm.into_turn()
    }

    #[test]
    fn concatenates_text_and_reports_stop() {
        let turn = feed(&[M::text("Hello, "), M::text("world"), M::finish(FinishReason::Stop)]);
        assert_eq!(turn.text, "Hello, world");
        assert!(turn.tool_calls.is_empty());
        assert_eq!(turn.finish_reason, Some(FinishReason::Stop));
    }

    #[test]
    fn push_chunk_returns_the_text_delta() {
        let mut asm = StreamAssembler::new();
        assert_eq!(asm.push_chunk(&M::text("abc")).as_deref(), Some("abc"));
        assert_eq!(asm.push_chunk(&M::finish(FinishReason::Stop)), None);
    }

    #[test]
    fn assembles_a_single_tool_call_from_fragments() {
        // id+name arrive first; arguments stream as three JSON fragments. ~keep
        let turn = feed(&[
            M::tool_call(0, Some("call_1"), Some("code:outline"), ""),
            M::tool_call(0, None, None, "{\"path\":"),
            M::tool_call(0, None, None, "\"src/lib.rs\""),
            M::tool_call(0, None, None, "}"),
            M::finish(FinishReason::ToolCalls),
        ]);
        assert_eq!(turn.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(turn.tool_calls.len(), 1);
        let call = &turn.tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "code:outline");
        assert_eq!(call.function.arguments, "{\"path\":\"src/lib.rs\"}");
    }

    #[test]
    fn assembles_parallel_tool_calls_ordered_by_index() {
        // Two calls interleaved across indices 0 and 1. ~keep
        let turn = feed(&[
            M::tool_call(0, Some("a"), Some("fs:read"), "{\"p\":"),
            M::tool_call(1, Some("b"), Some("shell:exec"), "{\"cmd\":"),
            M::tool_call(1, None, None, "\"ls\"}"),
            M::tool_call(0, None, None, "\"x\"}"),
            M::finish(FinishReason::ToolCalls),
        ]);
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].id, "a");
        assert_eq!(turn.tool_calls[0].function.name, "fs:read");
        assert_eq!(turn.tool_calls[0].function.arguments, "{\"p\":\"x\"}");
        assert_eq!(turn.tool_calls[1].id, "b");
        assert_eq!(turn.tool_calls[1].function.arguments, "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn captures_interleaved_text_and_tool_calls() {
        let turn = feed(&[
            M::text("thinking... "),
            M::tool_call(0, Some("c1"), Some("code:search_symbols"), "{}"),
            M::finish(FinishReason::ToolCalls),
        ]);
        assert_eq!(turn.text, "thinking... ");
        assert_eq!(turn.tool_calls.len(), 1);
    }

    #[test]
    fn captures_usage_from_final_chunk() {
        let mut usage_chunk = M::finish(FinishReason::Stop);
        usage_chunk.usage = Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_tokens_details: None,
        });
        let turn = feed(&[M::text("hi"), usage_chunk]);
        let usage = turn.usage.expect("usage captured");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }
}
