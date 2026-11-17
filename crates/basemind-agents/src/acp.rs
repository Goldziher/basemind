// Grok's launch arguments and `GROK_NO_AUTO_UPDATE`, the `yoloMode` session `_meta`, the Hermes and OpenCode session modes and the allow-once option preference are adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/provider/acp.rs) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! Agents that speak the Agent Client Protocol, through the `agent-client-protocol` crate.
//!
//! The crate owns framing, request ids and the typed messages. This module owns the process
//! (so it lives in the session's process group), maps session updates to [`Event`]s and puts
//! every permission request through the session's approval policy.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, EnvVariable, Implementation, InitializeRequest, McpServer, McpServerStdio, Meta,
    NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionNotification, SessionUpdate, SetSessionModeRequest, TextContent,
    ToolCallStatus, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, Client, Lines};
use futures::channel::mpsc::{Receiver, Sender, channel};
use futures::stream::FusedStream;
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt};

use crate::error::LaunchError;
use crate::event::{
    ApprovalKind, ApprovalRequest, ApprovalSource, Control, Event, ToolCall, ToolState, Tools, approval_detail,
};
use crate::frames::FrameReader;
use crate::harness::{AcpFlavor, HarnessKind};
use crate::pipe;
use crate::registry::Adapter;
use crate::session::{self, CONTROL_QUEUE, ChildIo, CommandPlan, ControlTx, Driver, EventSink, Session, Undelivered};
use crate::spec::{ApprovalVerdict, LaunchConfig, LaunchRequest, LaunchSpec, PermissionMode, Prepared};

/// Drives every [`HarnessKind::Acp`] harness.
///
/// An approval is answered with the request's `allow_once` option and never with
/// `allow_always`: a request without `allow_once` is denied whatever the verdict. A denial
/// takes `reject_once`, then `reject_always`, then the protocol's `cancelled` outcome.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcpAdapter;

fn flavor(spec: &LaunchSpec) -> Result<AcpFlavor, LaunchError> {
    match spec.harness.kind {
        HarnessKind::Acp(flavor) => Ok(flavor),
        other => Err(LaunchError::UnknownHarness(other.id())),
    }
}

/// OpenCode's inline configuration that makes every permission `ask`. It takes precedence over
/// the configuration files: OpenCode 2.3.0 asked under it even with a config file allowing
/// edits and shell commands named by `OPENCODE_CONFIG`.
pub(crate) const OPENCODE_ASK: (&str, &str) = ("OPENCODE_CONFIG_CONTENT", r#"{"permission":"ask"}"#);

/// The session mode to set after `session/new`, if the permission mode needs one.
///
/// What an ACP agent does without asking, in any mode but an authorized bypass, was not
/// verified against a live agent: Cursor, Gemini, Grok and Hermes are run in their default
/// mode, Grok's and OpenCode's read-only modes and Hermes's accept-edits mode are monocode's
/// names, and a generic agent is unknown. Each needs an [`UnverifiedModeOptIn`] naming the
/// harness and the mode. OpenCode supervised needs none: its default wrote a file unasked,
/// but under [`OPENCODE_ASK`] OpenCode 2.3.0 asked before an edit, a shell command and an
/// MCP tool call.
///
/// [`UnverifiedModeOptIn`]: crate::UnverifiedModeOptIn
fn session_mode(spec: &LaunchSpec, prepared: &Prepared, flavor: AcpFlavor) -> Result<Option<String>, LaunchError> {
    let kind = spec.harness.kind;
    let unsupported = |feature| LaunchError::Unsupported {
        harness: kind.id(),
        feature,
    };
    let mode = match (spec.permission_mode, flavor) {
        // Grok takes its bypass as a launch argument and a `_meta` switch instead.
        (PermissionMode::Bypass, AcpFlavor::Grok) => return Ok(None),
        (PermissionMode::Bypass, _) => return Ok(prepared.bypass_mode.clone()),
        (PermissionMode::Supervised, AcpFlavor::OpenCode) => return Ok(None),
        (PermissionMode::Supervised, _) => None,
        // Grok takes this as a launch argument instead of a session mode.
        (PermissionMode::ReadOnly, AcpFlavor::Grok) => None,
        (PermissionMode::ReadOnly, AcpFlavor::OpenCode) => Some("plan".to_owned()),
        (PermissionMode::ReadOnly, _) => return Err(unsupported("a read-only mode")),
        (PermissionMode::AcceptEdits, AcpFlavor::Hermes) => Some("accept_edits".to_owned()),
        (PermissionMode::AcceptEdits, _) => return Err(unsupported("an accept-edits mode")),
    };
    let consented = spec
        .unverified_mode
        .as_ref()
        .is_some_and(|opt_in| opt_in.harness == kind && opt_in.mode == spec.permission_mode);
    if !consented {
        return Err(LaunchError::UnverifiedMode {
            harness: kind.id(),
            mode: spec.permission_mode,
        });
    }
    Ok(mode)
}

impl Adapter for AcpAdapter {
    fn supports(&self, kind: HarnessKind) -> bool {
        matches!(kind, HarnessKind::Acp(_))
    }

    fn plan(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<CommandPlan, LaunchError> {
        self.plan_spec(&LaunchSpec::new(config, request)?)
    }

    fn launch(&self, config: &LaunchConfig, request: &LaunchRequest) -> Result<Session, LaunchError> {
        self.launch_spec(&LaunchSpec::new(config, request)?)
    }
}

impl AcpAdapter {
    pub(crate) fn plan_spec(&self, spec: &LaunchSpec) -> Result<CommandPlan, LaunchError> {
        self.plan_prepared(spec).map(|(plan, _)| plan)
    }

    /// The plan and the checked spec it was built from, checked once per launch.
    pub(crate) fn plan_prepared(&self, spec: &LaunchSpec) -> Result<(CommandPlan, Prepared), LaunchError> {
        let flavor = flavor(spec)?;
        let prepared = spec.prepare()?;
        session_mode(spec, &prepared, flavor)?;
        if spec
            .mcp_server
            .as_ref()
            .is_some_and(|server| !server.allow_tools.is_empty())
        {
            return Err(LaunchError::Unsupported {
                harness: spec.harness.kind.id(),
                feature: "pre-approving MCP tools",
            });
        }
        let mut env = Vec::new();
        let extra = prepared.model_args.iter().map(String::as_str);
        let args: Vec<&str> = match flavor {
            // `model_args` refuses a model or effort for these flavors.
            AcpFlavor::Cursor | AcpFlavor::Hermes => vec!["acp"],
            AcpFlavor::OpenCode => {
                // OpenCode's default edits files and runs commands unasked; it has no bypass
                // mode here, so every launch asks.
                env.push(OPENCODE_ASK);
                vec!["acp"]
            }
            AcpFlavor::Gemini => vec!["--experimental-acp"],
            AcpFlavor::Generic => Vec::new(),
            AcpFlavor::Grok => {
                env.push(("GROK_NO_AUTO_UPDATE", "1"));
                let mut args = vec!["--no-auto-update"];
                if spec.permission_mode == PermissionMode::ReadOnly {
                    args.extend(["--permission-mode", "plan"]);
                }
                args.extend(["agent", "--no-leader"]);
                // Grok's model and effort are options of `agent`, so they precede `stdio`.
                args.extend(extra);
                if prepared.bypass(spec) {
                    args.push("--always-approve");
                }
                args.push("stdio");
                args
            }
        };
        // ACP hands the MCP server over in `session/new`, not on the command line.
        let plan = spec.plan(
            &prepared,
            args.into_iter().map(str::to_owned).collect(),
            Vec::new(),
            &env,
        )?;
        Ok((plan, prepared))
    }

    fn launch_spec(&self, spec: &LaunchSpec) -> Result<Session, LaunchError> {
        let (plan, prepared) = self.plan_prepared(spec)?;
        let config = Config::new(spec, &plan, &prepared)?;
        let (tx, controls) = channel(CONTROL_QUEUE);
        let driver = Box::new(AcpDriver { config, controls });
        let tx = Mutex::new(tx);
        let control: ControlTx = Box::new(move |control| {
            let mut tx = tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            tx.try_send(control).map_err(|error| {
                if error.is_full() {
                    Undelivered::Full
                } else {
                    Undelivered::Closed
                }
            })
        });
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
    pub fn new(spec: &LaunchSpec, plan: &CommandPlan, prepared: &Prepared) -> Result<Self, LaunchError> {
        let flavor = flavor(spec)?;
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
            .zip(&prepared.mcp_command)
            .map(|(server, command)| {
                McpServer::Stdio(
                    McpServerStdio::new(&server.name, command)
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
            mode: session_mode(spec, prepared, flavor)?,
        })
    }
}

struct AcpDriver {
    config: Config,
    controls: Receiver<Control>,
}

impl Driver for AcpDriver {
    fn run(self: Box<Self>, io: ChildIo, sink: EventSink) -> Result<(), LaunchError> {
        let (line_tx, incoming) = lines_channel(INCOMING_LINES);
        let stdout = pipe::Reader::new(io.stdout, io.abandon.clone());
        std::thread::spawn(move || pump_lines(stdout, line_tx));
        let (out_tx, mut out_rx) = lines_channel::<String>(OUTGOING_LINES);
        let mut stdin = pipe::Writer::new(io.stdin, io.abandon)?.for_stdin(io.stdin_state);
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
        futures::executor::block_on(converse(FailFast(out_tx), incoming, self.config, sink, self.controls))
    }
}

/// The connection's outgoing lines. The protocol crate queues what it sends without bound and
/// answers every request it receives, so an agent that floods requests and never reads would
/// grow that queue until the wall-time limit. This sink never waits: a full queue fails the
/// connection, which ends the session with a protocol error.
struct FailFast(Sender<String>);

impl Sink<String> for FailFast {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, line: String) -> Result<(), Self::Error> {
        self.get_mut().0.try_send(line).map_err(|error| {
            if error.is_full() {
                std::io::Error::other(format!("the ACP agent left {OUTGOING_LINES} lines unread"))
            } else {
                std::io::ErrorKind::BrokenPipe.into()
            }
        })
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().0.close_channel();
        Poll::Ready(Ok(()))
    }
}

/// How many lines read from the agent may wait for the connection. Each may be a 16 MiB
/// frame, so the queue is kept short.
const INCOMING_LINES: usize = 2;
/// How many lines to the agent may wait for it to read them; beyond that the session fails.
const OUTGOING_LINES: usize = 16;

/// A channel that holds at most `lines` items with its one sender: a `futures` channel holds
/// its buffer plus one item per sender.
fn lines_channel<T>(lines: usize) -> (Sender<T>, Receiver<T>) {
    channel(lines - 1)
}

/// Reads the agent's output as lines of at most [`MAX_FRAME_BYTES`](crate::frames::MAX_FRAME_BYTES)
/// into `tx`. A line over the limit or not in UTF-8 is passed on as an
/// [`InvalidData`](std::io::ErrorKind::InvalidData) error and ends the reading.
fn pump_lines(reader: impl Read, mut tx: Sender<std::io::Result<String>>) {
    let mut frames = FrameReader::new(reader);
    loop {
        let line = match frames.next_frame() {
            Ok(None) => return,
            Ok(Some(frame)) => String::from_utf8(frame).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the agent sent a line that is not UTF-8",
                )
            }),
            Err(error) => Err(error),
        };
        let last = line.is_err();
        if futures::executor::block_on(tx.send(line)).is_err() || last {
            return;
        }
    }
}

type Answer = Box<dyn FnOnce(bool) -> Result<bool, agent_client_protocol::Error> + Send>;

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
            Some(Ok(allowed)) => sink.resolved(id.to_owned(), allowed, ApprovalSource::Caller),
            Some(Err(error)) => sink.emit(Event::Error(crate::event::error_line(&format!(
                "could not answer approval {id:?}: {error}"
            )))),
            None => sink.emit(Event::Error(format!("no approval request {id:?} is pending"))),
        }
    }

    /// Denies every request still pending, as the turn they belong to has ended.
    fn withdraw_all(&self, sink: &EventSink) {
        let answers = std::mem::take(&mut *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for (id, answer) in answers {
            // The agent may have dropped the request with its turn; it is moot either way.
            let _ = answer(false);
            sink.resolved(id, false, ApprovalSource::Withdrawn);
        }
    }
}

/// The option that answers `request`, and whether it lets the agent proceed. An allow takes
/// the `allow_once` option and nothing broader: without one it is answered as a denial, so a
/// single approval never becomes a standing one. A denial takes `reject_once`, then
/// `reject_always`, then the protocol's `cancelled` outcome.
fn outcome(request: &RequestPermissionRequest, allow: bool) -> (RequestPermissionOutcome, bool) {
    let select =
        |kind: PermissionOptionKind| {
            request.options.iter().find(|option| option.kind == kind).map(|option| {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
            })
        };
    if allow && let Some(selected) = select(PermissionOptionKind::AllowOnce) {
        return (selected, true);
    }
    let denial = select(PermissionOptionKind::RejectOnce)
        .or_else(|| select(PermissionOptionKind::RejectAlways))
        .unwrap_or(RequestPermissionOutcome::Cancelled);
    (denial, false)
}

/// Whether `request` offers a way to allow it just once.
fn can_allow_once(request: &RequestPermissionRequest) -> bool {
    request
        .options
        .iter()
        .any(|option| option.kind == PermissionOptionKind::AllowOnce)
}

/// The request for a `session/request_permission`. The grant is the tool call the agent
/// described, so the title follows its kind and never its agent-written title.
fn approval_request(id: String, request: &RequestPermissionRequest) -> ApprovalRequest {
    let (kind, title) = match request.tool_call.fields.kind {
        Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => (ApprovalKind::FileChange, "Allow file changes?"),
        Some(ToolKind::Execute) => (ApprovalKind::Command, "Allow this command?"),
        Some(ToolKind::Fetch) => (ApprovalKind::Fetch, "Allow this network request?"),
        // Switching the session mode changes what the agent may do unasked from then on.
        Some(ToolKind::SwitchMode) => (ApprovalKind::Permissions, "Allow a change of permission mode?"),
        _ => (ApprovalKind::Tool, "Allow this tool call?"),
    };
    let mut grant = serde_json::to_string_pretty(&request.tool_call).unwrap_or_default();
    if !can_allow_once(request) {
        grant.push_str("\nThe agent offers no allow-once option: an allow is answered as a denial.");
    }
    ApprovalRequest {
        id,
        kind,
        title: title.to_owned(),
        detail: approval_detail(&grant, None),
        params: serde_json::to_value(request).unwrap_or_default(),
    }
}

/// The session mode the agent reports, as a mode or as its mode config option, against the one
/// the session runs in: the mode the adapter set, else the one `session/new` returned. A switch the agent makes by itself
/// changes what it does unasked, so it ends the session.
struct ModeWatch {
    state: Mutex<ModeState>,
    switched: Sender<String>,
}

#[derive(Default)]
struct ModeState {
    /// Whether the session is open and `expected` is known.
    opened: bool,
    expected: Option<String>,
    /// The mode last reported before the session opened.
    reported: Option<String>,
}

impl ModeWatch {
    fn new() -> (Self, Receiver<String>) {
        let (switched, switches) = channel(1);
        let watch = Self {
            state: Mutex::default(),
            switched,
        };
        (watch, switches)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ModeState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a `current_mode_update`.
    fn report(&self, mode: String) {
        let mut state = self.lock();
        if !state.opened {
            state.reported = Some(mode);
        } else if state.expected.as_deref() != Some(mode.as_str()) {
            // One switch is enough to end the session; a full channel already holds one.
            let _ = self.switched.clone().try_send(mode);
        }
    }

    /// Marks the session open in mode `expected` (`None` when no mode is known).
    fn open(&self, expected: Option<String>) {
        let mut state = self.lock();
        state.opened = true;
        if let Some(reported) = state
            .reported
            .take()
            .filter(|mode| expected.as_deref() != Some(mode.as_str()))
        {
            let _ = self.switched.clone().try_send(reported);
        }
        state.expected = expected;
    }
}

/// The tool calls seen so far, so an update can be reported as the whole call, and the mode
/// the agent reports.
#[derive(Clone)]
struct Updates(Arc<Mutex<Tools>>, Arc<ModeWatch>);

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
            SessionUpdate::CurrentModeUpdate(update) => self.1.report(update.current_mode_id.0.to_string()),
            SessionUpdate::ConfigOptionUpdate(update) => {
                if let Some(mode) = mode_option(&update.config_options) {
                    self.1.report(mode);
                }
            }
            _ => {}
        }
    }

    fn merge(&self, id: &str, fields: ToolCallUpdateFields, sink: &EventSink) {
        let mut tools = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tool = tools.get_or_insert(id, || ToolCall::new(id, "tool"));
        if let Some(name) = fields.name.or(fields.title) {
            tool.name = name;
        }
        if let Some(input) = fields.raw_input {
            tool.input = input.to_string();
            tool.bound_input();
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

/// The value of the mode-category select option among `options`: OpenCode 2.3.0 has no session
/// modes and reports its mode (`build`, `plan`) this way instead.
fn mode_option(options: &[SessionConfigOption]) -> Option<String> {
    options
        .iter()
        .find_map(|option| match (&option.category, &option.kind) {
            (Some(SessionConfigOptionCategory::Mode), SessionConfigKind::Select(select)) => {
                Some(select.current_value.0.to_string())
            }
            _ => None,
        })
}

/// The error for an agent that switched its session mode by itself.
fn self_switch(mode: Option<String>) -> LaunchError {
    LaunchError::Protocol(format!(
        "the ACP agent switched itself to session mode {:?}",
        mode.unwrap_or_default()
    ))
}

fn protocol_error(error: &agent_client_protocol::Error) -> LaunchError {
    LaunchError::Protocol(format!("the ACP agent reported: {error}"))
}

/// Runs one ACP conversation over a pair of line channels until its turns are over.
pub(crate) async fn converse<O, I, C>(
    outgoing: O,
    incoming: I,
    config: Config,
    sink: EventSink,
    mut controls: C,
) -> Result<(), LaunchError>
where
    O: Sink<String, Error = std::io::Error> + Send + 'static,
    I: Stream<Item = std::io::Result<String>> + Send + 'static,
    C: Stream<Item = Control> + FusedStream + Unpin + Send + 'static,
{
    let pending = Pending::default();
    let (watch, mut switches) = ModeWatch::new();
    let watch = Arc::new(watch);
    let updates = Updates(Arc::default(), watch.clone());
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
            let once = can_allow_once(&request);
            let waiting = pending
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len();
            let verdict = sink.request_approval_once(approval_request(id.clone(), &request), once, waiting);
            let answer: Answer = Box::new(move |allow| {
                let (outcome, allowed) = outcome(&request, allow);
                responder.respond(RequestPermissionResponse::new(outcome))?;
                Ok(allowed)
            });
            match verdict {
                ApprovalVerdict::Ask => {
                    let mut pending = pending.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    pending.insert(id, answer);
                    Ok(())
                }
                verdict => answer(verdict == ApprovalVerdict::Allow).map(drop),
            }
        }
    };
    let conversation = async move |connection: ConnectionTo<Agent>| {
        let Config {
            cwd,
            brief,
            mcp_servers,
            session_meta,
            mode,
        } = config;
        let setup = async {
            let client = Implementation::new("basemind-agents", env!("CARGO_PKG_VERSION"));
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1).client_info(client))
                .block_task()
                .await?;
            let new_session = NewSessionRequest::new(cwd).mcp_servers(mcp_servers).meta(session_meta);
            let opened = connection.send_request(new_session).block_task().await?;
            let session = opened.session_id;
            sink.emit(Event::HarnessSession(session.0.to_string()));
            let current = opened
                .modes
                .map(|modes| modes.current_mode_id.0.to_string())
                .or_else(|| opened.config_options.as_deref().and_then(mode_option));
            if let Some(mode) = &mode {
                connection
                    .send_request(SetSessionModeRequest::new(session.clone(), mode.clone()))
                    .block_task()
                    .await?;
            }
            Ok::<_, agent_client_protocol::Error>((session, mode.or(current)))
        };
        let mut queued = VecDeque::new();
        // A stop, or an agent that hangs up, must end the session while it is still opening.
        let session = {
            let mut setup = pin!(setup.fuse());
            let mut closed = pin!(connection.incoming_closed().fuse());
            loop {
                futures::select_biased! {
                    opened = setup => {
                        let (session, mode) = opened?;
                        watch.open(mode);
                        break session;
                    }
                    control = controls.next() => match control {
                        Some(Control::FollowUp(text)) => queued.push_back(text),
                        Some(Control::Answer { id, allow }) => pending.answer(&id, allow, &sink),
                        Some(Control::Stop) | None => {
                            pending.withdraw_all(&sink);
                            return Ok(Ok(()));
                        }
                    },
                    () = closed => {
                        let message = "the ACP agent closed its output before the session opened";
                        return Ok(Err(LaunchError::Protocol(message.to_owned())));
                    }
                }
            }
        };
        let mut next = Some(brief);
        loop {
            let Some(text) = next.take().or_else(|| queued.pop_front()) else {
                let mut closed = pin!(connection.incoming_closed().fuse());
                futures::select_biased! {
                    mode = switches.next() => {
                        pending.withdraw_all(&sink);
                        return Ok(Err(self_switch(mode)));
                    }
                    control = controls.next() => match control {
                        Some(Control::FollowUp(text)) => next = Some(text),
                        Some(Control::Answer { id, allow }) => pending.answer(&id, allow, &sink),
                        Some(Control::Stop) | None => {
                            pending.withdraw_all(&sink);
                            return Ok(Ok(()));
                        }
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
                    mode = switches.next() => {
                        pending.withdraw_all(&sink);
                        connection.send_notification(CancelNotification::new(session.clone()))?;
                        return Ok(Err(self_switch(mode)));
                    }
                    response = response => {
                        response?;
                        break;
                    }
                    control = controls.next() => match control {
                        Some(Control::FollowUp(text)) => queued.push_back(text),
                        Some(Control::Answer { id, allow }) => pending.answer(&id, allow, &sink),
                        Some(Control::Stop) | None => {
                            // The protocol has the client answer what is pending when it cancels.
                            pending.withdraw_all(&sink);
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
            pending.withdraw_all(&sink);
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
