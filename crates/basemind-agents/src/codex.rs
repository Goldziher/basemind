// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider.rs,provider/lifecycle.rs,agent.rs,mono/bridge.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Codex over `codex app-server`: JSON-RPC without the `jsonrpc` member, one message per line.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::error::LaunchError;
use crate::event::{
    ApprovalKind, ApprovalRequest, ApprovalSource, Event, ToolCall, ToolState, Tools, approval_detail, one_line,
};
use crate::harness::HarnessKind;
use crate::line::{LineDriver, Protocol, Step, TextStream, write_json};
use crate::registry::Adapter;
use crate::session::{self, CommandPlan, EventSink, Session};
use crate::spec::{ApprovalVerdict, LaunchConfig, LaunchRequest, LaunchSpec, PermissionMode, Prepared, utf8};

const INITIALIZE_ID: u64 = 1;
const THREAD_ID: u64 = 2;
const TURN_ID_PREFIX: &str = "basemind-turn-";
const INTERRUPT_ID: &str = "basemind-interrupt";
/// The server request through which Codex asks before an MCP tool call.
const ELICITATION: &str = "mcpServer/elicitation/request";
/// How many file-change items keep their full changes for an approval request.
const FILE_CHANGE_ITEMS: usize = 16;
/// How often growing command output is reported.
const OUTPUT_REPORT_INTERVAL: Duration = Duration::from_millis(100);

/// Drives Codex.
#[derive(Clone, Copy, Debug, Default)]
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn supports(&self, kind: HarnessKind) -> bool {
        kind == HarnessKind::Codex
    }

    fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError> {
        self.plan_spec(&LaunchSpec::new(config, request)?)
    }

    fn launch(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<Session, LaunchError> {
        self.launch_spec(&LaunchSpec::new(config, request)?)
    }
}

impl CodexAdapter {
    pub(crate) fn plan_spec(&self, spec: &LaunchSpec) -> Result<CommandPlan, LaunchError> {
        self.plan_prepared(spec).map(|(plan, _)| plan)
    }

    /// The plan and the checked spec it was built from, checked once per launch.
    fn plan_prepared(&self, spec: &LaunchSpec) -> Result<(CommandPlan, Prepared), LaunchError> {
        let prepared = spec.prepare()?;
        if spec.permission_mode == PermissionMode::AcceptEdits {
            // Codex 0.169 under `on-request` and a workspace-write sandbox ran `touch` unasked:
            // its nearest mode also runs commands, which the mode does not promise.
            return Err(LaunchError::Unsupported {
                harness: spec.harness.kind.id(),
                feature: "an accept-edits mode: its nearest one also runs commands unasked",
            });
        }
        let mut server_args = Vec::new();
        if let (Some(server), Some(command)) = (&spec.mcp_server, &prepared.mcp_command) {
            // Codex approves a server's tools all at once; single tools cannot be named here.
            if !server.allow_tools.is_empty() {
                return Err(LaunchError::Unsupported {
                    harness: spec.harness.kind.id(),
                    feature: "pre-approving single MCP tools",
                });
            }
            // Codex hands a server only the variables named in `env_vars`.
            let settings = [
                ("command", toml_string(utf8(command)?)),
                ("args", toml_array(&server.args)),
                ("env_vars", toml_array(&spec.mcp_env_names())),
            ];
            for (key, value) in settings {
                server_args.extend(["-c".to_owned(), format!("mcp_servers.{}.{key}={value}", server.name)]);
            }
        }
        let mut args = vec!["app-server".to_owned()];
        args.extend(prepared.model_args.iter().cloned());
        let plan = spec.plan(&prepared, args, server_args, &[])?;
        Ok((plan, prepared))
    }

    fn launch_spec(&self, spec: &LaunchSpec) -> Result<Session, LaunchError> {
        let (plan, prepared) = self.plan_prepared(spec)?;
        let access = Access::new(spec.permission_mode, prepared.bypass(spec));
        let protocol = CodexProtocol::new(spec.brief.clone(), utf8(&plan.cwd)?.to_owned(), access);
        let (driver, control) = LineDriver::boxed(protocol);
        session::start(&plan, spec, driver, control)
    }
}

/// `text` as a TOML basic string. JSON's escaping is not enough: TOML also forbids a raw DEL
/// and every other control character, so all of them are written as `\uXXXX`.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `items` as a TOML array of basic strings.
fn toml_array(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|item| toml_string(item)).collect();
    format!("[{}]", items.join(","))
}

/// The approval policy and sandbox a permission mode maps to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Access {
    approval_policy: &'static str,
    sandbox: &'static str,
    sandbox_policy: Value,
}

impl Access {
    /// `bypass` is the checked consent of the spec; without it `Bypass` degrades to supervised.
    /// `ReadOnly` uses approval policy `never`: the read-only sandbox is the only control,
    /// nothing is asked, and whatever the sandbox blocks fails instead of becoming a request
    /// an allowing policy could turn into a write. Codex 0.169 failed an MCP tool call under
    /// it ("requires approval, but approval policy is never") rather than running it.
    /// `AcceptEdits` is refused by the adapter before it gets here, and maps to supervised.
    pub fn new(mode: PermissionMode, bypass: bool) -> Self {
        let (approval_policy, sandbox, sandbox_policy) = match mode {
            PermissionMode::ReadOnly => ("never", "read-only", "readOnly"),
            PermissionMode::Bypass if bypass => ("on-request", "danger-full-access", "dangerFullAccess"),
            PermissionMode::Supervised | PermissionMode::AcceptEdits | PermissionMode::Bypass => {
                ("untrusted", "read-only", "readOnly")
            }
        };
        Self {
            approval_policy,
            sandbox,
            sandbox_policy: json!({"type": sandbox_policy}),
        }
    }
}

fn error_message(error: &Value, fallback: &str) -> String {
    let message = error["message"].as_str().unwrap_or(fallback);
    if error["codexErrorInfo"] == "usageLimitExceeded" {
        format!("Usage limit reached: {message}")
    } else {
        message.to_owned()
    }
}

fn failure(error: &Value, fallback: &str) -> LaunchError {
    LaunchError::Protocol(format!("Codex reported: {}", error_message(error, fallback)))
}

pub(crate) struct CodexProtocol {
    /// The brief, until its turn has been started.
    brief: Option<String>,
    cwd: String,
    access: Access,
    initialized: bool,
    thread: Option<String>,
    active_turn: Option<String>,
    /// A `turn/start` of ours is out and its turn has not been announced yet.
    awaiting_turn: bool,
    turn_requests: u32,
    text: TextStream,
    tools: Tools,
    /// Calls whose output grew since they were last reported.
    unreported: HashSet<String>,
    /// When output increments were last reported.
    reported_at: Instant,
    /// Pending approvals: the wire id, the request method and its params, by event id.
    approvals: HashMap<String, (Value, String, Value)>,
    /// The number of approval requests seen, which names the next one.
    approval_sequence: u64,
    /// The full `changes` of the latest file-change items, by item id; `None` when they were
    /// too large to show in a request.
    file_changes: VecDeque<(String, Option<Value>)>,
}

impl CodexProtocol {
    pub fn new(brief: String, cwd: String, access: Access) -> Self {
        Self {
            brief: Some(brief),
            cwd,
            access,
            initialized: false,
            thread: None,
            active_turn: None,
            awaiting_turn: false,
            turn_requests: 0,
            text: TextStream::default(),
            tools: Tools::default(),
            unreported: HashSet::new(),
            reported_at: Instant::now(),
            approvals: HashMap::new(),
            approval_sequence: 0,
            file_changes: VecDeque::new(),
        }
    }

    /// Records the turn the harness announced. One that answers no `turn/start` of ours is a
    /// turn of its own making and counts against the limit.
    fn turn_started(&mut self, id: &str, sink: &EventSink) -> Result<(), LaunchError> {
        if self.active_turn.as_deref() == Some(id) {
            return Ok(());
        }
        if !std::mem::take(&mut self.awaiting_turn) {
            sink.begin_turn()?;
        }
        self.active_turn = Some(id.to_owned());
        Ok(())
    }

    fn respond(
        &self,
        wire: &Value,
        method: &str,
        request: &Value,
        allow: bool,
        out: &mut dyn Write,
    ) -> Result<(), LaunchError> {
        let result = if method == ELICITATION {
            // An empty `_meta` leaves out `persist`: the allow covers this one call.
            json!({"action": if allow { "accept" } else { "decline" }, "content": if allow { json!({}) } else { Value::Null }, "_meta": null})
        } else if method.contains("permissions") {
            json!({"permissions": if allow { request["permissions"].clone() } else { json!({}) }, "scope": "turn"})
        } else if method == "execCommandApproval" || method == "applyPatchApproval" {
            json!({"decision": if allow { "approved" } else { "denied" }})
        } else {
            json!({"decision": if allow { "accept" } else { "decline" }})
        };
        write_json(out, &json!({"id": wire, "result": result}))
    }

    fn approval(
        &mut self,
        wire: &Value,
        method: &str,
        params: &Value,
        out: &mut dyn Write,
        sink: &EventSink,
    ) -> Result<(), LaunchError> {
        let reused = self.approvals.iter().find(|(_, (pending, _, _))| pending == wire);
        if let Some(id) = reused.map(|(id, _)| id.clone()) {
            // An answer by this wire id could not tell the two apart: both are denied, the
            // pending one reported withdrawn and the second unseen.
            sink.emit(Event::Status(format!(
                "Denied two approval requests sharing the id {wire}"
            )));
            if let Some((pending, earlier_method, earlier)) = self.approvals.remove(&id) {
                self.respond(&pending, &earlier_method, &earlier, false, out)?;
                sink.resolved(id, false, ApprovalSource::Withdrawn);
            }
            return self.respond(wire, method, params, false, out);
        }
        self.approval_sequence += 1;
        let id = format!("codex-{}", self.approval_sequence);
        let (request, shown) = self.approval_request(id.clone(), method, params);
        if !shown {
            // An allow would approve changes nobody was shown: denied unasked.
            sink.emit(Event::ApprovalRequest(request));
            sink.resolved(id, false, ApprovalSource::GrantUnknown);
            return self.respond(wire, method, params, false, out);
        }
        let verdict = sink.request_approval(request, self.approvals.len());
        match verdict {
            ApprovalVerdict::Ask => {
                self.approvals
                    .insert(id, (wire.clone(), method.to_owned(), params.clone()));
                Ok(())
            }
            verdict => self.respond(wire, method, params, verdict == ApprovalVerdict::Allow, out),
        }
    }

    /// The full changes an allow of a file-change request would approve: the request's own
    /// `fileChanges` (the older protocol), else the `changes` of the item it names. `None`
    /// when neither is known, or they are too large to hold for the caller.
    fn requested_changes(&self, params: &Value) -> Option<Value> {
        let changes = match &params["fileChanges"] {
            Value::Null => {
                let item = params["itemId"].as_str()?;
                let (_, changes) = self.file_changes.iter().find(|(id, _)| id == item)?;
                changes.clone()?
            }
            changes => changes.clone(),
        };
        (crate::session::json_len(&changes) <= crate::session::MAX_PENDING_BYTES).then_some(changes)
    }

    /// Renders what an allow would approve. The command and the permissions are in the
    /// request; the file changes are in the item the harness announced before asking, and
    /// are added to `params` as `item.changes` in full. The flag is `false` for a file-change
    /// request whose changes are unknown or too large: it can only be denied.
    fn approval_request(&self, id: String, method: &str, params: &Value) -> (ApprovalRequest, bool) {
        let pretty = |value: &Value| serde_json::to_string_pretty(value).unwrap_or_default();
        let known = params["itemId"].as_str().and_then(|item| self.tools.get(item));
        let mut params = params.clone();
        let (kind, title, mut grant) = if method == ELICITATION {
            let meta = &params["_meta"];
            let grant = format!(
                "Server: {}\nTool: {}\nArguments: {}",
                params["serverName"].as_str().unwrap_or("?"),
                meta["tool_name"].as_str().unwrap_or("?"),
                pretty(&meta["tool_params"])
            );
            (ApprovalKind::McpTool, "Allow this MCP tool call?", grant)
        } else if method.contains("fileChange") || method == "applyPatchApproval" {
            let Some(changes) = self.requested_changes(&params) else {
                let detail = "The harness did not say in full which files would change, or the changes are \
                              too large to show, so the request is denied.";
                let request = ApprovalRequest {
                    id,
                    kind: ApprovalKind::FileChange,
                    title: "Allow file changes?".to_owned(),
                    detail: detail.to_owned(),
                    params,
                };
                return (request, false);
            };
            let grant = pretty(&changes);
            params["item"] = json!({"id": params["itemId"], "changes": changes});
            (ApprovalKind::FileChange, "Allow file changes?", grant)
        } else if method.contains("permissions") {
            let permissions = pretty(&params["permissions"]);
            (ApprovalKind::Permissions, "Allow additional permissions?", permissions)
        } else if method.contains("commandExecution") || method == "execCommandApproval" {
            let command = match (&params["command"], known) {
                (Value::String(command), _) => command.clone(),
                (command, _) if !command.is_null() => pretty(command),
                (_, Some(tool)) => tool.input.clone(),
                (_, None) => "The harness did not say which command.".to_owned(),
            };
            // An accept also grants what the command asks beyond the sandbox, so that is the
            // grant shown, and the request is one for permissions.
            let escalated = !params["additionalPermissions"].is_null() || !params["networkApprovalContext"].is_null();
            match params["kind"].as_str() {
                Some("writeStdin") => (ApprovalKind::Command, "Allow input to a running command?", command),
                _ if escalated => (
                    ApprovalKind::Permissions,
                    "Allow this command with more access?",
                    command,
                ),
                _ => (ApprovalKind::Command, "Allow this command?", command),
            }
        } else {
            (ApprovalKind::Tool, "Allow this request?", pretty(&params))
        };
        if let Some(cwd) = params["cwd"].as_str() {
            grant.push_str(&format!("\nIn: {cwd}"));
        }
        if let Some(root) = params["grantRoot"].as_str() {
            grant.push_str(&format!("\nAlso requests write access under: {root}"));
        }
        if !params["additionalPermissions"].is_null() {
            grant.push_str(&format!(
                "\nAlso requests: {}",
                pretty(&params["additionalPermissions"])
            ));
        }
        let network = &params["networkApprovalContext"];
        if !network.is_null() {
            grant.push_str(&format!(
                "\nAlso requests network access to {} ({})",
                one_line(network["host"].as_str().unwrap_or("?")),
                one_line(network["protocol"].as_str().unwrap_or("?"))
            ));
        }
        let request = ApprovalRequest {
            id,
            kind,
            title: title.to_owned(),
            detail: approval_detail(&grant, params["reason"].as_str()),
            params,
        };
        (request, true)
    }

    /// Whether a turn notification belongs to our thread; everything before the thread is
    /// known is taken as ours.
    fn ours(&self, params: &Value) -> bool {
        match (params["threadId"].as_str(), &self.thread) {
            (Some(thread), Some(ours)) => thread == ours,
            _ => true,
        }
    }

    /// Reports the calls whose output grew, at most every [`OUTPUT_REPORT_INTERVAL`] unless
    /// `now`: a report copies up to 64 KiB, so one per one-byte increment would be quadratic.
    fn report_output(&mut self, now: bool, sink: &EventSink) {
        if self.unreported.is_empty() || !(now || self.reported_at.elapsed() >= OUTPUT_REPORT_INTERVAL) {
            return;
        }
        self.reported_at = Instant::now();
        for id in std::mem::take(&mut self.unreported) {
            if let Some(tool) = self.tools.get_mut(&id) {
                tool.bound_output();
                sink.emit(Event::ToolCall(tool.clone()));
            }
        }
    }

    /// Drops the pending requests whose wire id `which` selects, reporting them withdrawn.
    fn withdraw(&mut self, which: impl Fn(&Value) -> bool, sink: &EventSink) {
        let ids: Vec<String> = self
            .approvals
            .iter()
            .filter(|(_, (wire, _, _))| which(wire))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.approvals.remove(&id);
            sink.resolved(id, false, ApprovalSource::Withdrawn);
        }
    }

    /// Keeps a file-change item's full `changes` for a later approval request, forgetting the
    /// oldest item beyond [`FILE_CHANGE_ITEMS`] and the changes of one too large to show.
    fn record_changes(&mut self, id: &str, changes: &Value) {
        self.file_changes.retain(|(known, _)| known != id);
        if self.file_changes.len() >= FILE_CHANGE_ITEMS {
            self.file_changes.pop_front();
        }
        let shown = (!changes.is_null() && crate::session::json_len(changes) <= crate::session::MAX_PENDING_BYTES)
            .then(|| changes.clone());
        self.file_changes.push_back((id.to_owned(), shown));
    }

    fn tool(&mut self, item: &Value, completed: bool, sink: &EventSink) {
        let kind = item["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch" | "dynamicToolCall"
        ) {
            return;
        }
        let id = item["id"].as_str().unwrap_or("");
        self.unreported.remove(id);
        if kind == "fileChange" {
            self.record_changes(id, &item["changes"]);
        }
        let tool = self.tools.get_or_insert(id, || ToolCall::new(id, kind));
        tool.name = item["tool"].as_str().unwrap_or(kind).to_owned();
        if kind == "mcpToolCall" {
            tool.name = format!("{}:{}", item["server"].as_str().unwrap_or("mcp"), tool.name);
        }
        tool.input = item["command"]
            .as_str()
            .or_else(|| item["query"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let arguments = item.get("arguments").or_else(|| item.get("changes"));
                serde_json::to_string_pretty(arguments.unwrap_or(&Value::Null)).unwrap_or_default()
            });
        if let Some(changes) = item["changes"].as_array().filter(|_| kind == "fileChange") {
            let paths: Vec<_> = changes.iter().filter_map(|change| change["path"].as_str()).collect();
            tool.input = json!({"paths": paths}).to_string();
            tool.output = changes
                .iter()
                .map(|change| {
                    format!(
                        "{}\n{}",
                        change["path"].as_str().unwrap_or(""),
                        change["diff"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        if let Some(output) = item["aggregatedOutput"].as_str().or_else(|| item["output"].as_str()) {
            output.clone_into(&mut tool.output);
        } else if let Some(result) = item.get("result") {
            tool.output = serde_json::to_string_pretty(result).unwrap_or_default();
        }
        tool.bound_output();
        tool.bound_input();
        let fallback = if completed { "completed" } else { "inProgress" };
        tool.state = match item["status"].as_str().unwrap_or(fallback) {
            "completed" => ToolState::Completed,
            "failed" | "declined" => ToolState::Failed,
            "cancelled" | "canceled" | "interrupted" | "aborted" => ToolState::Cancelled,
            _ => ToolState::Running,
        };
        if item["exitCode"].as_i64().is_some_and(|code| code != 0) {
            tool.state = ToolState::Failed;
        }
        sink.emit(Event::ToolCall(tool.clone()));
    }

    fn response(&mut self, frame: &Value, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        if let Some(error) = frame.get("error").filter(|error| !error.is_null()) {
            return Err(failure(error, "RPC request failed"));
        }
        let result = &frame["result"];
        match frame["id"].as_u64() {
            Some(INITIALIZE_ID) if !self.initialized => {
                self.initialized = true;
                write_json(out, &json!({"method": "initialized"}))?;
                write_json(
                    out,
                    &json!({"id": THREAD_ID, "method": "thread/start", "params": {
                        "cwd": self.cwd,
                        "approvalPolicy": self.access.approval_policy,
                        "sandbox": self.access.sandbox,
                        "sandboxPolicy": self.access.sandbox_policy,
                        "approvalsReviewer": "user",
                    }}),
                )?;
            }
            Some(THREAD_ID) if self.thread.is_none() => {
                let id = result["thread"]["id"]
                    .as_str()
                    .ok_or_else(|| LaunchError::Protocol("Codex returned no thread id".to_owned()))?;
                self.thread = Some(id.to_owned());
                sink.emit(Event::HarnessSession(id.to_owned()));
                if let Some(brief) = self.brief.take() {
                    self.prompt(&brief, out, sink)?;
                }
            }
            _ => {
                let ours = frame["id"].as_str().is_some_and(|id| id.starts_with(TURN_ID_PREFIX));
                if let Some(turn) = result["turn"]["id"].as_str().filter(|_| ours) {
                    self.turn_started(turn, sink)?;
                }
            }
        }
        Ok(())
    }

    /// Answers every pending request with a denial: the turn they belong to is over, and a
    /// sub-agent thread's request would otherwise wait until the wall-time limit.
    fn deny_pending(&mut self, out: &mut dyn Write) -> Result<Vec<String>, LaunchError> {
        let pending: Vec<_> = self.approvals.drain().collect();
        let mut ids = Vec::with_capacity(pending.len());
        for (id, (wire, method, request)) in pending {
            self.respond(&wire, &method, &request, false, out)?;
            ids.push(id);
        }
        Ok(ids)
    }

    fn turn_ended(
        &mut self,
        method: &str,
        params: &Value,
        out: &mut dyn Write,
        sink: &EventSink,
    ) -> Result<Step, LaunchError> {
        self.report_output(true, sink);
        let turn = &params["turn"];
        let status = turn["status"].as_str().unwrap_or("completed");
        if status == "failed" {
            return Err(failure(&turn["error"], "Codex turn failed"));
        }
        let interrupted = matches!(status, "interrupted" | "cancelled" | "canceled") || method == "turn/aborted";
        for item in turn["items"].as_array().into_iter().flatten() {
            self.tool(item, true, sink);
        }
        if interrupted {
            for tool in self.tools.values_mut().filter(|tool| tool.state == ToolState::Running) {
                tool.state = ToolState::Cancelled;
                sink.emit(Event::ToolCall(tool.clone()));
            }
        }
        self.active_turn = None;
        for id in self.deny_pending(out)? {
            sink.resolved(id, false, ApprovalSource::Withdrawn);
        }
        Ok(Step::TurnDone)
    }

    fn notification(
        &mut self,
        method: &str,
        frame: &Value,
        out: &mut dyn Write,
        sink: &EventSink,
    ) -> Result<Step, LaunchError> {
        let params = &frame["params"];
        let request = frame.get("id");
        let turn_event = matches!(method, "turn/started" | "turn/completed" | "turn/aborted");
        if turn_event && self.thread.is_some() && params["threadId"].as_str().is_none() {
            // Codex 0.169's schema requires it. Without it a sub-agent's turn could end ours,
            // or a turn of ours escape the count, so the session fails instead.
            return Err(LaunchError::Protocol(format!("Codex sent {method} without a threadId")));
        }
        match method {
            // Codex runs further threads (sub-agents) on the same connection: only our
            // thread's turns count and end ours.
            "turn/started" | "turn/completed" | "turn/aborted" if !self.ours(params) => {}
            "turn/started" => {
                if let Some(id) = params["turn"]["id"].as_str() {
                    self.turn_started(id, sink)?;
                }
            }
            "item/agentMessage/delta" => {
                let key = params["itemId"].as_str().unwrap_or("assistant");
                self.text.push(key, params["delta"].as_str().unwrap_or(""), false, sink);
            }
            "item/completed" if params["item"]["type"] == "agentMessage" => {
                let key = params["item"]["id"].as_str().unwrap_or("assistant");
                self.text
                    .push(key, params["item"]["text"].as_str().unwrap_or(""), true, sink);
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                sink.emit(Event::Reasoning(params["delta"].as_str().unwrap_or("").to_owned()));
            }
            "item/started" | "item/completed" => self.tool(&params["item"], method == "item/completed", sink),
            "item/commandExecution/outputDelta" => {
                let id = params["itemId"].as_str().unwrap_or("");
                let tool = self.tools.get_or_insert(id, || ToolCall::new(id, "commandExecution"));
                tool.append_output(params["delta"].as_str().unwrap_or(""));
                if !self.unreported.contains(id) {
                    self.unreported.insert(id.to_owned());
                }
                self.report_output(false, sink);
            }
            "turn/completed" | "turn/aborted" => return self.turn_ended(method, params, out, sink),
            "serverRequest/resolved" => {
                let wire = &params["requestId"];
                self.withdraw(|pending| pending == wire, sink);
            }
            "error" if params["willRetry"] != true => return Err(failure(&params["error"], "Codex stream error")),
            "error" => sink.emit(Event::Status(error_message(&params["error"], "Codex is reconnecting"))),
            _ => {
                if let Some(wire) = request {
                    self.server_request(wire, method, params, out, sink)?;
                }
            }
        }
        Ok(Step::Continue)
    }

    fn server_request(
        &mut self,
        wire: &Value,
        method: &str,
        params: &Value,
        out: &mut dyn Write,
        sink: &EventSink,
    ) -> Result<(), LaunchError> {
        if method == "currentTime/read" {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            return write_json(out, &json!({"id": wire, "result": {"currentTimeAt": now}}));
        }
        if method.ends_with("/requestApproval") || matches!(method, "execCommandApproval" | "applyPatchApproval") {
            return self.approval(wire, method, params, out, sink);
        }
        if method == ELICITATION && params["_meta"]["codex_approval_kind"] == "mcp_tool_call" {
            // Codex 0.169 asks before every MCP tool call this way, not with a requestApproval.
            return self.approval(wire, method, params, out, sink);
        }
        if method == ELICITATION {
            sink.emit(Event::Status(
                "Declined an MCP elicitation: no form can be shown here".to_owned(),
            ));
            return write_json(
                out,
                &json!({"id": wire, "result": {"action": "cancel", "content": null, "_meta": null}}),
            );
        }
        // Every server request is answered promptly, or the harness waits on it forever.
        sink.emit(Event::Status(format!("Unsupported Codex request: {method}")));
        write_json(
            out,
            &json!({"id": wire, "error": {"code": -32601, "message": format!("Unsupported method: {method}")}}),
        )
    }
}

impl Protocol for CodexProtocol {
    fn start(&mut self, out: &mut dyn Write) -> Result<(), LaunchError> {
        write_json(
            out,
            &json!({"id": INITIALIZE_ID, "method": "initialize", "params": {
                "clientInfo": {"name": "basemind_agents", "title": "basemind", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true},
            }}),
        )
    }

    fn frame(&mut self, frame: Value, out: &mut dyn Write, sink: &EventSink) -> Result<Step, LaunchError> {
        match frame["method"].as_str() {
            Some(method) => self.notification(method, &frame, out, sink),
            None => {
                if frame["id"] != INTERRUPT_ID {
                    self.response(&frame, out, sink)?;
                }
                Ok(Step::Continue)
            }
        }
    }

    fn answer(&mut self, id: &str, allow: bool, out: &mut dyn Write) -> Result<bool, LaunchError> {
        let Some((wire, method, request)) = self.approvals.remove(id) else {
            return Ok(false);
        };
        self.respond(&wire, &method, &request, allow, out)?;
        Ok(true)
    }

    fn prompt(&mut self, text: &str, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        let thread = self
            .thread
            .clone()
            .ok_or_else(|| LaunchError::Protocol("Codex has no thread to start a turn in".to_owned()))?;
        sink.begin_turn()?;
        self.awaiting_turn = true;
        self.turn_requests += 1;
        write_json(
            out,
            &json!({"id": format!("{TURN_ID_PREFIX}{}", self.turn_requests), "method": "turn/start", "params": {
                "threadId": thread,
                "input": [{"type": "text", "text": text}],
                "approvalPolicy": self.access.approval_policy,
                "sandboxPolicy": self.access.sandbox_policy,
                "approvalsReviewer": "user",
            }}),
        )
    }

    fn tick(&mut self, _elapsed: Duration, _out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        self.report_output(false, sink);
        Ok(())
    }

    fn interrupt(&mut self, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError> {
        for id in self.deny_pending(out)? {
            sink.resolved(id, false, ApprovalSource::Withdrawn);
        }
        match (&self.thread, &self.active_turn) {
            (Some(thread), Some(turn)) => write_json(
                out,
                &json!({"id": INTERRUPT_ID, "method": "turn/interrupt", "params": {"threadId": thread, "turnId": turn}}),
            ),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
