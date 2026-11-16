//! Agents that speak the Agent Client Protocol, through the `agent-client-protocol` crate.
//!
//! The crate owns framing, request ids and the typed messages. This module owns the process
//! (so it lives in the session's process group), maps session updates to [`Event`]s and puts
//! every permission request through the session's approval policy.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, EnvVariable, Implementation, InitializeRequest, McpServer, McpServerStdio, Meta,
    NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate, SetSessionModeRequest,
    TextContent, ToolCallStatus, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, Client, Lines};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt};

use crate::error::LaunchError;
use crate::event::{ApprovalRequest, ApprovalSource, Control, Event, ToolCall, ToolState};
use crate::harness::{AcpFlavor, HarnessKind};
use crate::registry::Adapter;
use crate::session::{self, ChildIo, CommandPlan, ControlTx, Driver, EventSink, Session};
use crate::spec::{ApprovalVerdict, LaunchSpec, PermissionMode, Prepared};

/// Drives every [`HarnessKind::Acp`] harness.
///
/// An approval is answered with the request's `allow_once` option, falling back to
/// `allow_always` only when the agent offers nothing narrower; a denial with `reject_once`,
/// then `reject_always`, then the protocol's `cancelled` outcome.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcpAdapter;

fn flavor(spec: &LaunchSpec) -> Result<AcpFlavor, LaunchError> {
    match spec.harness.kind {
        HarnessKind::Acp(flavor) => Ok(flavor),
        other => Err(LaunchError::UnknownHarness(other.id())),
    }
}

/// The session mode to set after `session/new`, if the permission mode needs one.
fn session_mode(spec: &LaunchSpec, prepared: &Prepared, flavor: AcpFlavor) -> Result<Option<String>, LaunchError> {
    let unsupported = |feature| LaunchError::Unsupported {
        harness: spec.harness.kind.id(),
        feature,
    };
    match (spec.permission_mode, flavor) {
        (PermissionMode::Supervised, _) => Ok(None),
        // Grok takes these as launch arguments instead of a session mode.
        (PermissionMode::ReadOnly | PermissionMode::Bypass, AcpFlavor::Grok) => Ok(None),
        (PermissionMode::ReadOnly, AcpFlavor::OpenCode) => Ok(Some("plan".to_owned())),
        (PermissionMode::ReadOnly, _) => Err(unsupported("a read-only mode")),
        (PermissionMode::AcceptEdits, AcpFlavor::Hermes) => Ok(Some("accept_edits".to_owned())),
        (PermissionMode::AcceptEdits, _) => Err(unsupported("an accept-edits mode")),
        (PermissionMode::Bypass, _) => Ok(prepared.bypass_mode.clone()),
    }
}

impl Adapter for AcpAdapter {
    fn supports(&self, kind: HarnessKind) -> bool {
        matches!(kind, HarnessKind::Acp(_))
    }

    fn plan(&self, spec: &LaunchSpec) -> Result<CommandPlan, LaunchError> {
        let flavor = flavor(spec)?;
        let prepared = spec.prepare()?;
        session_mode(spec, &prepared, flavor)?;
        let mut env = Vec::new();
        let args: Vec<&str> = match flavor {
            AcpFlavor::Cursor | AcpFlavor::Hermes | AcpFlavor::OpenCode => vec!["acp"],
            AcpFlavor::Gemini => vec!["--experimental-acp"],
            AcpFlavor::Generic => Vec::new(),
            AcpFlavor::Grok => {
                env.push(("GROK_NO_AUTO_UPDATE", "1"));
                let mut args = vec!["--no-auto-update"];
                if spec.permission_mode == PermissionMode::ReadOnly {
                    args.extend(["--permission-mode", "plan"]);
                }
                args.extend(["agent", "--no-leader"]);
                if prepared.bypass(spec) {
                    args.push("--always-approve");
                }
                args.push("stdio");
                args
            }
        };
        spec.plan(&prepared, args.into_iter().map(str::to_owned).collect(), &env)
    }

    fn launch(&self, spec: &LaunchSpec) -> Result<Session, LaunchError> {
        let plan = self.plan(spec)?;
        let config = Config::new(spec, &plan)?;
        let (tx, controls) = unbounded();
        let driver = Box::new(AcpDriver { config, controls });
        let control: ControlTx = Box::new(move |control| tx.unbounded_send(control).is_ok());
        session::start(&plan, spec, driver, control)
    }
}

/// What the conversation needs from the spec, already in the protocol's types.
pub(crate) struct Config {
    cwd: PathBuf,
    brief: String,
    mcp_servers: Vec<McpServer>,
    /// `_meta` of `session/new`: Grok reads its bypass switch from there.
    session_meta: Option<Meta>,
    mode: Option<String>,
}

impl Config {
    pub fn new(spec: &LaunchSpec, plan: &CommandPlan) -> Result<Self, LaunchError> {
        let flavor = flavor(spec)?;
        let prepared = spec.prepare()?;
        // ACP names a server's variables explicitly, so the identity is passed by value.
        let env: Vec<EnvVariable> = spec
            .mcp_env_names()
            .into_iter()
            .filter_map(|name| {
                let value = plan.env.get(&name)?.to_str()?.to_owned();
                Some(EnvVariable::new(name, value))
            })
            .collect();
        let mcp_servers = spec
            .mcp_server
            .iter()
            .map(|server| {
                McpServer::Stdio(
                    McpServerStdio::new(&server.name, &server.command)
                        .args(server.args.clone())
                        .env(env.clone()),
                )
            })
            .collect();
        let session_meta = (flavor == AcpFlavor::Grok && prepared.bypass(spec)).then(|| {
            let mut meta = Meta::new();
            meta.insert("yoloMode".to_owned(), true.into());
            meta
        });
        Ok(Self {
            cwd: plan.cwd.clone(),
            brief: spec.brief.clone(),
            mcp_servers,
            session_meta,
            mode: session_mode(spec, &prepared, flavor)?,
        })
    }
}

struct AcpDriver {
    config: Config,
    controls: UnboundedReceiver<Control>,
}

impl Driver for AcpDriver {
    fn run(self: Box<Self>, io: ChildIo, sink: EventSink) -> Result<(), LaunchError> {
        let (line_tx, incoming) = unbounded();
        let stdout = io.stdout;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line_tx.unbounded_send(line).is_err() {
                    break;
                }
            }
        });
        let (out_tx, mut out_rx) = unbounded::<String>();
        let mut stdin = io.stdin;
        std::thread::spawn(move || {
            while let Some(line) = futures::executor::block_on(out_rx.next()) {
                let written = stdin
                    .write_all(line.as_bytes())
                    .and_then(|()| stdin.write_all(b"\n"))
                    .and_then(|()| stdin.flush());
                if written.is_err() {
                    break;
                }
            }
        });
        let outgoing = out_tx.sink_map_err(std::io::Error::other);
        futures::executor::block_on(converse(outgoing, incoming, self.config, sink, self.controls))
    }
}

type Answer = Box<dyn FnOnce(bool) -> Result<(), agent_client_protocol::Error> + Send>;

/// Permission requests the policy left for the caller, by event id.
#[derive(Clone, Default)]
struct Pending(Arc<Mutex<HashMap<String, Answer>>>);

impl Pending {
    fn answer(&self, id: &str, allow: bool, sink: &EventSink) {
        let answer = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        match answer.map(|answer| answer(allow)) {
            Some(Ok(())) => sink.resolved(id.to_owned(), allow, ApprovalSource::Caller),
            Some(Err(error)) => sink.emit(Event::Error(format!("could not answer approval {id:?}: {error}"))),
            None => sink.emit(Event::Error(format!("no approval request {id:?} is pending"))),
        }
    }
}

fn outcome(request: &RequestPermissionRequest, allow: bool) -> RequestPermissionOutcome {
    let preference = if allow {
        [PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]
    } else {
        [PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
    };
    preference
        .iter()
        .find_map(|kind| request.options.iter().find(|option| option.kind == *kind))
        .map_or(RequestPermissionOutcome::Cancelled, |option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
        })
}

fn approval_request(id: String, request: &RequestPermissionRequest) -> ApprovalRequest {
    let fields = &request.tool_call.fields;
    let title = match fields.kind {
        Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => "Allow file changes?".to_owned(),
        Some(ToolKind::Execute) => "Allow this command?".to_owned(),
        _ => format!("Allow {}?", fields.title.as_deref().unwrap_or("this tool")),
    };
    ApprovalRequest {
        id,
        title,
        detail: serde_json::to_string_pretty(&request.tool_call).unwrap_or_default(),
    }
}

/// The tool calls seen so far, so an update can be reported as the whole call.
#[derive(Clone, Default)]
struct Updates(Arc<Mutex<HashMap<String, ToolCall>>>);

impl Updates {
    fn apply(&self, update: SessionUpdate, sink: &EventSink) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    sink.emit(Event::Text(text.text));
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    sink.emit(Event::Reasoning(text.text));
                }
            }
            SessionUpdate::ToolCall(call) => {
                let mut fields = ToolCallUpdateFields::new()
                    .title(call.title)
                    .status(call.status)
                    .raw_input(call.raw_input)
                    .raw_output(call.raw_output);
                fields.name = call.name;
                self.merge(&call.tool_call_id.0, fields, sink);
            }
            SessionUpdate::ToolCallUpdate(update) => self.merge(&update.tool_call_id.0, update.fields, sink),
            _ => {}
        }
    }

    fn merge(&self, id: &str, fields: ToolCallUpdateFields, sink: &EventSink) {
        let mut tools = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tool = tools.entry(id.to_owned()).or_insert_with(|| ToolCall::new(id, "tool"));
        if let Some(name) = fields.name.or(fields.title) {
            tool.name = name;
        }
        if let Some(input) = fields.raw_input {
            tool.input = input.to_string();
        }
        if let Some(output) = fields.raw_output {
            tool.output = output.as_str().map_or_else(|| output.to_string(), str::to_owned);
            tool.bound_output();
        }
        match fields.status {
            Some(ToolCallStatus::Completed) => tool.state = ToolState::Completed,
            Some(ToolCallStatus::Failed) => tool.state = ToolState::Failed,
            _ => {}
        }
        sink.emit(Event::ToolCall(tool.clone()));
    }
}

fn protocol_error(error: &agent_client_protocol::Error) -> LaunchError {
    LaunchError::Protocol(format!("the ACP agent reported: {error}"))
}

/// Runs one ACP conversation over a pair of line channels until its turns are over.
pub(crate) async fn converse<O, I>(
    outgoing: O,
    incoming: I,
    config: Config,
    sink: EventSink,
    mut controls: UnboundedReceiver<Control>,
) -> Result<(), LaunchError>
where
    O: Sink<String, Error = std::io::Error> + Send + 'static,
    I: Stream<Item = std::io::Result<String>> + Send + 'static,
{
    let pending = Pending::default();
    let updates = Updates::default();
    let on_update = {
        let sink = sink.clone();
        async move |notification: SessionNotification, _connection: ConnectionTo<Agent>| {
            updates.apply(notification.update, &sink);
            Ok(())
        }
    };
    let on_permission = {
        let (sink, pending) = (sink.clone(), pending.clone());
        let mut sequence = 0_u64;
        async move |request: RequestPermissionRequest,
                    responder: Responder<RequestPermissionResponse>,
                    _connection: ConnectionTo<Agent>| {
            sequence += 1;
            let id = format!("acp-{sequence}");
            let verdict = sink.request_approval(approval_request(id.clone(), &request));
            let answer: Answer =
                Box::new(move |allow| responder.respond(RequestPermissionResponse::new(outcome(&request, allow))));
            match verdict {
                ApprovalVerdict::Ask => {
                    let mut pending = pending.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    pending.insert(id, answer);
                    Ok(())
                }
                verdict => answer(verdict == ApprovalVerdict::Allow),
            }
        }
    };
    let conversation = async move |connection: ConnectionTo<Agent>| {
        let client = Implementation::new("basemind-agents", env!("CARGO_PKG_VERSION"));
        connection
            .send_request(InitializeRequest::new(ProtocolVersion::V1).client_info(client))
            .block_task()
            .await?;
        let new_session = NewSessionRequest::new(config.cwd)
            .mcp_servers(config.mcp_servers)
            .meta(config.session_meta);
        let session = connection.send_request(new_session).block_task().await?.session_id;
        sink.emit(Event::HarnessSession(session.0.to_string()));
        if let Some(mode) = config.mode {
            connection
                .send_request(SetSessionModeRequest::new(session.clone(), mode))
                .block_task()
                .await?;
        }
        let mut next = Some(config.brief);
        let mut queued = VecDeque::new();
        loop {
            let Some(text) = next.take().or_else(|| queued.pop_front()) else {
                let mut closed = pin!(connection.incoming_closed().fuse());
                futures::select_biased! {
                    control = controls.next() => match control {
                        Some(Control::FollowUp(text)) => next = Some(text),
                        Some(Control::Answer { id, allow }) => pending.answer(&id, allow, &sink),
                        Some(Control::Stop) | None => return Ok(Ok(())),
                    },
                    () = closed => return Ok(Ok(())),
                }
                continue;
            };
            if let Err(error) = sink.begin_turn() {
                return Ok(Err(error));
            }
            let prompt = PromptRequest::new(session.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
            let mut response = pin!(connection.send_request(prompt).block_task().fuse());
            let mut closed = pin!(connection.incoming_closed().fuse());
            loop {
                futures::select_biased! {
                    response = response => {
                        response?;
                        break;
                    }
                    control = controls.next() => match control {
                        Some(Control::FollowUp(text)) => queued.push_back(text),
                        Some(Control::Answer { id, allow }) => pending.answer(&id, allow, &sink),
                        Some(Control::Stop) | None => {
                            connection.send_notification(CancelNotification::new(session.clone()))?;
                            return Ok(Ok(()));
                        }
                    },
                    () = closed => {
                        let message = "the ACP agent closed its output before the turn ended";
                        return Ok(Err(LaunchError::Protocol(message.to_owned())));
                    }
                }
            }
            let turn = sink.turns();
            sink.emit(Event::TurnCompleted { turn });
            if turn >= sink.max_turns() {
                return Ok(Ok(()));
            }
        }
    };
    Client
        .v1()
        .name("basemind-agents")
        .on_receive_notification(on_update, agent_client_protocol::on_receive_notification!())
        .on_receive_request(on_permission, agent_client_protocol::on_receive_request!())
        .connect_with(Lines::new(outgoing, incoming), conversation)
        .await
        .map_err(|error| protocol_error(&error))?
}

#[cfg(test)]
#[path = "acp_tests.rs"]
mod tests;
