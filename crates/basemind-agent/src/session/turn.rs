//! The turn-loop — the engine's state machine, written as a plain loop (no separate FSM to drift
//! out of sync with the runtime).
//!
//! One turn drives the model until it stops asking for tools: build a request from history + the
//! tool specs, stream the response (emitting `TextDelta`s and assembling tool calls by index),
//! record the assistant message, then — if the model asked for tools — permission-gate and execute
//! each call, feed the results back, and loop. Tool failures (bad JSON, a tool error, a denial) are
//! fed back to the model as tool-result messages rather than aborting the turn; only cancellation,
//! a provider/stream error, or the step budget end a turn early.

use std::path::PathBuf;
use std::sync::Arc;

use basemind::mcp::BasemindServer;
use futures::StreamExt;
use liter_llm::{
    AssistantContent, AssistantMessage, ChatCompletionRequest, FinishReason, Message, StreamOptions, ToolCall,
    ToolChoice, ToolChoiceMode, ToolMessage,
};
use tokio::sync::{broadcast, mpsc};

use super::stream_assembler::{AssembledTurn, StreamAssembler};
use crate::command::{AgentCommand, PermissionDecision};
use crate::event::{AgentEvent, StopReason};
use crate::history::History;
use crate::permission::{Decision, PermissionEngine};
use crate::provider::ResolvedRole;
use crate::tools::{ToolCtx, ToolOutput, ToolRegistry};

/// Longest tool-result summary surfaced in a [`AgentEvent::ToolResult`] event.
const SUMMARY_CAP: usize = 200;

/// Everything a turn needs beyond the event/command channels.
pub struct TurnContext<'a> {
    /// The running conversation (mutated in place as the turn progresses).
    pub history: &'a mut History,
    /// The tools available this turn.
    pub tools: &'a ToolRegistry,
    /// The resolved model + per-request knobs for this turn's role.
    pub role: &'a ResolvedRole,
    /// The permission policy.
    pub permission: &'a PermissionEngine,
    /// Repository root for tool execution.
    pub root: PathBuf,
    /// The in-process basemind server for code-nav tools, if the workspace is indexed.
    pub server: Option<Arc<BasemindServer>>,
    /// The connected multi-agent room for `room:*` tools, if one is wired.
    pub room: Option<Arc<dyn crate::room::RoomClient>>,
    /// Maximum model steps (tool-call rounds) before the turn stops.
    pub max_steps: u32,
}

/// Run one turn to completion, emitting events and consuming commands (permission replies / cancel).
pub async fn run_turn(
    turn: u64,
    cx: &mut TurnContext<'_>,
    events: &broadcast::Sender<AgentEvent>,
    commands: &mut mpsc::Receiver<AgentCommand>,
) -> StopReason {
    let _ = events.send(AgentEvent::TurnStarted { turn });
    let mut seq = 0u64;

    for step in 0..cx.max_steps {
        let assembled = match stream_step(turn, cx, events, commands, &mut seq).await {
            Ok(assembled) => assembled,
            Err(reason) => return finish(events, turn, reason, step + 1),
        };

        cx.history.push(assistant_message(&assembled));
        if let Some(usage) = &assembled.usage {
            let (input_tokens, output_tokens) = cx.history.add_usage(usage);
            let _ = events.send(AgentEvent::Usage {
                turn,
                input_tokens,
                output_tokens,
                cost_usd: None,
            });
        }

        match assembled.finish_reason {
            Some(FinishReason::ToolCalls) if !assembled.tool_calls.is_empty() => {}
            Some(FinishReason::Length) => return finish(events, turn, StopReason::Length, step + 1),
            Some(FinishReason::ContentFilter) => return finish(events, turn, StopReason::ContentFilter, step + 1),
            _ => return finish(events, turn, StopReason::Stop, step + 1),
        }

        for (index, call) in assembled.tool_calls.iter().enumerate() {
            if let Some(reason) = execute_call(turn, cx, events, commands, call).await {
                // On cancellation, feed a synthetic result for every sibling call not yet run so the ~keep
                // assistant's tool_calls all have matching tool results — required for the history to ~keep
                // stay valid for the next turn or a resumed session. ~keep
                for pending in &assembled.tool_calls[index + 1..] {
                    cx.history.push(tool_result_message(pending, "cancelled".into()));
                }
                return finish(events, turn, reason, step + 1);
            }
        }
    }

    finish(events, turn, StopReason::MaxSteps, cx.max_steps)
}

/// Open a stream, emit text deltas, and assemble the turn. Returns the assembled turn, a terminal
/// [`StopReason`] on a provider/stream error, or [`StopReason::Cancelled`] if the user cancels
/// mid-stream. The command channel is polled concurrently with the stream so a `Cancel` aborts the
/// model response promptly rather than waiting for it to finish; stray non-cancel commands (there is
/// no outstanding permission prompt, and mid-turn user messages are not queued) are ignored.
async fn stream_step(
    turn: u64,
    cx: &TurnContext<'_>,
    events: &broadcast::Sender<AgentEvent>,
    commands: &mut mpsc::Receiver<AgentCommand>,
    seq: &mut u64,
) -> Result<AssembledTurn, StopReason> {
    let request = build_request(cx);
    let mut stream = match cx.role.client.chat_stream(request).await {
        Ok(stream) => stream,
        Err(error) => {
            emit_error(events, turn, &error.to_string());
            return Err(StopReason::Error);
        }
    };

    let mut assembler = StreamAssembler::new();
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(Ok(chunk)) => {
                    if let Some(delta) = assembler.push_chunk(&chunk) {
                        *seq += 1;
                        let _ = events.send(AgentEvent::TextDelta {
                            turn,
                            seq: *seq,
                            text: delta,
                        });
                    }
                }
                Some(Err(error)) => {
                    emit_error(events, turn, &error.to_string());
                    return Err(StopReason::Error);
                }
                None => break,
            },
            command = commands.recv() => {
                if is_cancel(&command) {
                    return Err(StopReason::Cancelled);
                }
            }
        }
    }
    Ok(assembler.into_turn())
}

/// Permission-gate and execute one tool call, feeding the result back into history. Returns a
/// terminal [`StopReason`] only if the turn must end (cancellation); otherwise `None`.
async fn execute_call(
    turn: u64,
    cx: &mut TurnContext<'_>,
    events: &broadcast::Sender<AgentEvent>,
    commands: &mut mpsc::Receiver<AgentCommand>,
    call: &ToolCall,
) -> Option<StopReason> {
    // Announce the call up front so the transcript shows the pending tool during the permission ~keep
    // prompt, and every outcome — denied, cancelled, or run — has a started entry to fill with its ~keep
    // result (an unknown tool or a rejected claim renders a failed result the same way). ~keep
    let _ = events.send(AgentEvent::ToolStarted {
        turn,
        call_id: call.id.clone(),
        name: call.function.name.clone(),
        args: serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null),
    });

    let Some(tool) = cx.tools.get(&call.function.name) else {
        feed_tool_error(cx, events, call, format!("unknown tool `{}`", call.function.name));
        return None;
    };
    let tool = Arc::clone(tool);

    let claim = match tool.permission_of(&call.function.arguments) {
        Ok(claim) => claim,
        Err(error) => {
            feed_tool_error(cx, events, call, error.to_string());
            return None;
        }
    };

    match cx.permission.evaluate(&claim) {
        Decision::Allow => {}
        Decision::Deny => {
            feed_tool_error(cx, events, call, "denied by policy".into());
            return None;
        }
        Decision::Ask => {
            let req_id = cx.permission.next_request_id();
            let _ = events.send(AgentEvent::PermissionRequested {
                turn,
                req_id,
                call_id: call.id.clone(),
                tool: call.function.name.clone(),
                action: claim.kind.as_str().to_string(),
                target: claim.target.clone(),
            });
            match await_permission(req_id, commands).await {
                Some(PermissionDecision::Allow) => {}
                Some(PermissionDecision::AllowForSession) => cx.permission.remember(&claim),
                Some(PermissionDecision::Deny) => {
                    feed_tool_error(cx, events, call, "denied by user".into());
                    return None;
                }
                None => {
                    feed_tool_error(cx, events, call, "cancelled".into());
                    return Some(StopReason::Cancelled);
                }
            }
        }
    }

    let ctx = ToolCtx {
        root: cx.root.clone(),
        server: cx.server.clone(),
        room: cx.room.clone(),
    };
    // Race the tool against the command channel so a cancel aborts a long-running tool (shell, ~keep
    // scan) promptly. Dropping the future is the cooperative cancel; a non-cancel command that ~keep
    // arrives mid-execution is ignored and the tool continues. ~keep
    let call_future = tool.call(&call.function.arguments, &ctx);
    tokio::pin!(call_future);
    let output = loop {
        tokio::select! {
            result = &mut call_future => break match result {
                Ok(output) => output,
                // A tool that errors hard still feeds the message back to the model rather than aborting. ~keep
                Err(error) => ToolOutput::error(error.to_string()),
            },
            command = commands.recv() => {
                if is_cancel(&command) {
                    feed_tool_error(cx, events, call, "cancelled".into());
                    return Some(StopReason::Cancelled);
                }
            }
        }
    };

    let _ = events.send(AgentEvent::ToolResult {
        call_id: call.id.clone(),
        ok: !output.is_error,
        summary: truncate(&output.text),
    });
    cx.history.push(tool_result_message(call, output.text));
    None
}

/// Whether a command observed mid-turn should cancel it: an explicit `Cancel`, a `Shutdown`, or a
/// closed channel (the UI dropped its client). `Shutdown` ends the turn here; the session itself
/// ends when the runner sees the command channel close (the UI drops its client on shutdown).
/// Non-cancel commands (stray permission replies, mid-turn user messages) do not cancel.
fn is_cancel(command: &Option<AgentCommand>) -> bool {
    matches!(
        command,
        None | Some(AgentCommand::Cancel) | Some(AgentCommand::Shutdown)
    )
}

/// Wait for the permission reply matching `req_id`. `Cancel` (or a closed channel) returns `None`;
/// unrelated commands are ignored for the duration of the wait.
async fn await_permission(req_id: u64, commands: &mut mpsc::Receiver<AgentCommand>) -> Option<PermissionDecision> {
    while let Some(command) = commands.recv().await {
        match command {
            AgentCommand::PermissionDecision {
                req_id: replied,
                decision,
            } if replied == req_id => return Some(decision),
            AgentCommand::Cancel | AgentCommand::Shutdown => return None,
            _ => {}
        }
    }
    None
}

fn build_request(cx: &TurnContext<'_>) -> ChatCompletionRequest {
    let tools = cx.tools.specs();
    ChatCompletionRequest {
        model: cx.role.model.clone(),
        messages: cx.history.to_messages(),
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice: Some(ToolChoice::Mode(ToolChoiceMode::Auto)),
        temperature: cx.role.temperature,
        max_tokens: cx.role.max_tokens,
        stream_options: Some(StreamOptions {
            include_usage: Some(true),
        }),
        ..Default::default()
    }
}

fn assistant_message(assembled: &AssembledTurn) -> Message {
    Message::Assistant(AssistantMessage {
        content: (!assembled.text.is_empty()).then(|| AssistantContent::Text(assembled.text.clone())),
        name: None,
        tool_calls: (!assembled.tool_calls.is_empty()).then(|| assembled.tool_calls.clone()),
        refusal: None,
        function_call: None,
    })
}

fn tool_result_message(call: &ToolCall, content: String) -> Message {
    Message::Tool(ToolMessage {
        content,
        tool_call_id: call.id.clone(),
        name: Some(call.function.name.clone()),
    })
}

/// Push a tool-error result (fed back to the model) and emit the corresponding failed `ToolResult`.
fn feed_tool_error(cx: &mut TurnContext<'_>, events: &broadcast::Sender<AgentEvent>, call: &ToolCall, message: String) {
    let _ = events.send(AgentEvent::ToolResult {
        call_id: call.id.clone(),
        ok: false,
        summary: truncate(&message),
    });
    cx.history.push(tool_result_message(call, message));
}

fn emit_error(events: &broadcast::Sender<AgentEvent>, turn: u64, message: &str) {
    let _ = events.send(AgentEvent::Error {
        turn: Some(turn),
        message: message.to_string(),
        fatal: false,
    });
}

fn finish(events: &broadcast::Sender<AgentEvent>, turn: u64, reason: StopReason, steps: u32) -> StopReason {
    let _ = events.send(AgentEvent::TurnFinished { turn, reason, steps });
    reason
}

fn truncate(text: &str) -> String {
    if text.len() <= SUMMARY_CAP {
        return text.to_string();
    }
    let mut end = SUMMARY_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
