//! End-to-end engine test: drive the real [`Session::run`] loop with a scripted [`replay`] scenario
//! over the [`AgentClient`] transport — streaming text, a permission-gated shell tool, and a clean
//! stop — with no network and no code index. This exercises the full runner + tools + transport
//! stack that the mocked unit tests stop short of, deterministically.

use std::path::PathBuf;
use std::sync::Arc;

use basemind_agent::replay::Scenario;
use basemind_agent::tools::ShellTool;
use basemind_agent::{
    AgentClient, AgentCommand, AgentEvent, PermissionDecision, Session, StopReason, ToolRegistry, in_proc_channel,
};

/// Step budget for the scripted turn.
const MAX_STEPS: u32 = 10;

/// A hermetic scenario: one line of text, one permission-gated `echo`, then a closing line + stop.
fn echo_scenario() -> Scenario {
    Scenario::from_json(
        r#"{
            "user": "run echo and finish",
            "turns": [
                { "text": "Running echo. ",
                  "tools": [ { "id": "c1", "name": "shell:exec", "args": { "command": "echo hi-e2e" } } ] },
                { "text": "Done." }
            ]
        }"#,
    )
    .expect("scenario parses")
}

#[tokio::test]
async fn a_scripted_scenario_runs_the_full_session_loop() {
    let scenario = echo_scenario();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ShellTool));

    // No code index (server: None) — this test is hermetic and shell-only. ~keep
    let session = Session::with_provider(
        scenario.provider(),
        PathBuf::from("."),
        None,
        tools,
        scenario.system.clone(),
        MAX_STEPS,
    );

    let (endpoint, mut client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    client
        .send_command(AgentCommand::UserMessage {
            text: scenario.user.clone(),
        })
        .await
        .expect("send prompt");

    // Collect the whole turn, auto-approving the shell permission when it is requested. ~keep
    let mut events = Vec::new();
    while let Some(event) = client.next_event().await {
        if let AgentEvent::PermissionRequested { req_id, .. } = &event {
            client
                .send_command(AgentCommand::PermissionDecision {
                    req_id: *req_id,
                    decision: PermissionDecision::Allow,
                })
                .await
                .expect("approve");
        }
        let finished = matches!(event, AgentEvent::TurnFinished { .. });
        events.push(event);
        if finished {
            break;
        }
    }

    let _ = client.send_command(AgentCommand::Shutdown).await;
    let _ = engine.await;

    // The turn starts and ends cleanly. ~keep
    assert!(
        matches!(events.first(), Some(AgentEvent::TurnStarted { turn: 1 })),
        "first event is TurnStarted, got {:?}",
        events.first()
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::TurnFinished {
                reason: StopReason::Stop,
                ..
            })
        ),
        "last event is a clean Stop, got {:?}",
        events.last()
    );

    // The shell tool was permission-gated and produced its output. ~keep
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionRequested { tool, .. } if tool == "shell:exec")),
        "the exec tool asked for permission"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { ok: true, summary, .. } if summary.contains("hi-e2e"))),
        "the approved echo produced its output"
    );

    // The scripted text on both rounds streamed through. ~keep
    let streamed: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        streamed.contains("Running echo.") && streamed.contains("Done."),
        "both rounds' text streamed, got {streamed:?}"
    );
}
