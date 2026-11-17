// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider.rs,provider/lifecycle.rs,agent.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Claude Code over `--input-format stream-json --output-format stream-json`.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::LaunchError;
use crate::event::{
    ApprovalKind, ApprovalRequest, ApprovalSource, Event, ToolCall, ToolState, Tools, approval_detail, one_line,
};
use crate::harness::HarnessKind;
use crate::line::{LineDriver, Protocol, Step, TextStream, write_json};
use crate::registry::Adapter;
use crate::session::{self, CommandPlan, EventSink, Session};
use crate::spec::{ApprovalVerdict, LaunchConfig, LaunchRequest, LaunchSpec, PermissionMode, utf8};

const INIT_REQUEST: &str = "basemind-init";
/// The most partial tool input collected for one content block.
const PARTIAL_INPUT_BYTES: usize = 1024 * 1024;
/// The most content blocks whose partial input is collected at once; the block index is
/// the harness's to choose, so the table is bounded here.
const PARTIAL_BLOCKS: usize = 16;
/// Claude Code tools that start or schedule work outside the session (remote triggers, cron
/// jobs, wake-ups, messages to other projects, notifications, workflows, other worktrees). The
/// adapter disallows them and `system/init` must not offer them: what they start is outside the
/// session's limits and its process group. Claude Code 2.1.326 dropped all of them from its
/// `tools` under `--disallowedTools`.
const OUT_OF_SESSION_TOOLS: &[&str] = &[
    "RemoteTrigger",
    "CronCreate",
    "CronDelete",
    "CronList",
    "ScheduleWakeup",
    "SendToProject",
    "SendToProjectContext",
    "PushNotification",
    "Workflow",
    "EnterWorktree",
    "ExitWorktree",
    "Monitor",
    "TaskStop",
];
/// Older CLIs never acknowledge the initialize request; the prompt goes out regardless.
const INIT_PATIENCE: Duration = Duration::from_secs(8);

/// Drives Claude Code.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeCodeAdapter;

impl Adapter for ClaudeCodeAdapter {
    fn supports(&self, kind: HarnessKind) -> bool {
        kind == HarnessKind::ClaudeCode
    }

    fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError> {
        self.plan_spec(&LaunchSpec::new(config, request)?)
    }

    fn launch(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<Session, LaunchError> {
        self.launch_spec(&LaunchSpec::new(config, request)?)
    }
}

impl ClaudeCodeAdapter {
    pub(crate) fn plan_spec(&self, spec: &LaunchSpec) -> Result<CommandPlan, LaunchError> {
        let prepared = spec.prepare()?;
        let mode = permission_mode(spec.permission_mode);
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
        args.extend(["--disallowedTools".to_owned(), OUT_OF_SESSION_TOOLS.join(",")]);
        let mut servers = serde_json::Map::new();
        if let (Some(server), Some(command)) = (&spec.mcp_server, &prepared.mcp_command) {
            // The server inherits the harness's environment, which carries the identity.
            servers.insert(
                server.name.clone(),
                json!({"type": "stdio", "command": utf8(command)?, "args": server.args}),
            );
        }
        // Always given, so `--strict-mcp-config` leaves exactly these servers.
        let mut server_args = vec!["--mcp-config".to_owned(), json!({"mcpServers": servers}).to_string()];
        if let Some(server) = spec.mcp_server.as_ref().filter(|server| !server.allow_tools.is_empty()) {
            let tools: Vec<String> = server
                .allow_tools
                .iter()
                .map(|tool| format!("mcp__{}__{tool}", server.name))
                .collect();
            server_args.extend(["--allowedTools".to_owned(), tools.join(",")]);
        }
        if prepared.bypass(spec) {
            args.push("--allow-dangerously-skip-permissions".to_owned());
        }
        args.extend(prepared.model_args.iter().cloned());
        spec.plan(&prepared, args, server_args, &[])
    }

    fn launch_spec(&self, spec: &LaunchSpec) -> Result<Session, LaunchError> {
        let plan = self.plan_spec(spec)?;
        let servers = spec.mcp_server.iter().map(|server| server.name.clone()).collect();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let protocol = ClaudeProtocol::new(spec.brief.clone(), permission_mode(spec.permission_mode), servers)
            .defining(defined_names(&plan.cwd, home.as_deref()));
        let (driver, control) = LineDriver::boxed(protocol);
        session::start(&plan, spec, driver, control)
    }
}

/// Claude Code's name for `mode`, as `--permission-mode` takes it and `system/init` reports it.
fn permission_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Supervised => "default",
        PermissionMode::ReadOnly => "plan",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Bypass => "bypassPermissions",
    }
}

/// Checks Claude Code's `system/init` frame: it has to report the permission mode the adapter
/// asked for, exactly the MCP servers it injected and each of them as loaded from
/// `--mcp-config` (`source: "dynamic"`), only built-in plugins, and none of the
/// [`OUT_OF_SESSION_TOOLS`]. Claude Code 2.1.325 was
/// seen to report exactly that under the adapter's flags, and project, plugin and claude.ai
/// servers and user plugins without them; a version that stops honouring the flags, or whose
/// frame no longer carries the mode and the servers, fails the session here.
fn check_init(frame: &Value, mode: &str, servers: &[String], defined: &[String]) -> Result<(), LaunchError> {
    let reported_mode = frame["permissionMode"].as_str();
    if reported_mode != Some(mode) {
        return Err(LaunchError::Protocol(format!(
            "Claude Code reported permission mode {}, not the requested {mode:?}",
            frame["permissionMode"]
        )));
    }
    let reported: Option<Vec<&str>> = frame["mcp_servers"]
        .as_array()
        .and_then(|list| list.iter().map(|server| server["name"].as_str()).collect());
    let mut reported = reported
        .ok_or_else(|| LaunchError::Protocol(format!("Claude Code reported MCP servers {}", frame["mcp_servers"])))?;
    reported.sort_unstable();
    let mut expected: Vec<&str> = servers.iter().map(String::as_str).collect();
    expected.sort_unstable();
    if reported != expected {
        return Err(LaunchError::Protocol(format!(
            "Claude Code reported MCP servers {reported:?}, not {expected:?}"
        )));
    }
    let mut list = frame["mcp_servers"].as_array().into_iter().flatten();
    if let Some(server) = list.find(|server| !server["source"].is_null() && server["source"] != "dynamic") {
        return Err(LaunchError::Protocol(format!(
            "Claude Code loaded MCP server {} from {}, not from the adapter's --mcp-config",
            server["name"], server["source"]
        )));
    }
    let tools = frame["tools"]
        .as_array()
        .ok_or_else(|| LaunchError::Protocol("Claude Code reported no tools".to_owned()))?;
    if let Some(tool) = tools
        .iter()
        .find(|tool| tool.as_str().is_some_and(|tool| OUT_OF_SESSION_TOOLS.contains(&tool)))
    {
        return Err(LaunchError::Protocol(format!(
            "Claude Code offers {tool}, which starts work outside the session"
        )));
    }
    for kind in ["agent", "skill"] {
        let listed = frame[format!("{kind}s")].as_array().into_iter().flatten();
        check_names(kind, listed.filter_map(Value::as_str), defined)?;
    }
    let mut plugins = frame["plugins"].as_array().into_iter().flatten();
    if let Some(plugin) = plugins.find(|plugin| plugin["path"] != "builtin") {
        return Err(LaunchError::Protocol(format!(
            "Claude Code loaded plugin {} from {}",
            plugin["name"], plugin["path"]
        )));
    }
    Ok(())
}

/// Checks Claude Code's answer to `initialize`, before the brief goes out: when it reports
/// them, the permission mode has to be the requested one, every command built in and no agent
/// one the project or the user defines. Claude Code 2.1.326 reported `current_permission_mode`
/// and only `builtin` commands under the adapter's flags, and user and project skills as
/// commands without the flag when the setting sources were not emptied.
fn check_initialize(answer: &Value, mode: &str, defined: &[String]) -> Result<(), LaunchError> {
    if let Some(reported) = answer["current_permission_mode"]
        .as_str()
        .filter(|reported| *reported != mode)
    {
        return Err(LaunchError::Protocol(format!(
            "Claude Code started in permission mode {reported:?}, not the requested {mode:?}"
        )));
    }
    let commands = answer["commands"].as_array().into_iter().flatten();
    if let Some(command) = commands.into_iter().find(|command| command["builtin"] != true) {
        return Err(LaunchError::Protocol(format!(
            "Claude Code loaded command {} that is not built in",
            command["name"]
        )));
    }
    let agents = answer["agents"].as_array().into_iter().flatten();
    let names = agents.filter_map(|agent| agent["name"].as_str());
    check_names("agent", names, defined)
}

/// Refuses a skill or agent `kind` named in `names` that a plugin (`plugin:name`), the project
/// or the user defines.
fn check_names<'a>(
    kind: &str,
    mut names: impl Iterator<Item = &'a str>,
    defined: &[String],
) -> Result<(), LaunchError> {
    match names.find(|name| name.contains(':') || defined.iter().any(|known| known == name)) {
        Some(name) => Err(LaunchError::Protocol(format!(
            "Claude Code loaded {kind} {name:?}, which the adapter's flags should have left out"
        ))),
        None => Ok(()),
    }
}

/// The agents, skills and commands defined in `.claude/` of `cwd` and each ancestor, and of
/// `home`: what Claude Code would list if it read the project's or the user's settings.
pub(crate) fn defined_names(cwd: &std::path::Path, home: Option<&std::path::Path>) -> Vec<String> {
    let mut names = Vec::new();
    for dir in cwd.ancestors().chain(home) {
        for kind in ["agents", "skills", "commands"] {
            let Ok(entries) = std::fs::read_dir(dir.join(".claude").join(kind)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = if kind == "skills" {
                    path.file_name()
                } else {
                    path.file_stem()
                };
                if let Some(name) = name
                    .and_then(|name| name.to_str())
                    .filter(|name| !names.iter().any(|n: &String| n == name))
                {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names
}

/// On resume, Claude Code replays a background-task notification from the earlier session and
/// answers it with an empty result before the real turn.
fn is_turn_result(frame: &Value) -> bool {
    frame["type"] == "result" && !(frame["origin"]["kind"] == "task-notification" && frame["num_turns"] == 0)
}

/// The request for a `can_use_tool` control request. An allow answers with the request's own
/// `input`, so that input, and the path Claude Code flagged, are the grant shown.
fn approval_request(id: String, request: &Value) -> ApprovalRequest {
    let tool = request["tool_name"].as_str().unwrap_or("tool");
    let kind = match tool {
        "Bash" => ApprovalKind::Command,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ApprovalKind::FileChange,
        // Leaving plan mode lifts its restrictions for the rest of the session.
        "ExitPlanMode" => ApprovalKind::Permissions,
        "WebFetch" | "WebSearch" => ApprovalKind::Fetch,
        _ if tool.starts_with("mcp__") => ApprovalKind::McpTool,
        _ => ApprovalKind::Tool,
    };
    let mut grant = serde_json::to_string_pretty(&request["input"]).unwrap_or_default();
    if let Some(path) = request["blocked_path"].as_str() {
        grant.push_str(&format!("\nOutside the allowed directories: {path}"));
    }
    ApprovalRequest {
        id,
        kind,
        title: format!("Allow {}?", one_line(tool)),
        detail: approval_detail(&grant, None),
        params: request.clone(),
    }
}

pub(crate) struct ClaudeProtocol {
    /// The brief, until it has been sent.
    brief: Option<String>,
    /// The permission mode `system/init` has to report.
    mode: &'static str,
    /// The MCP servers `system/init` has to report, and no others.
    servers: Vec<String>,
    /// The agents and skills the project or the user defines, which neither the `initialize`
    /// answer nor `system/init` may list.
    defined: Vec<String>,
    /// Whether a `system/init` frame was checked.
    verified: bool,
    turn_active: bool,
    session_id: Option<String>,
    text: TextStream,
    message: usize,
    /// Whether the turn in progress has produced assistant text.
    begun: bool,
    tools: Tools,
    /// Streaming tool id and its partial input JSON, by content block index; `None` once
    /// the input passed [`PARTIAL_INPUT_BYTES`].
    partial_tools: HashMap<u64, (String, Option<String>)>,
    /// Pending approvals: the wire id and the request, by event id.
    approvals: HashMap<String, (Value, Value)>,
    /// The number of approval requests seen, which names the next one. The wire id is the
    /// harness's to reuse; the event id is not, so a late answer cannot reach a newer request.
    approval_sequence: u64,
}

impl ClaudeProtocol {
    pub fn new(brief: String, mode: &'static str, servers: Vec<String>) -> Self {
        Self {
            brief: Some(brief),
            mode,
            servers,
            defined: Vec::new(),
            verified: false,
            turn_active: false,
            session_id: None,
            text: TextStream::default(),
            message: 0,
            begun: false,
            tools: Tools::default(),
            partial_tools: HashMap::new(),
            approvals: HashMap::new(),
            approval_sequence: 0,
        }
    }

    /// The same protocol, refusing the agents and skills in `defined`.
    pub fn defining(mut self, defined: Vec<String>) -> Self {
        self.defined = defined;
        self
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
                if self.partial_tools.len() < PARTIAL_BLOCKS || self.partial_tools.contains_key(&index) {
                    self.partial_tools.insert(index, (id.to_owned(), Some(String::new())));
                }
                sink.emit(Event::ToolCall(self.tools.insert(tool).clone()));
            }
            ("content_block_delta", "input_json_delta") => {
                // Collected and parsed once, when the block ends: parsing on every delta is
                // quadratic. Beyond the cap it is dropped; the assistant frame repeats it.
                if let Some((_, slot)) = self.partial_tools.get_mut(&index) {
                    let delta = event["delta"]["partial_json"].as_str().unwrap_or("");
                    match slot {
                        Some(partial) if partial.len() + delta.len() <= PARTIAL_INPUT_BYTES => partial.push_str(delta),
                        _ => *slot = None,
                    }
                }
            }
            ("content_block_stop", _) => {
                let Some((id, Some(partial))) = self.partial_tools.remove(&index) else {
                    return;
                };
                let parsed = serde_json::from_str::<Value>(&partial).is_ok_and(|value| value.is_object());
                if let Some(tool) = self.tools.get_mut(&id).filter(|_| parsed) {
                    tool.input = partial;
                    tool.bound_input();
                    sink.emit(Event::ToolCall(tool.clone()));
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
                .get_or_insert(id, || ToolCall::new(id, block["name"].as_str().unwrap_or("Tool")));
            tool.input = serde_json::to_string_pretty(&block["input"]).unwrap_or_default();
            tool.bound_input();
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
        // A request of a finished turn will not be acted on any more.
        for id in std::mem::take(&mut self.approvals).into_keys() {
            sink.resolved(id, false, ApprovalSource::Withdrawn);
        }
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
        let reused = self.approvals.iter().find(|(_, (pending, _))| pending == wire);
        if let Some(id) = reused.map(|(id, _)| id.clone()) {
            // A cancel or an answer by this wire id could not tell the two apart: both are
            // denied, the pending one reported withdrawn and the second unseen.
            sink.emit(Event::Status(format!(
                "Denied two approval requests sharing the id {wire}"
            )));
            if let Some((pending, earlier)) = self.approvals.remove(&id) {
                self.respond(&pending, &earlier, false, out)?;
                sink.resolved(id, false, ApprovalSource::Withdrawn);
            }
            return self.respond(wire, request, false, out);
        }
        self.approval_sequence += 1;
        let id = format!("claude-{}", self.approval_sequence);
        let verdict = sink.request_approval(approval_request(id.clone(), request), self.approvals.len());
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
            let wire = &frame["request_id"];
            let cancelled = self.approvals.iter().find(|(_, (pending, _))| pending == wire);
            if let Some(id) = cancelled.map(|(id, _)| id.clone()) {
                self.approvals.remove(&id);
                sink.resolved(id, false, ApprovalSource::Withdrawn);
            }
            return Ok(Step::Continue);
        }
        if kind == "system" && frame["subtype"] == "init" {
            check_init(&frame, self.mode, &self.servers, &self.defined)?;
            self.verified = true;
        }
        if !self.verified
            && matches!(
                kind,
                "assistant" | "stream_event" | "user" | "result" | "control_request"
            )
        {
            return Err(LaunchError::Protocol(format!(
                "Claude Code sent a {kind} frame before reporting its permission mode"
            )));
        }
        if kind == "control_response" && frame["response"]["request_id"] == INIT_REQUEST {
            check_initialize(&frame["response"]["response"], self.mode, &self.defined)?;
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

    fn interrupt(&mut self, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        for (id, (wire, request)) in std::mem::take(&mut self.approvals) {
            self.respond(&wire, &request, false, out)?;
            sink.resolved(id, false, ApprovalSource::Withdrawn);
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
