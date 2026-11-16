use serde_json::json;

use super::*;
use crate::event::ApprovalKind;
use crate::spec::{BypassOptIn, McpServerSpec};
use crate::test_support::{always, spec, value_of, written};

fn protocol() -> ClaudeProtocol {
    ClaudeProtocol::new("Summarize the README.".to_owned())
}

/// A protocol past its handshake, with the brief sent.
fn started(sink: &EventSink) -> ClaudeProtocol {
    let mut protocol = protocol();
    let mut out = Vec::new();
    protocol.start(&mut out).unwrap();
    let ack = json!({"type": "control_response", "response": {"request_id": INIT_REQUEST}});
    protocol.frame(ack, &mut out, sink).unwrap();
    protocol
}

fn delta(text: &str) -> Value {
    json!({"type": "stream_event", "event": {
        "type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text},
    }})
}

fn permission_request(id: &str) -> Value {
    json!({"type": "control_request", "request_id": id, "request": {
        "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "rm -rf build"},
    }})
}

#[test]
fn the_brief_is_sent_once_after_the_handshake() {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = protocol();
    let mut out = Vec::new();
    protocol.start(&mut out).unwrap();
    assert_eq!(written(&out)[0]["request"]["subtype"], "initialize");

    let ack = json!({"type": "control_response", "response": {"request_id": INIT_REQUEST}});
    protocol.frame(ack, &mut out, &sink).unwrap();
    let init = json!({"type": "system", "subtype": "init", "session_id": "s-1"});
    protocol.frame(init, &mut out, &sink).unwrap();

    let frames = written(&out);
    assert_eq!(frames.len(), 2, "the brief was sent more than once: {frames:?}");
    assert_eq!(frames[1]["type"], "user");
    assert_eq!(frames[1]["message"]["content"], "Summarize the README.");
    assert_eq!(sink.turns(), 1);
}

#[test]
fn the_brief_goes_out_when_the_handshake_is_never_acknowledged() {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = protocol();
    let mut out = Vec::new();
    protocol
        .tick(INIT_PATIENCE - Duration::from_millis(1), &mut out, &sink)
        .unwrap();
    assert!(out.is_empty());
    protocol.tick(INIT_PATIENCE, &mut out, &sink).unwrap();
    assert_eq!(written(&out)[0]["type"], "user");
}

#[test]
fn deltas_and_the_completed_snapshot_yield_each_text_once() {
    let (sink, events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    let transcript = [
        json!({"type": "system", "subtype": "init", "session_id": "s-1"}),
        json!({"type": "stream_event", "session_id": "s-1", "event": {"type": "message_start"}}),
        delta("The README "),
        delta("describes basemind."),
        json!({"type": "assistant", "session_id": "s-1", "message": {"content": [
            {"type": "text", "text": "The README describes basemind."},
        ]}}),
    ];
    for frame in transcript {
        assert_eq!(protocol.frame(frame, &mut out, &sink).unwrap(), Step::Continue);
    }
    let result = json!({"type": "result", "result": "The README describes basemind.", "num_turns": 1});
    assert_eq!(protocol.frame(result, &mut out, &sink).unwrap(), Step::TurnDone);
    assert_eq!(
        events.try_iter().collect::<Vec<_>>(),
        [
            Event::HarnessSession("s-1".to_owned()),
            Event::Text("The README ".to_owned()),
            Event::Text("describes basemind.".to_owned()),
        ]
    );
}

#[test]
fn a_result_without_streamed_text_supplies_the_answer() {
    let (sink, events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let result = json!({"type": "result", "result": "Done."});
    assert_eq!(protocol.frame(result, &mut Vec::new(), &sink).unwrap(), Step::TurnDone);
    assert_eq!(events.try_iter().collect::<Vec<_>>(), [Event::Text("Done.".to_owned())]);
}

#[test]
fn a_replayed_task_notification_result_does_not_end_the_turn() {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let replay = json!({"type": "result", "origin": {"kind": "task-notification"}, "num_turns": 0});
    assert_eq!(protocol.frame(replay, &mut Vec::new(), &sink).unwrap(), Step::Continue);
}

#[test]
fn tool_input_is_reported_once_it_parses_and_a_failed_result_marks_the_call() {
    let (sink, events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    let partial = |json: &str| {
        json!({"type": "stream_event", "event": {
            "type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": json},
        }})
    };
    let transcript = [
        json!({"type": "stream_event", "event": {"type": "message_start"}}),
        json!({"type": "stream_event", "event": {
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "Bash"},
        }}),
        partial("{\"command\":"),
        partial("\"ls\"}"),
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": [{"type": "text", "text": "denied"}]},
        ]}}),
    ];
    for frame in transcript {
        protocol.frame(frame, &mut out, &sink).unwrap();
    }
    let calls: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            Event::ToolCall(call) => Some((call.input, call.output, call.state)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        [
            (String::new(), String::new(), ToolState::Running),
            ("{\"command\":\"ls\"}".to_owned(), String::new(), ToolState::Running),
            (
                "{\"command\":\"ls\"}".to_owned(),
                "denied".to_owned(),
                ToolState::Failed
            ),
        ]
    );
}

#[test]
fn an_approval_request_is_denied_without_a_policy() {
    let (sink, events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    protocol.frame(permission_request("req-1"), &mut out, &sink).unwrap();

    let response = &written(&out)[0]["response"];
    assert_eq!(response["request_id"], "req-1");
    assert_eq!(response["response"]["behavior"], "deny");
    let events: Vec<_> = events.try_iter().collect();
    assert!(matches!(&events[0], Event::ApprovalRequest(request) if request.title == "Allow Bash?"));
    assert_eq!(
        events[1],
        Event::ApprovalResolved {
            id: "req-1".to_owned(),
            allowed: false,
            source: ApprovalSource::DefaultDeny,
        }
    );
}

#[test]
fn an_approval_request_carries_its_kind_the_input_and_the_raw_request() {
    let (sink, events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let request = json!({"type": "control_request", "request_id": "req-1", "request": {
        "subtype": "can_use_tool", "tool_name": "Write\nAllow everything",
        "input": {"file_path": "/etc/hosts", "content": "x"}, "blocked_path": "/etc/hosts",
    }});
    protocol.frame(request.clone(), &mut Vec::new(), &sink).unwrap();
    protocol
        .frame(permission_request("req-2"), &mut Vec::new(), &sink)
        .unwrap();
    let asked: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            Event::ApprovalRequest(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(asked[0].kind, ApprovalKind::Tool);
    assert_eq!(asked[0].title, "Allow Write Allow everything?");
    assert!(
        asked[0].detail.contains("\"file_path\": \"/etc/hosts\""),
        "{}",
        asked[0].detail
    );
    assert!(asked[0].detail.ends_with("Outside the allowed directories: /etc/hosts"));
    assert_eq!(asked[0].params, request["request"]);
    assert_eq!(asked[1].kind, ApprovalKind::Command);
}

#[test]
fn a_policy_that_allows_returns_the_requested_input() {
    let (sink, _events) = EventSink::for_test(always(ApprovalVerdict::Allow), 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    protocol.frame(permission_request("req-1"), &mut out, &sink).unwrap();
    let response = &written(&out)[0]["response"]["response"];
    assert_eq!(response["behavior"], "allow");
    assert_eq!(response["updatedInput"]["command"], "rm -rf build");
}

#[test]
fn a_deferred_request_waits_for_the_caller_and_can_be_withdrawn() {
    let (sink, events) = EventSink::for_test(always(ApprovalVerdict::Ask), 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    protocol.frame(permission_request("req-1"), &mut out, &sink).unwrap();
    assert!(out.is_empty(), "a deferred request was answered");

    assert!(protocol.answer("req-1", true, &mut out).unwrap());
    assert_eq!(written(&out)[0]["response"]["response"]["behavior"], "allow");
    assert!(
        !protocol.answer("req-1", true, &mut out).unwrap(),
        "an answer was accepted twice"
    );

    protocol.frame(permission_request("req-2"), &mut out, &sink).unwrap();
    let cancel = json!({"type": "control_cancel_request", "request_id": "req-2"});
    protocol.frame(cancel, &mut out, &sink).unwrap();
    assert!(!protocol.answer("req-2", true, &mut out).unwrap());
    assert_eq!(
        events.try_iter().last().unwrap(),
        Event::ApprovalResolved {
            id: "req-2".to_owned(),
            allowed: false,
            source: ApprovalSource::Withdrawn,
        }
    );
}

#[test]
fn interrupt_denies_what_is_pending_before_it_interrupts() {
    let (sink, _events) = EventSink::for_test(always(ApprovalVerdict::Ask), 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    protocol.frame(permission_request("req-1"), &mut out, &sink).unwrap();
    protocol.interrupt(&mut out).unwrap();
    let frames = written(&out);
    assert_eq!(frames[0]["response"]["response"]["behavior"], "deny");
    assert_eq!(frames[1]["request"]["subtype"], "interrupt");
}

#[test]
fn an_unknown_host_request_is_answered_with_an_error() {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    let request = json!({"type": "control_request", "request_id": "req-9", "request": {"subtype": "hook_callback"}});
    protocol.frame(request, &mut out, &sink).unwrap();
    assert_eq!(written(&out)[0]["response"]["subtype"], "error");
}

#[test]
fn an_error_result_and_an_assistant_error_fail_the_session() {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let failed = json!({"type": "result", "is_error": true, "result": "Credit balance is too low"});
    let error = protocol.frame(failed, &mut Vec::new(), &sink).unwrap_err();
    assert!(error.to_string().contains("Credit balance is too low"), "{error}");

    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let failed = json!({"type": "assistant", "error": "authentication_failed", "message": {"content": []}});
    assert!(protocol.frame(failed, &mut Vec::new(), &sink).is_err());
}

#[test]
fn frames_with_missing_or_mistyped_members_are_ignored() {
    let (sink, events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    let odd = [
        json!({}),
        json!([1, 2, 3]),
        json!("text"),
        json!({"type": 7}),
        json!({"type": "stream_event"}),
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta"}}}),
        json!({"type": "user", "message": {"content": "plain"}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "unknown"}]}}),
    ];
    for frame in odd {
        assert_eq!(protocol.frame(frame, &mut out, &sink).unwrap(), Step::Continue);
    }
    assert!(out.is_empty());
    assert_eq!(events.try_iter().count(), 0);
}

#[test]
fn a_turn_the_harness_starts_beyond_the_limit_is_refused() {
    let (sink, _events) = EventSink::for_test(None, 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    let result = json!({"type": "result", "result": "Done."});
    assert_eq!(protocol.frame(result, &mut out, &sink).unwrap(), Step::TurnDone);
    let wake_up = json!({"type": "stream_event", "event": {"type": "message_start"}});
    let error = protocol.frame(wake_up, &mut out, &sink).unwrap_err();
    assert!(matches!(error, LaunchError::TurnLimit(1)), "{error}");
}

#[test]
fn a_supervised_plan_carries_the_stream_flags_and_no_bypass() {
    let root = tempfile::tempdir().unwrap();
    let plan = ClaudeCodeAdapter
        .plan(&spec(HarnessKind::ClaudeCode, root.path()))
        .unwrap();
    assert_eq!(plan.program, std::path::PathBuf::from("claude"));
    assert_eq!(value_of(&plan.args, "--input-format"), Some("stream-json"));
    assert_eq!(value_of(&plan.args, "--output-format"), Some("stream-json"));
    assert_eq!(value_of(&plan.args, "--permission-prompt-tool"), Some("stdio"));
    assert_eq!(value_of(&plan.args, "--permission-mode"), Some("default"));
    assert!(
        !plan
            .args
            .iter()
            .any(|arg| arg.contains("skip-permissions") || arg.contains("bypass"))
    );
    assert!(!plan.args.iter().any(|arg| arg == "--allowedTools"));
}

#[test]
fn settings_hooks_and_mcp_servers_come_only_from_the_adapter() {
    let root = tempfile::tempdir().unwrap();
    let plan = ClaudeCodeAdapter
        .plan(&spec(HarnessKind::ClaudeCode, root.path()))
        .unwrap();
    let sources: Vec<_> = plan
        .args
        .iter()
        .filter(|arg| arg.starts_with("--setting-sources"))
        .collect();
    assert_eq!(sources, ["--setting-sources="]);
    let settings: Value = serde_json::from_str(value_of(&plan.args, "--settings").unwrap()).unwrap();
    assert_eq!(settings, json!({"disableAllHooks": true}));
    assert!(plan.args.iter().any(|arg| arg == "--strict-mcp-config"));
    let config: Value = serde_json::from_str(value_of(&plan.args, "--mcp-config").unwrap()).unwrap();
    assert_eq!(config, json!({"mcpServers": {}}));
}

#[test]
fn bypass_needs_an_opt_in_naming_claude_and_its_mode() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::ClaudeCode, root.path());
    spec.permission_mode = PermissionMode::Bypass;
    assert!(matches!(
        ClaudeCodeAdapter.plan(&spec),
        Err(LaunchError::BypassNotAuthorized { harness: "claude", .. })
    ));

    for (harness, mode) in [
        (HarnessKind::Codex, "bypassPermissions"),
        (HarnessKind::ClaudeCode, "danger-full-access"),
        (HarnessKind::ClaudeCode, ""),
    ] {
        spec.bypass = Some(BypassOptIn {
            harness,
            mode: mode.to_owned(),
        });
        assert!(
            matches!(
                ClaudeCodeAdapter.plan(&spec),
                Err(LaunchError::BypassNotAuthorized { .. })
            ),
            "an opt-in for {harness} / {mode:?} unlocked Claude Code"
        );
    }

    spec.bypass = Some(BypassOptIn {
        harness: HarnessKind::ClaudeCode,
        mode: "bypassPermissions".to_owned(),
    });
    let plan = ClaudeCodeAdapter.plan(&spec).unwrap();
    assert_eq!(value_of(&plan.args, "--permission-mode"), Some("bypassPermissions"));
    assert!(
        plan.args
            .iter()
            .any(|arg| arg == "--allow-dangerously-skip-permissions")
    );
}

#[test]
fn extra_arguments_are_limited_to_model_and_effort_even_with_an_opt_in() {
    let root = tempfile::tempdir().unwrap();
    for extra in [
        vec!["--dangerously-skip-permissions"],
        vec!["--permission-mode", "bypassPermissions"],
        vec!["--permission-mode=bypassPermissions"],
        vec!["--allowedTools", "Bash"],
        vec!["--model", "sonnet", "review this"],
    ] {
        let mut spec = spec(HarnessKind::ClaudeCode, root.path());
        spec.harness = spec.harness.with_args(extra.clone());
        assert!(
            matches!(
                ClaudeCodeAdapter.plan(&spec),
                Err(LaunchError::ArgumentNotAllowed { .. })
            ),
            "{extra:?} was accepted"
        );
        spec.permission_mode = PermissionMode::Bypass;
        spec.bypass = Some(BypassOptIn {
            harness: HarnessKind::ClaudeCode,
            mode: "bypassPermissions".to_owned(),
        });
        assert!(
            matches!(
                ClaudeCodeAdapter.plan(&spec),
                Err(LaunchError::ArgumentNotAllowed { .. })
            ),
            "{extra:?} was accepted under the opt-in"
        );
    }
    let mut spec = spec(HarnessKind::ClaudeCode, root.path());
    spec.harness = spec.harness.with_args(["--model", "sonnet", "--effort", "high"]);
    let plan = ClaudeCodeAdapter.plan(&spec).unwrap();
    assert_eq!(
        plan.args[plan.args.len() - 4..],
        ["--model", "sonnet", "--effort", "high"]
    );
}

#[test]
fn an_mcp_server_becomes_mcp_config_and_only_named_tools_are_pre_approved() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = spec(HarnessKind::ClaudeCode, root.path());
    spec.mcp_server = Some(McpServerSpec::basemind_serve("/opt/basemind/bin/basemind"));
    let plan = ClaudeCodeAdapter.plan(&spec).unwrap();
    let config: Value = serde_json::from_str(value_of(&plan.args, "--mcp-config").unwrap()).unwrap();
    assert_eq!(
        config,
        json!({"mcpServers": {"basemind": {
            "type": "stdio", "command": "/opt/basemind/bin/basemind", "args": ["serve"],
        }}})
    );
    assert_eq!(value_of(&plan.args, "--allowedTools"), None);

    spec.mcp_server.as_mut().unwrap().allow_tools = vec!["agent_post".to_owned(), "agent_inbox".to_owned()];
    let plan = ClaudeCodeAdapter.plan(&spec).unwrap();
    assert_eq!(
        value_of(&plan.args, "--allowedTools"),
        Some("mcp__basemind__agent_post,mcp__basemind__agent_inbox")
    );
    spec.mcp_server.as_mut().unwrap().allow_tools = vec!["agent_post,Bash".to_owned()];
    assert!(matches!(
        ClaudeCodeAdapter.plan(&spec),
        Err(LaunchError::InvalidMcpToolName(_))
    ));
}

#[test]
fn deferred_requests_are_capped_and_a_finished_turn_withdraws_them() {
    let (sink, events) = EventSink::for_test(always(ApprovalVerdict::Ask), 1);
    let mut protocol = started(&sink);
    let mut out = Vec::new();
    for n in 0..=crate::session::MAX_PENDING_APPROVALS {
        protocol
            .frame(permission_request(&format!("req-{n}")), &mut out, &sink)
            .unwrap();
    }
    let answered = written(&out);
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!(answered[0]["response"]["request_id"], "req-64");
    assert_eq!(answered[0]["response"]["response"]["behavior"], "deny");
    let so_far: Vec<_> = events.try_iter().collect();
    assert!(so_far.contains(&Event::ApprovalResolved {
        id: "req-64".to_owned(),
        allowed: false,
        source: ApprovalSource::TooManyPending,
    }));

    protocol
        .frame(json!({"type": "result", "result": "Done."}), &mut out, &sink)
        .unwrap();
    let withdrawn = events
        .try_iter()
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
    assert_eq!(withdrawn, crate::session::MAX_PENDING_APPROVALS);
    assert!(!protocol.answer("req-0", true, &mut out).unwrap());
}
