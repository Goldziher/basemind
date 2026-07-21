//! M0 gate: the turn-loop drives a full user turn end-to-end against a scripted model — streaming
//! text, running a permission-gated tool, feeding the result back, and stopping — with no network
//! and no code index. This is the deterministic proof the loop works before any TUI exists.

use std::path::PathBuf;
use std::sync::Arc;

use basemind_agent::model::{MockModelClient as Mock, StallingModelClient};
use basemind_agent::permission::{ClaimKind, Rule, RuleAction, RuleSet};
use basemind_agent::tools::ShellTool;
use basemind_agent::{
    AgentCommand, AgentEvent, History, ModelClient, PermissionDecision, PermissionEngine, ResolvedRole, StopReason,
    ToolRegistry, TurnContext, run_turn,
};
use tokio::sync::{broadcast, mpsc};

/// A scripted two-round model: round 1 emits some text then asks to run `shell:exec`; round 2 emits
/// a closing line and stops.
fn scripted_shell_role() -> ResolvedRole {
    let client: Arc<dyn ModelClient> = Arc::new(Mock::new(vec![
        vec![
            Mock::text("Let me list the files. "),
            Mock::tool_call(0, Some("call_1"), Some("shell:exec"), r#"{"command":"echo hi"}"#),
            Mock::finish_tool_calls(),
        ],
        vec![Mock::text("Done."), Mock::finish_stop()],
    ]));
    ResolvedRole {
        client,
        model: "mock/model".into(),
        temperature: None,
        max_tokens: None,
    }
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ShellTool));
    registry
}

/// Drain all currently-buffered events from a receiver (non-blocking).
fn drain(rx: &mut broadcast::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn turn_runs_a_permitted_tool_and_continues_to_stop() {
    let (events, mut recorder) = broadcast::channel(256);
    let (_cmd_tx, mut commands) = mpsc::channel(16);

    let mut history = History::new(Some("you are a coding agent".into()));
    history.push_user("list the files");
    let tools = registry();
    let resolved = scripted_shell_role();

    // Allow exec so the loop runs without a permission round-trip.
    let mut rules = RuleSet::base();
    rules.push(Rule::new(ClaimKind::Exec, "*", RuleAction::Allow).unwrap());
    let permission = PermissionEngine::new(rules);

    let mut cx = TurnContext {
        history: &mut history,
        tools: &tools,
        role: &resolved,
        permission: &permission,
        root: PathBuf::from("."),
        server: None,
        max_steps: 10,
    };

    let reason = run_turn(1, &mut cx, &events, &mut commands).await;
    assert_eq!(reason, StopReason::Stop);

    let seen = drain(&mut recorder);
    let kinds: Vec<&str> = seen.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "TurnStarted",
            "TextDelta",   // "Let me list the files. "
            "ToolStarted", // shell:exec
            "ToolResult",  // echo hi
            "TextDelta",   // "Done."
            "TurnFinished",
        ]
    );

    // The tool ran and its output was fed back into history as a tool message.
    assert!(matches!(seen[3], AgentEvent::ToolResult { ok: true, .. }));
}

#[tokio::test]
async fn turn_suspends_on_permission_then_resumes_on_approval() {
    let (events, mut recorder) = broadcast::channel(256);
    let (cmd_tx, mut commands) = mpsc::channel(16);
    let mut responder = events.subscribe();

    let mut history = History::new(None);
    history.push_user("run echo");
    let tools = registry();
    let resolved = scripted_shell_role();
    // Base ruleset only: exec is Ask, so the loop must suspend for approval.
    let permission = PermissionEngine::with_base();

    let mut cx = TurnContext {
        history: &mut history,
        tools: &tools,
        role: &resolved,
        permission: &permission,
        root: PathBuf::from("."),
        server: None,
        max_steps: 10,
    };

    // Concurrently: run the turn, and answer the permission request when it arrives.
    let run = run_turn(1, &mut cx, &events, &mut commands);
    let respond = async {
        loop {
            if let Ok(AgentEvent::PermissionRequested { req_id, .. }) = responder.recv().await {
                cmd_tx
                    .send(AgentCommand::PermissionDecision {
                        req_id,
                        decision: PermissionDecision::Allow,
                    })
                    .await
                    .unwrap();
                break;
            }
        }
    };
    let (reason, ()) = tokio::join!(run, respond);

    assert_eq!(reason, StopReason::Stop);
    let seen = drain(&mut recorder);
    assert!(
        seen.iter().any(|e| matches!(e, AgentEvent::PermissionRequested { .. })),
        "a permission request should have been emitted"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { ok: true, .. })),
        "the approved tool should have run"
    );
}

#[tokio::test]
async fn turn_denied_permission_feeds_error_back_and_still_stops() {
    let (events, mut recorder) = broadcast::channel(256);
    let (cmd_tx, mut commands) = mpsc::channel(16);
    let mut responder = events.subscribe();

    let mut history = History::new(None);
    history.push_user("run echo");
    let tools = registry();
    let resolved = scripted_shell_role();
    let permission = PermissionEngine::with_base();

    let mut cx = TurnContext {
        history: &mut history,
        tools: &tools,
        role: &resolved,
        permission: &permission,
        root: PathBuf::from("."),
        server: None,
        max_steps: 10,
    };

    let run = run_turn(1, &mut cx, &events, &mut commands);
    let respond = async {
        loop {
            if let Ok(AgentEvent::PermissionRequested { req_id, .. }) = responder.recv().await {
                cmd_tx
                    .send(AgentCommand::PermissionDecision {
                        req_id,
                        decision: PermissionDecision::Deny,
                    })
                    .await
                    .unwrap();
                break;
            }
        }
    };
    let (reason, ()) = tokio::join!(run, respond);

    // Denied tool is fed back as a failed result, but the model still gets its next round and stops.
    assert_eq!(reason, StopReason::Stop);
    let seen = drain(&mut recorder);
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { ok: false, .. }))
    );
}

#[tokio::test]
async fn cancel_during_streaming_ends_the_turn() {
    let (events, mut recorder) = broadcast::channel(256);
    let (cmd_tx, mut commands) = mpsc::channel(16);

    let mut history = History::new(None);
    history.push_user("hello");
    let tools = registry();
    // A provider whose stream stalls forever after one delta — the only way out is a cancel.
    let client: Arc<dyn ModelClient> = Arc::new(StallingModelClient);
    let resolved = ResolvedRole {
        client,
        model: "mock/model".into(),
        temperature: None,
        max_tokens: None,
    };
    let permission = PermissionEngine::with_base();

    let mut cx = TurnContext {
        history: &mut history,
        tools: &tools,
        role: &resolved,
        permission: &permission,
        root: PathBuf::from("."),
        server: None,
        max_steps: 10,
    };

    // Run the (stalling) turn and cancel it concurrently.
    let run = run_turn(1, &mut cx, &events, &mut commands);
    let cancel = async {
        tokio::task::yield_now().await;
        cmd_tx.send(AgentCommand::Cancel).await.unwrap();
    };
    let (reason, ()) = tokio::join!(run, cancel);

    assert_eq!(reason, StopReason::Cancelled);
    let seen = drain(&mut recorder);
    assert!(
        matches!(
            seen.last(),
            Some(AgentEvent::TurnFinished {
                reason: StopReason::Cancelled,
                ..
            })
        ),
        "the turn should finish as cancelled, got {seen:?}"
    );
}

#[tokio::test]
async fn cancel_during_tool_execution_ends_the_turn() {
    let (events, mut recorder) = broadcast::channel(256);
    let (cmd_tx, mut commands) = mpsc::channel(16);
    let mut responder = events.subscribe();

    let mut history = History::new(None);
    history.push_user("run a slow command");
    let tools = registry();
    // The model asks to run a long sleep; we cancel while it is in flight.
    let client: Arc<dyn ModelClient> = Arc::new(Mock::new(vec![vec![
        Mock::tool_call(0, Some("call_1"), Some("shell:exec"), r#"{"command":"sleep 2"}"#),
        Mock::finish_tool_calls(),
    ]]));
    let resolved = ResolvedRole {
        client,
        model: "mock/model".into(),
        temperature: None,
        max_tokens: None,
    };

    // Auto-allow exec so the turn reaches tool execution without a permission round-trip.
    let mut rules = RuleSet::base();
    rules.push(Rule::new(ClaimKind::Exec, "*", RuleAction::Allow).unwrap());
    let permission = PermissionEngine::new(rules);

    let mut cx = TurnContext {
        history: &mut history,
        tools: &tools,
        role: &resolved,
        permission: &permission,
        root: PathBuf::from("."),
        server: None,
        max_steps: 10,
    };

    let run = run_turn(1, &mut cx, &events, &mut commands);
    let cancel = async {
        // Cancel as soon as the tool has actually started.
        loop {
            if let Ok(AgentEvent::ToolStarted { .. }) = responder.recv().await {
                cmd_tx.send(AgentCommand::Cancel).await.unwrap();
                break;
            }
        }
    };
    let (reason, ()) = tokio::join!(run, cancel);

    assert_eq!(reason, StopReason::Cancelled);
    let seen = drain(&mut recorder);
    assert!(
        seen.iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { ok: false, .. })),
        "the cancelled tool should feed back a failed result, got {seen:?}"
    );
    assert!(matches!(
        seen.last(),
        Some(AgentEvent::TurnFinished {
            reason: StopReason::Cancelled,
            ..
        })
    ));
}

fn event_kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::TurnStarted { .. } => "TurnStarted",
        AgentEvent::TextDelta { .. } => "TextDelta",
        AgentEvent::ToolStarted { .. } => "ToolStarted",
        AgentEvent::ToolProgress { .. } => "ToolProgress",
        AgentEvent::ToolResult { .. } => "ToolResult",
        AgentEvent::PermissionRequested { .. } => "PermissionRequested",
        AgentEvent::Usage { .. } => "Usage",
        AgentEvent::Compacted { .. } => "Compacted",
        AgentEvent::TurnFinished { .. } => "TurnFinished",
        AgentEvent::Error { .. } => "Error",
    }
}
