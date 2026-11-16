// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider.rs,provider/lifecycle.rs,agent.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Claude Code over `--input-format stream-json --output-format stream-json`.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::LaunchError;
use crate::event::{ApprovalRequest, ApprovalSource, Event, ToolCall, ToolState};
use crate::harness::HarnessKind;
use crate::line::{LineDriver, Protocol, Step, TextStream, write_json};
use crate::registry::Adapter;
use crate::session::{self, CommandPlan, EventSink, Session};
use crate::spec::{ApprovalVerdict, LaunchSpec, PermissionMode};

const INIT_REQUEST: &str = "basemind-init";
/// Older CLIs never acknowledge the initialize request; the prompt goes out regardless.
const INIT_PATIENCE: Duration = Duration::from_secs(8);

/// Drives Claude Code.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCodeAdapter;

impl Adapter for ClaudeCodeAdapter {
    fn supports(&self, kind: HarnessKind) -> bool {
        kind == HarnessKind::ClaudeCode
    }

    fn plan(&self, spec: &LaunchSpec) -> Result<CommandPlan, LaunchError> {
        let prepared = spec.prepare()?;
        let mode = match spec.permission_mode {
            PermissionMode::Supervised => "default",
            PermissionMode::ReadOnly => "plan",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Bypass => "bypassPermissions",
        };
        let mut args: Vec<String> = [
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            mode,
            // No user, project or local settings: their permission rules, hooks and MCP
            // servers would act behind the approval policy. The flags are monocode's
            // (`provider.rs`); the empty source list and the always-on hook switch are ours.
            "--setting-sources=",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--strict-mcp-config",
        ]
        .map(str::to_owned)
        .into();
        let mut servers = serde_json::Map::new();
        if let Some(server) = &spec.mcp_server {
            // The server inherits the harness's environment, which carries the identity.
            servers.insert(
                server.name.clone(),
                json!({"type": "stdio", "command": server.command, "args": server.args}),
            );
        }
        // Always given, so `--strict-mcp-config` leaves exactly these servers.
        args.extend(["--mcp-config".to_owned(), json!({"mcpServers": servers}).to_string()]);
        if let Some(server) = spec.mcp_server.as_ref().filter(|server| server.allow_tools) {
            args.extend(["--allowedTools".to_owned(), format!("mcp__{}", server.name)]);
        }
        if prepared.bypass(spec) {
            args.push("--allow-dangerously-skip-permissions".to_owned());
        }
        args.extend(prepared.extra_args.iter().cloned());
        spec.plan(&prepared, args, &[])
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Session, LaunchError> {
        let plan = self.plan(spec)?;
        let (driver, control) = LineDriver::boxed(ClaudeProtocol::new(spec.brief.clone()));
        session::start(&plan, spec, driver, control)
    }
}

/// On resume, Claude Code replays a background-task notification from the earlier session and
/// answers it with an empty result before the real turn.
fn is_turn_result(frame: &Value) -> bool {
    frame["type"] == "result" && !(frame["origin"]["kind"] == "task-notification" && frame["num_turns"] == 0)
}

fn request_id(wire: &Value) -> String {
    wire.as_str().map_or_else(|| wire.to_string(), str::to_owned)
}

pub(crate) struct ClaudeProtocol {
    /// The brief, until it has been sent.
    brief: Option<String>,
    turn_active: bool,
    session_id: Option<String>,
    text: TextStream,
    message: usize,
    /// Whether the turn in progress has produced assistant text.
    begun: bool,
    tools: HashMap<String, ToolCall>,
    /// Streaming tool id and its partial input JSON, by content block index.
    partial_tools: HashMap<u64, (String, String)>,
    /// Pending approvals: the wire id and the request, by event id.
    approvals: HashMap<String, (Value, Value)>,
}

impl ClaudeProtocol {
    pub fn new(brief: String) -> Self {
        Self {
            brief: Some(brief),
            turn_active: false,
            session_id: None,
            text: TextStream::default(),
            message: 0,
            begun: false,
            tools: HashMap::new(),
            partial_tools: HashMap::new(),
            approvals: HashMap::new(),
        }
    }

    fn send_brief(&mut self, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        match self.brief.take() {
            Some(brief) => self.prompt(&brief, out, sink),
            None => Ok(()),
        }
    }

    fn push_text(&mut self, text: &str, snapshot: bool, sink: &EventSink) {
        let key = self.message.to_string();
        self.begun |= self.text.push(&key, text, snapshot, sink);
    }

    fn respond(&mut self, wire: &Value, request: &Value, allow: bool, out: &mut dyn Write) -> Result<(), LaunchError> {
        let response = if allow {
            json!({"behavior": "allow", "updatedInput": request["input"]})
        } else {
            json!({"behavior": "deny", "message": "Denied by the launcher's approval policy"})
        };
        write_json(
            out,
            &json!({"type": "control_response", "response": {
                "subtype": "success", "request_id": wire, "response": response,
            }}),
        )
    }

    fn stream_event(&mut self, event: &Value, sink: &EventSink) {
        let kind = event["type"].as_str().unwrap_or("");
        let index = event["index"].as_u64().unwrap_or(0);
        match (kind, event["delta"]["type"].as_str().unwrap_or("")) {
            ("message_start", _) => {
                self.message += 1;
                self.partial_tools.clear();
            }
            ("content_block_start", _) if event["content_block"]["type"] == "tool_use" => {
                let block = &event["content_block"];
                let id = block["id"].as_str().unwrap_or("");
                let tool = ToolCall::new(id, block["name"].as_str().unwrap_or("Tool"));
                self.partial_tools.insert(index, (id.to_owned(), String::new()));
                self.tools.insert(id.to_owned(), tool.clone());
                sink.emit(Event::ToolCall(tool));
            }
            ("content_block_delta", "input_json_delta") => {
                // Keep the last input that parses; a half-streamed object is not reported.
                if let Some((id, partial)) = self.partial_tools.get_mut(&index) {
                    partial.push_str(event["delta"]["partial_json"].as_str().unwrap_or(""));
                    let parsed = serde_json::from_str::<Value>(partial).is_ok_and(|value| value.is_object());
                    if let Some(tool) = self.tools.get_mut(id.as_str()).filter(|_| parsed) {
                        tool.input.clone_from(partial);
                        sink.emit(Event::ToolCall(tool.clone()));
                    }
                }
            }
            ("content_block_delta", "thinking_delta") => {
                sink.emit(Event::Reasoning(
                    event["delta"]["thinking"].as_str().unwrap_or("").to_owned(),
                ));
            }
            ("content_block_delta", "text_delta") => {
                self.push_text(event["delta"]["text"].as_str().unwrap_or(""), false, sink);
            }
            _ => {}
        }
    }

    fn assistant(&mut self, frame: &Value, sink: &EventSink) -> Result<(), LaunchError> {
        if let Some(error) = frame["error"].as_str() {
            return Err(LaunchError::Protocol(format!("Claude Code reported: {error}")));
        }
        let blocks = || frame["message"]["content"].as_array().into_iter().flatten();
        for block in blocks().filter(|block| block["type"] == "tool_use") {
            let id = block["id"].as_str().unwrap_or("");
            let tool = self
                .tools
                .entry(id.to_owned())
                .or_insert_with(|| ToolCall::new(id, block["name"].as_str().unwrap_or("Tool")));
            tool.input = serde_json::to_string_pretty(&block["input"]).unwrap_or_default();
            sink.emit(Event::ToolCall(tool.clone()));
        }
        let text: String = blocks()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect();
        self.push_text(&text, true, sink);
        Ok(())
    }

    fn tool_results(&mut self, frame: &Value, sink: &EventSink) {
        let blocks = frame["message"]["content"].as_array().into_iter().flatten();
        for block in blocks.filter(|block| block["type"] == "tool_result") {
            let Some(tool) = block["tool_use_id"].as_str().and_then(|id| self.tools.get_mut(id)) else {
                continue;
            };
            tool.output = block["content"].as_str().map_or_else(
                || {
                    let parts = block["content"].as_array().into_iter().flatten();
                    parts
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                },
                str::to_owned,
            );
            tool.bound_output();
            tool.state = if block["is_error"] == true {
                ToolState::Failed
            } else {
                ToolState::Completed
            };
            sink.emit(Event::ToolCall(tool.clone()));
        }
    }

    fn result(&mut self, frame: &Value, sink: &EventSink) -> Result<Step, LaunchError> {
        if frame["is_error"] == true {
            let error = frame["result"]
                .as_str()
                .map_or_else(|| frame["errors"].to_string(), str::to_owned);
            return Err(LaunchError::Protocol(format!("Claude Code reported: {error}")));
        }
        // Some versions omit partial and assistant frames in simple turns.
        if !self.begun {
            self.push_text(frame["result"].as_str().unwrap_or(""), true, sink);
        }
        self.turn_active = false;
        self.begun = false;
        Ok(Step::TurnDone)
    }

    fn control_request(&mut self, frame: &Value, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        let request = &frame["request"];
        let wire = &frame["request_id"];
        if request["subtype"] != "can_use_tool" {
            return write_json(
                out,
                &json!({"type": "control_response", "response": {
                    "subtype": "error", "request_id": wire, "error": "Unsupported host request",
                }}),
            );
        }
        let id = request_id(wire);
        let verdict = sink.request_approval(ApprovalRequest {
            id: id.clone(),
            title: format!("Allow {}?", request["tool_name"].as_str().unwrap_or("tool")),
            detail: serde_json::to_string_pretty(&request["input"]).unwrap_or_default(),
        });
        match verdict {
            ApprovalVerdict::Ask => {
                self.approvals.insert(id, (wire.clone(), request.clone()));
                Ok(())
            }
            verdict => self.respond(wire, request, verdict == ApprovalVerdict::Allow, out),
        }
    }
}

impl Protocol for ClaudeProtocol {
    fn start(&mut self, out: &mut dyn Write) -> Result<(), LaunchError> {
        write_json(
            out,
            &json!({"type": "control_request", "request_id": INIT_REQUEST, "request": {"subtype": "initialize"}}),
        )
    }

    fn frame(&mut self, frame: Value, out: &mut dyn Write, sink: &EventSink) -> Result<Step, LaunchError> {
        let kind = frame["type"].as_str().unwrap_or("");
        if kind == "control_cancel_request" {
            let id = request_id(&frame["request_id"]);
            if self.approvals.remove(&id).is_some() {
                sink.resolved(id, false, ApprovalSource::Withdrawn);
            }
            return Ok(Step::Continue);
        }
        let initialized = kind == "system" && matches!(frame["subtype"].as_str(), Some("init" | "initialized"));
        if kind == "control_response" || initialized {
            self.send_brief(out, sink)?;
        }
        let starts_message = frame["parent_tool_use_id"].is_null()
            && (kind == "assistant" || (kind == "stream_event" && frame["event"]["type"] == "message_start"));
        if starts_message && !self.turn_active {
            // The harness began a turn nobody prompted, such as a background-task wake-up.
            sink.begin_turn()?;
            self.turn_active = true;
        }
        if let Some(id) = frame["session_id"]
            .as_str()
            .filter(|id| self.session_id.as_deref() != Some(id))
        {
            self.session_id = Some(id.to_owned());
            sink.emit(Event::HarnessSession(id.to_owned()));
        }
        match kind {
            "stream_event" => self.stream_event(&frame["event"], sink),
            "assistant" => self.assistant(&frame, sink)?,
            "user" => self.tool_results(&frame, sink),
            "result" if is_turn_result(&frame) => return self.result(&frame, sink),
            "control_request" => self.control_request(&frame, out, sink)?,
            _ => {}
        }
        Ok(Step::Continue)
    }

    fn answer(&mut self, id: &str, allow: bool, out: &mut dyn Write) -> Result<bool, LaunchError> {
        let Some((wire, request)) = self.approvals.remove(id) else {
            return Ok(false);
        };
        self.respond(&wire, &request, allow, out)?;
        Ok(true)
    }

    fn prompt(&mut self, text: &str, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        sink.begin_turn()?;
        self.turn_active = true;
        write_json(
            out,
            &json!({"type": "user", "session_id": "", "parent_tool_use_id": null,
                "message": {"role": "user", "content": text}}),
        )
    }

    fn interrupt(&mut self, out: &mut dyn Write) -> Result<(), LaunchError> {
        for (wire, request) in std::mem::take(&mut self.approvals).into_values() {
            self.respond(&wire, &request, false, out)?;
        }
        write_json(
            out,
            &json!({"type": "control_request", "request_id": "basemind-interrupt", "request": {"subtype": "interrupt"}}),
        )
    }

    fn tick(&mut self, elapsed: Duration, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        if elapsed >= INIT_PATIENCE {
            self.send_brief(out, sink)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
