use std::path::Path;
use std::thread::JoinHandle;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};

use super::*;
use crate::spec::{ApprovalPolicy, BypassOptIn, McpServerSpec, UnverifiedModeOptIn};
use crate::test_support::{EXECUTABLE, MCP_COMMAND, always, spec};

/// The agent's end of an in-memory connection. A test scripts it frame by frame, so every
/// message the client sends is the real serialization of the protocol crate.
struct FakeAgent {
    to_client: UnboundedSender<std::io::Result<String>>,
    from_client: UnboundedReceiver<String>,
    /// The session handle's side: what a caller would send while the turn runs.
    controls: UnboundedSender<Control>,
}

impl FakeAgent {
    fn recv(&mut self) -> Value {
        let line = futures::executor::block_on(self.from_client.next()).expect("the client hung up");
        serde_json::from_str(&line).expect("the client sent a line that is not JSON")
    }

    fn expect(&mut self, method: &str) -> Value {
        let frame = self.recv();
        assert_eq!(frame["method"], method, "unexpected frame {frame}");
        frame
    }

    fn send(&mut self, frame: &Value) {
        self.send_line(&frame.to_string());
    }

    fn send_line(&mut self, line: &str) {
        self.to_client
            .unbounded_send(Ok(line.to_owned()))
            .expect("the client hung up");
    }

    fn reply(&mut self, request: &Value, result: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
    }

    fn update(&mut self, update: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "sess-1", "update": update,
        }}));
    }

    /// Answers `initialize` and `session/new`, returning the `session/new` request.
    fn open(&mut self) -> Value {
        let initialize = self.expect("initialize");
        self.reply(&initialize, &json!({"protocolVersion": 1}));
        let new_session = self.expect("session/new");
        self.reply(&new_session, &json!({"sessionId": "sess-1"}));
        new_session
    }

    fn request_permission(&mut self, options: &Value) {
        self.send(
            &json!({"jsonrpc": "2.0", "id": 100, "method": "session/request_permission", "params": {
                "sessionId": "sess-1",
                "toolCall": {"toolCallId": "t1", "title": "Run the tests", "kind": "execute"},
                "options": options,
            }}),
        );
    }

    fn permission_outcome(&mut self) -> Value {
        let answer = self.recv();
        assert_eq!(answer["id"], 100, "unexpected frame {answer}");
        answer["result"]["outcome"].clone()
    }

    /// Asks for permission to run a command and returns the client's answer.
    fn ask_permission(&mut self, options: &Value) -> Value {
        self.request_permission(options);
        self.permission_outcome()
    }
}

fn standard_options() -> Value {
    json!([
        {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
        {"optionId": "once", "name": "Allow", "kind": "allow_once"},
        {"optionId": "no", "name": "Reject", "kind": "reject_once"},
    ])
}

struct Run {
    result: Result<(), LaunchError>,
    events: Vec<Event>,
}

/// Runs the conversation for `spec` against `script`. `controls` are queued before it starts.
fn converse_with(
    spec: &LaunchSpec,
    controls: Vec<Control>,
    script: impl FnOnce(&mut FakeAgent) + Send + 'static,
) -> Run {
    let (plan, prepared) = AcpAdapter.plan_prepared(spec).unwrap();
    let config = Config::new(spec, &plan, &prepared).unwrap();
    let (sink, events) = EventSink::for_test(spec.approval_policy.clone(), spec.limits.max_turns);
    let (to_client, incoming) = unbounded();
    let (out_tx, from_client) = unbounded::<String>();
    let (control_tx, control_rx) = unbounded();
    for control in controls {
        control_tx.unbounded_send(control).unwrap();
    }
    // A session keeps its control channel open for as long as it lives; so does this.
    let session_side = control_tx.clone();
    let agent: JoinHandle<()> = std::thread::spawn(move || {
        let mut agent = FakeAgent {
            to_client,
            from_client,
            controls: control_tx,
        };
        script(&mut agent);
    });
    let outgoing = out_tx.sink_map_err(std::io::Error::other);
    let result = futures::executor::block_on(converse(outgoing, incoming, config, sink, control_rx));
    drop(session_side);
    if let Err(panic) = agent.join() {
        std::panic::resume_unwind(panic);
    }
    Run {
        result,
        events: events.try_iter().collect(),
    }
}

/// A spec for `flavor`, with the consent its supervised mode needs.
fn acp_spec(flavor: AcpFlavor, root: &Path) -> LaunchSpec {
    let mut spec = spec(HarnessKind::Acp(flavor), root);
    spec.unverified_mode = Some(UnverifiedModeOptIn {
        harness: HarnessKind::Acp(flavor),
        mode: PermissionMode::Supervised,
    });
    spec
}

/// `spec` in `mode`, with the consent that mode needs.
fn in_mode(mut spec: LaunchSpec, mode: PermissionMode) -> LaunchSpec {
    spec.permission_mode = mode;
    spec.unverified_mode = Some(UnverifiedModeOptIn {
        harness: spec.harness.kind,
        mode,
    });
    spec
}

fn with_policy(mut spec: LaunchSpec, policy: Option<Arc<dyn ApprovalPolicy>>) -> LaunchSpec {
    spec.approval_policy = policy;
    spec
}

#[test]
fn a_turn_carries_the_brief_and_reports_text_and_tool_calls() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), move |agent| {
        let new_session = agent.open();
        assert_eq!(new_session["params"]["cwd"], json!(canonical));
        assert_eq!(new_session["params"]["mcpServers"], json!([]));
        assert!(new_session["params"].get("_meta").is_none());
        let prompt = agent.expect("session/prompt");
        assert_eq!(prompt["params"]["sessionId"], "sess-1");
        assert_eq!(
            prompt["params"]["prompt"],
            json!([{"type": "text", "text": "Summarize the README."}])
        );
        agent.update(&json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "reading"}}));
        agent.update(
            &json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read README.md",
            "kind": "read", "status": "in_progress", "rawInput": {"path": "README.md"}}),
        );
        agent.update(
            &json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "rawOutput": "# basemind"}),
        );
        agent.update(
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "It is a code map."}}),
        );
        agent.update(&json!({"sessionUpdate": "plan", "entries": []}));
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
    let tool = |output: &str, state| {
        Event::ToolCall(ToolCall {
            id: "t1".to_owned(),
            name: "Read README.md".to_owned(),
            input: "{\"path\":\"README.md\"}".to_owned(),
            output: output.to_owned(),
            state,
        })
    };
    assert_eq!(
        run.events,
        [
            Event::HarnessSession("sess-1".to_owned()),
            Event::Reasoning("reading".to_owned()),
            tool("", ToolState::Running),
            tool("# basemind", ToolState::Completed),
            Event::Text("It is a code map.".to_owned()),
            Event::TurnCompleted { turn: 1 },
        ]
    );
}

#[test]
fn the_mcp_server_is_passed_in_session_new_with_the_identity_by_value() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = acp_spec(AcpFlavor::Cursor, root.path());
    spec.mcp_server = Some(McpServerSpec::basemind_serve(MCP_COMMAND));
    let run = converse_with(&spec, Vec::new(), |agent| {
        let new_session = agent.open();
        let servers = new_session["params"]["mcpServers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "basemind");
        assert_eq!(servers[0]["command"], MCP_COMMAND);
        assert_eq!(servers[0]["args"], json!(["serve"]));
        let env = servers[0]["env"].as_array().unwrap();
        assert!(
            env.contains(&json!({"name": "BASEMIND_AGENT_ID", "value": "launched-agent"})),
            "{env:?}"
        );
        assert!(
            env.contains(&json!({"name": "BASEMIND_HOST", "value": "cursor"})),
            "{env:?}"
        );
        let prompt = agent.expect("session/prompt");
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();

    spec.mcp_server.as_mut().unwrap().allow_tools = vec!["agent_post".to_owned()];
    assert!(matches!(
        AcpAdapter.plan_spec(&spec),
        Err(LaunchError::Unsupported { .. })
    ));
}

#[test]
fn a_permission_request_is_rejected_without_a_policy() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        let outcome = agent.ask_permission(&standard_options());
        assert_eq!(outcome, json!({"outcome": "selected", "optionId": "no"}));
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
    let Event::ApprovalRequest(request) = &run.events[1] else {
        panic!("unexpected event {:?}", run.events[1]);
    };
    // The agent-written title "Run the tests" is part of the grant, not the question.
    assert_eq!(request.title, "Allow this command?");
    assert_eq!(request.kind, crate::event::ApprovalKind::Command);
    assert!(
        request.detail.contains("\"title\": \"Run the tests\""),
        "{}",
        request.detail
    );
    assert_eq!(request.params["toolCall"]["toolCallId"], "t1");
    assert_eq!(request.params["options"].as_array().unwrap().len(), 3);
    assert_eq!(
        run.events[2],
        Event::ApprovalResolved {
            id: "acp-1".to_owned(),
            allowed: false,
            source: ApprovalSource::DefaultDeny,
        }
    );
}

#[test]
fn an_allowing_policy_selects_allow_once_over_allow_always() {
    let root = tempfile::tempdir().unwrap();
    let spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), always(ApprovalVerdict::Allow));
    let run = converse_with(&spec, Vec::new(), |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        let outcome = agent.ask_permission(&standard_options());
        assert_eq!(outcome, json!({"outcome": "selected", "optionId": "once"}));
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
}

#[test]
fn a_denial_without_a_reject_option_cancels_the_request() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        let only_allow = json!([{"optionId": "once", "name": "Allow", "kind": "allow_once"}]);
        assert_eq!(agent.ask_permission(&only_allow), json!({"outcome": "cancelled"}));
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
}

#[test]
fn a_deferred_request_is_answered_by_the_caller() {
    let root = tempfile::tempdir().unwrap();
    // The policy runs inside the request handler, on the conversation's own thread, and the
    // request is pending before that thread reads another control. Its signal therefore
    // tells the agent's thread when an answer can no longer arrive too early.
    let (asked_tx, asked) = std::sync::mpsc::channel();
    let policy = move |_: &ApprovalRequest| {
        asked_tx.send(()).unwrap();
        ApprovalVerdict::Ask
    };
    let spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), Some(Arc::new(policy)));
    let answer = |id: &str| Control::Answer {
        id: id.to_owned(),
        allow: true,
    };
    let run = converse_with(&spec, vec![answer("acp-9")], move |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        agent.request_permission(&standard_options());
        asked.recv().unwrap();
        agent.controls.unbounded_send(answer("acp-1")).unwrap();
        assert_eq!(
            agent.permission_outcome(),
            json!({"outcome": "selected", "optionId": "once"})
        );
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
    assert!(
        run.events
            .contains(&Event::Error("no approval request \"acp-9\" is pending".to_owned()))
    );
    assert!(run.events.contains(&Event::ApprovalResolved {
        id: "acp-1".to_owned(),
        allowed: true,
        source: ApprovalSource::Caller,
    }));
}

#[test]
fn grok_gets_its_bypass_switches_only_with_an_opt_in() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = acp_spec(AcpFlavor::Grok, root.path());
    let plan = AcpAdapter.plan_spec(&spec).unwrap();
    assert_eq!(plan.args, ["--no-auto-update", "agent", "--no-leader", "stdio"]);
    assert_eq!(plan.env["GROK_NO_AUTO_UPDATE"], "1");
    let run = converse_with(&spec, Vec::new(), |agent| {
        let new_session = agent.open();
        assert!(!new_session.to_string().contains("yolo"), "{new_session}");
        let prompt = agent.expect("session/prompt");
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();

    spec.permission_mode = PermissionMode::Bypass;
    assert!(matches!(
        AcpAdapter.plan_spec(&spec),
        Err(LaunchError::BypassNotAuthorized { harness: "grok", .. })
    ));
    spec.bypass = Some(BypassOptIn {
        harness: HarnessKind::Acp(AcpFlavor::Grok),
        mode: "yoloMode".to_owned(),
    });
    let plan = AcpAdapter.plan_spec(&spec).unwrap();
    assert_eq!(
        plan.args,
        ["--no-auto-update", "agent", "--no-leader", "--always-approve", "stdio"]
    );
    let run = converse_with(&spec, Vec::new(), |agent| {
        let new_session = agent.open();
        assert_eq!(new_session["params"]["_meta"], json!({"yoloMode": true}));
        let prompt = agent.expect("session/prompt");
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
}

#[test]
fn a_session_mode_is_set_only_for_an_authorized_bypass() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = acp_spec(AcpFlavor::Hermes, root.path());
    spec.permission_mode = PermissionMode::Bypass;
    spec.bypass = Some(BypassOptIn {
        harness: HarnessKind::Acp(AcpFlavor::Hermes),
        mode: "yolo".to_owned(),
    });
    assert!(matches!(
        AcpAdapter.plan_spec(&spec),
        Err(LaunchError::BypassNotAuthorized { .. })
    ));
    spec.bypass.as_mut().unwrap().mode = "dont_ask".to_owned();
    let run = converse_with(&spec, Vec::new(), |agent| {
        agent.open();
        let set_mode = agent.expect("session/set_mode");
        assert_eq!(set_mode["params"], json!({"sessionId": "sess-1", "modeId": "dont_ask"}));
        agent.reply(&set_mode, &json!({}));
        let prompt = agent.expect("session/prompt");
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();

    let mut generic = acp_spec(AcpFlavor::Generic, root.path());
    generic.permission_mode = PermissionMode::Bypass;
    generic.bypass = Some(BypassOptIn {
        harness: HarnessKind::Acp(AcpFlavor::Generic),
        mode: "full-auto".to_owned(),
    });
    let (plan, prepared) = AcpAdapter.plan_prepared(&generic).unwrap();
    assert_eq!(
        Config::new(&generic, &plan, &prepared).unwrap().mode.as_deref(),
        Some("full-auto")
    );

    let mut cursor = acp_spec(AcpFlavor::Cursor, root.path());
    cursor.bypass = Some(BypassOptIn {
        harness: HarnessKind::Acp(AcpFlavor::Cursor),
        mode: "yolo".to_owned(),
    });
    assert!(matches!(
        AcpAdapter.plan_spec(&cursor),
        Err(LaunchError::Unsupported { .. })
    ));
}

#[test]
fn launch_arguments_follow_the_flavor_and_unsupported_modes_are_refused() {
    let root = tempfile::tempdir().unwrap();
    for (flavor, program, args) in [
        (AcpFlavor::Cursor, "cursor-agent", vec!["acp"]),
        (AcpFlavor::Hermes, "hermes", vec!["acp"]),
        (AcpFlavor::OpenCode, "opencode", vec!["acp"]),
        (AcpFlavor::Gemini, "gemini", vec!["--experimental-acp"]),
        (AcpFlavor::Generic, "acp", vec![]),
    ] {
        let plan = AcpAdapter.plan_spec(&acp_spec(flavor, root.path())).unwrap();
        assert_eq!(HarnessKind::Acp(flavor).default_executable(), program);
        assert_eq!(plan.program, PathBuf::from(EXECUTABLE));
        assert_eq!(plan.args, args);
    }
    let read_only = in_mode(acp_spec(AcpFlavor::Grok, root.path()), PermissionMode::ReadOnly);
    let plan = AcpAdapter.plan_spec(&read_only).unwrap();
    assert_eq!(plan.args[..3], ["--no-auto-update", "--permission-mode", "plan"]);

    let mut cursor = acp_spec(AcpFlavor::Cursor, root.path());
    cursor.permission_mode = PermissionMode::ReadOnly;
    assert!(matches!(
        AcpAdapter.plan_spec(&cursor),
        Err(LaunchError::Unsupported { .. })
    ));
    cursor.permission_mode = PermissionMode::AcceptEdits;
    assert!(matches!(
        AcpAdapter.plan_spec(&cursor),
        Err(LaunchError::Unsupported { .. })
    ));

    let mut gemini = acp_spec(AcpFlavor::Gemini, root.path());
    gemini.model = Some("gemini-2.5-pro".to_owned());
    assert!(matches!(
        AcpAdapter.plan_spec(&gemini),
        Err(LaunchError::Unsupported { harness: "gemini", .. })
    ));

    let mut grok = acp_spec(AcpFlavor::Grok, root.path());
    grok.model = Some("grok-4".to_owned());
    grok.effort = Some("high".to_owned());
    let plan = AcpAdapter.plan_spec(&grok).unwrap();
    assert_eq!(
        plan.args,
        [
            "--no-auto-update",
            "agent",
            "--no-leader",
            "--model",
            "grok-4",
            "--reasoning-effort",
            "high",
            "stdio"
        ]
    );
}

#[test]
fn an_agent_that_hangs_up_mid_turn_fails_the_session() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        agent.open();
        agent.expect("session/prompt");
    });
    let error = run.result.unwrap_err();
    assert!(matches!(error, LaunchError::Protocol(_)), "{error}");
    assert!(!run.events.contains(&Event::TurnCompleted { turn: 1 }));
}

#[test]
fn an_error_reply_fails_the_session() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        let initialize = agent.expect("initialize");
        agent.reply(&initialize, &json!({"protocolVersion": 1}));
        let new_session = agent.expect("session/new");
        agent.send(&json!({"jsonrpc": "2.0", "id": new_session["id"], "error": {
            "code": -32000, "message": "Authentication required",
        }}));
    });
    let error = run.result.unwrap_err();
    assert!(error.to_string().contains("Authentication required"), "{error}");
}

#[test]
fn malformed_and_truncated_lines_do_not_derail_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        agent.send_line("Loading model...");
        agent.send_line("{\"jsonrpc\":\"2.0\",\"method\":\"session/upd");
        agent.send_line("{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":7}}");
        agent.update(
            &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Still here."}}),
        );
        // The client may answer a malformed frame; skip to the end of the turn.
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
        while futures::executor::block_on(agent.from_client.next()).is_some() {}
    });
    run.result.unwrap();
    assert!(
        run.events.contains(&Event::Text("Still here.".to_owned())),
        "{:?}",
        run.events
    );
    assert_eq!(run.events.last(), Some(&Event::TurnCompleted { turn: 1 }));
}

#[test]
fn stop_cancels_the_turn_through_the_protocol() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        agent.open();
        agent.expect("session/prompt");
        agent.controls.unbounded_send(Control::Stop).unwrap();
        let cancel = agent.expect("session/cancel");
        assert_eq!(cancel["params"]["sessionId"], "sess-1");
        // Stay connected until the client hangs up, as a real agent's pipe would.
        while futures::executor::block_on(agent.from_client.next()).is_some() {}
    });
    run.result.unwrap();
    assert!(!run.events.contains(&Event::TurnCompleted { turn: 1 }));
}

#[test]
fn a_follow_up_runs_as_a_second_turn_up_to_the_limit() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = acp_spec(AcpFlavor::Cursor, root.path());
    spec.limits.max_turns = 2;
    let follow_ups = vec![
        Control::FollowUp("Now fix it.".to_owned()),
        Control::FollowUp("And once more.".to_owned()),
    ];
    let run = converse_with(&spec, follow_ups, |agent| {
        agent.open();
        let first = agent.expect("session/prompt");
        agent.reply(&first, &json!({"stopReason": "end_turn"}));
        let second = agent.expect("session/prompt");
        assert_eq!(second["params"]["prompt"][0]["text"], "Now fix it.");
        agent.reply(&second, &json!({"stopReason": "end_turn"}));
        assert!(
            futures::executor::block_on(agent.from_client.next()).is_none(),
            "a third turn was started"
        );
    });
    run.result.unwrap();
    assert_eq!(
        run.events[1..],
        [Event::TurnCompleted { turn: 1 }, Event::TurnCompleted { turn: 2 }]
    );
}

#[test]
fn permission_outcomes_prefer_the_narrowest_option() {
    let request = |kinds: &[(&str, PermissionOptionKind)]| {
        let options = kinds
            .iter()
            .map(|(id, kind)| {
                agent_client_protocol::v1::schema::PermissionOption::new((*id).to_owned(), (*id).to_owned(), *kind)
            })
            .collect();
        let call = agent_client_protocol::v1::schema::ToolCallUpdate::new("t1", ToolCallUpdateFields::new());
        RequestPermissionRequest::new("sess-1", call, options)
    };
    let selected = |outcome| match outcome {
        RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.0.to_string()),
        _ => None,
    };
    let only_always = request(&[
        ("always", PermissionOptionKind::AllowAlways),
        ("never", PermissionOptionKind::RejectAlways),
    ]);
    let (allowed_outcome, allowed) = outcome(&only_always, true);
    assert_eq!(selected(allowed_outcome).as_deref(), Some("never"));
    assert!(!allowed, "an allow without allow_once went through");
    let (denied, allowed) = outcome(&only_always, false);
    assert_eq!(selected(denied).as_deref(), Some("never"));
    assert!(!allowed);
    let once = request(&[
        ("always", PermissionOptionKind::AllowAlways),
        ("once", PermissionOptionKind::AllowOnce),
    ]);
    let (allowed_outcome, allowed) = outcome(&once, true);
    assert_eq!(selected(allowed_outcome).as_deref(), Some("once"));
    assert!(allowed);
    let none = request(&[]);
    assert_eq!(selected(outcome(&none, true).0), None);
    assert_eq!(selected(outcome(&none, false).0), None);
}

#[test]
fn an_allow_without_an_allow_once_option_is_answered_as_a_denial() {
    let only_always = json!([
        {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
        {"optionId": "no", "name": "Reject", "kind": "reject_once"},
    ]);
    let root = tempfile::tempdir().unwrap();
    let spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), always(ApprovalVerdict::Allow));
    let options = only_always.clone();
    let run = converse_with(&spec, Vec::new(), move |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        assert_eq!(
            agent.ask_permission(&options),
            json!({"outcome": "selected", "optionId": "no"})
        );
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
    assert!(run.events.contains(&Event::ApprovalResolved {
        id: "acp-1".to_owned(),
        allowed: false,
        source: ApprovalSource::Policy,
    }));

    // As in the deferred test above: the policy's signal says the request is pending.
    let (asked_tx, asked) = std::sync::mpsc::channel();
    let policy = move |_: &ApprovalRequest| {
        asked_tx.send(()).unwrap();
        ApprovalVerdict::Ask
    };
    let spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), Some(Arc::new(policy)));
    let run = converse_with(&spec, Vec::new(), move |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        agent.request_permission(&only_always);
        asked.recv().unwrap();
        agent
            .controls
            .unbounded_send(Control::Answer {
                id: "acp-1".to_owned(),
                allow: true,
            })
            .unwrap();
        assert_eq!(
            agent.permission_outcome(),
            json!({"outcome": "selected", "optionId": "no"})
        );
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
    });
    run.result.unwrap();
    assert!(run.events.contains(&Event::ApprovalResolved {
        id: "acp-1".to_owned(),
        allowed: false,
        source: ApprovalSource::Caller,
    }));
}

#[test]
fn the_reader_passes_on_a_line_that_is_not_utf8_as_an_error_and_stops() {
    let (tx, rx) = futures::channel::mpsc::channel(4);
    pump_lines(&b"{\"ok\":1}\n\xff\xfe\n{\"after\":2}\n"[..], tx);
    let lines: Vec<_> = futures::executor::block_on(rx.collect::<Vec<_>>());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].as_ref().unwrap(), "{\"ok\":1}");
    assert_eq!(lines[1].as_ref().unwrap_err().kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn a_read_error_mid_turn_fails_the_session() {
    let root = tempfile::tempdir().unwrap();
    let run = converse_with(&acp_spec(AcpFlavor::Cursor, root.path()), Vec::new(), |agent| {
        agent.open();
        agent.expect("session/prompt");
        let invalid = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the agent sent a line that is not UTF-8",
        );
        agent.to_client.unbounded_send(Err(invalid)).unwrap();
    });
    assert!(run.result.is_err(), "{:?}", run.events);
}

#[test]
fn deferred_requests_are_capped_and_the_end_of_the_turn_withdraws_them() {
    let root = tempfile::tempdir().unwrap();
    let spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), always(ApprovalVerdict::Ask));
    let cap = crate::session::MAX_PENDING_APPROVALS;
    let run = converse_with(&spec, Vec::new(), move |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        for n in 0..=cap {
            agent.send(
                &json!({"jsonrpc": "2.0", "id": 1000 + n, "method": "session/request_permission", "params": {
                    "sessionId": "sess-1",
                    "toolCall": {"toolCallId": format!("t{n}"), "kind": "execute"},
                    "options": standard_options(),
                }}),
            );
        }
        let answer = agent.recv();
        assert_eq!(answer["id"], 1000 + cap, "unexpected frame {answer}");
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "no"})
        );
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
        for _ in 0..cap {
            let answer = agent.recv();
            assert_eq!(
                answer["result"]["outcome"]["optionId"], "no",
                "unexpected frame {answer}"
            );
        }
    });
    run.result.unwrap();
    assert!(run.events.contains(&Event::ApprovalResolved {
        id: format!("acp-{}", cap + 1),
        allowed: false,
        source: ApprovalSource::TooManyPending,
    }));
    let withdrawn = run
        .events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ApprovalResolved {
                    source: ApprovalSource::Withdrawn,
                    ..
                }
            )
        })
        .count();
    assert_eq!(withdrawn, cap);
}

#[test]
fn a_stop_ends_a_session_whose_agent_never_answers_initialize() {
    let root = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let run = converse_with(
        &acp_spec(AcpFlavor::Cursor, root.path()),
        vec![Control::Stop],
        |agent| {
            agent.expect("initialize");
            // No answer: the agent hangs while the session opens, until the client hangs up.
            while futures::executor::block_on(agent.from_client.next()).is_some() {}
        },
    );
    run.result.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// The request OpenCode 2.3.0 sent under [`OPENCODE_ASK`] before writing `probe.txt`.
const LIVE_OPENCODE_PERMISSION: &str =
    include_str!("../tests/fixtures/live/opencode-2.3.0-request-permission-edit.json");

#[test]
fn a_recorded_opencode_edit_request_is_rejected_once_without_a_policy() {
    let root = tempfile::tempdir().unwrap();
    let spec = spec(HarnessKind::Acp(AcpFlavor::OpenCode), root.path());
    let run = converse_with(&spec, Vec::new(), |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        let mut request: Value = serde_json::from_str(LIVE_OPENCODE_PERMISSION).unwrap();
        request["id"] = json!(100);
        request["params"]["sessionId"] = json!("sess-1");
        agent.send(&request);
        assert_eq!(
            agent.permission_outcome(),
            json!({"outcome": "selected", "optionId": "reject"})
        );
        agent.reply(&prompt, &json!({"stopReason": "cancelled"}));
    });
    run.result.unwrap();
    let Event::ApprovalRequest(request) = &run.events[1] else {
        panic!("unexpected event {:?}", run.events[1]);
    };
    assert_eq!(request.kind, crate::event::ApprovalKind::FileChange);
    assert!(request.detail.contains("probe.txt"), "{}", request.detail);
}

#[test]
fn a_mode_switch_and_a_fetch_are_not_plain_tool_calls() {
    let request = |kind: &str| {
        let request: RequestPermissionRequest = serde_json::from_value(json!({
            "sessionId": "sess-1",
            "toolCall": {"toolCallId": "t1", "kind": kind},
            "options": standard_options(),
        }))
        .unwrap();
        approval_request("acp-1".to_owned(), &request).kind
    };
    assert_eq!(request("switch_mode"), crate::event::ApprovalKind::Permissions);
    assert_eq!(request("fetch"), crate::event::ApprovalKind::Fetch);
    assert_eq!(request("read"), crate::event::ApprovalKind::Tool);
}

#[test]
fn a_stop_declines_the_requests_still_pending() {
    let root = tempfile::tempdir().unwrap();
    let (asked_tx, asked) = std::sync::mpsc::channel();
    let policy = move |_: &ApprovalRequest| {
        asked_tx.send(()).unwrap();
        ApprovalVerdict::Ask
    };
    let spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), Some(Arc::new(policy)));
    let run = converse_with(&spec, Vec::new(), move |agent| {
        agent.open();
        agent.expect("session/prompt");
        agent.request_permission(&standard_options());
        asked.recv().unwrap();
        agent.controls.unbounded_send(Control::Stop).unwrap();
        assert_eq!(
            agent.permission_outcome(),
            json!({"outcome": "selected", "optionId": "no"})
        );
        agent.expect("session/cancel");
        while futures::executor::block_on(agent.from_client.next()).is_some() {}
    });
    run.result.unwrap();
    assert!(run.events.contains(&Event::ApprovalResolved {
        id: "acp-1".to_owned(),
        allowed: false,
        source: ApprovalSource::Withdrawn,
    }));
}

#[test]
fn a_stop_between_turns_declines_a_request_sent_after_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let (asked_tx, asked) = std::sync::mpsc::channel();
    let policy = move |_: &ApprovalRequest| {
        asked_tx.send(()).unwrap();
        ApprovalVerdict::Ask
    };
    let mut spec = with_policy(acp_spec(AcpFlavor::Cursor, root.path()), Some(Arc::new(policy)));
    spec.limits.max_turns = 2;
    let run = converse_with(&spec, Vec::new(), move |agent| {
        agent.open();
        let prompt = agent.expect("session/prompt");
        agent.reply(&prompt, &json!({"stopReason": "end_turn"}));
        // Let the client finish the turn first, so the request waits while it is idle.
        std::thread::sleep(std::time::Duration::from_millis(200));
        agent.request_permission(&standard_options());
        asked.recv().unwrap();
        agent.controls.unbounded_send(Control::Stop).unwrap();
        assert_eq!(
            agent.permission_outcome(),
            json!({"outcome": "selected", "optionId": "no"})
        );
        while futures::executor::block_on(agent.from_client.next()).is_some() {}
    });
    run.result.unwrap();
    assert!(run.events.contains(&Event::ApprovalResolved {
        id: "acp-1".to_owned(),
        allowed: false,
        source: ApprovalSource::Withdrawn,
    }));
}

#[test]
fn the_outgoing_queue_holds_exactly_its_limit() {
    let (tx, _rx) = lines_channel::<String>(OUTGOING_LINES);
    let mut sink = FailFast(tx);
    for n in 0..OUTGOING_LINES {
        Pin::new(&mut sink).start_send(n.to_string()).unwrap();
    }
    let error = Pin::new(&mut sink).start_send("one more".to_owned()).unwrap_err();
    assert!(error.to_string().contains("left 16 lines unread"), "{error}");

    let (mut tx, _rx) = lines_channel::<u8>(INCOMING_LINES);
    for _ in 0..INCOMING_LINES {
        tx.try_send(0).unwrap();
    }
    assert!(tx.try_send(0).unwrap_err().is_full());
}

#[path = "acp_mode_tests.rs"]
mod modes;
