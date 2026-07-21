//! End-to-end round-trip over a real Unix socket: a [`UdsAgentClient`] driving a daemon-hosted,
//! scripted engine through [`serve_connection`] and the [`serve`] accept loop. Proves both
//! directions of the transport — events stream out to the client, and commands (a user message, a
//! permission decision) flow back into the engine — carry the same [`AgentEvent`]/[`AgentCommand`]
//! values a UI sees in-process, and that a persistent session outlives a client disconnect and
//! advances on the next attach.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use basemind_agent::replay::Scenario;
use basemind_agent::tools::ShellTool;
use basemind_agent::{
    AgentClient, AgentCommand, AgentEvent, PermissionDecision, Session, StopReason, ToolRegistry, in_proc_channel,
};
use basemind_agent_ipc::{UdsAgentClient, bind_listener, probe_alive, serve, serve_connection};
use tokio::net::UnixListener;

/// Model-step budget for a scripted turn (no network — cannot run away).
const REPLAY_MAX_STEPS: u32 = 20;
/// How long any single event may take before the test fails rather than hangs.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind a socket in `dir`, then spawn a one-connection daemon that accepts a single client and
/// bridges it to a scripted engine running `scenario_json`. Returns the socket path; the socket file
/// exists before this returns, so a client can connect immediately.
fn spawn_scripted_daemon(dir: &Path, scenario_json: &str) -> PathBuf {
    let scenario = Scenario::from_json(scenario_json).expect("parse scenario");
    let socket_path = dir.join("agent.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind socket");

    tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept connection");
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(ShellTool));
        let session = Session::with_provider(
            scenario.provider(),
            std::env::temp_dir(),
            None,
            tools,
            scenario.system.clone(),
            REPLAY_MAX_STEPS,
        );
        let (endpoint, engine_client) = in_proc_channel(32, 256);
        let engine = tokio::spawn(session.run(endpoint));
        serve_connection(stream, engine_client).await.expect("serve connection");
        let _ = engine.await;
    });

    socket_path
}

/// Bind a socket in `dir` and spawn a *persistent* daemon: one long-lived scripted session behind the
/// [`serve`] accept loop, so the session outlives each connection and a later attach reuses it.
fn spawn_persistent_daemon(dir: &Path, scenario_json: &str) -> PathBuf {
    let scenario = Scenario::from_json(scenario_json).expect("parse scenario");
    let socket_path = dir.join("agent.sock");
    let listener = bind_listener(&socket_path, probe_alive).expect("bind socket");

    tokio::spawn(async move {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(ShellTool));
        let session = Session::with_provider(
            scenario.provider(),
            std::env::temp_dir(),
            None,
            tools,
            scenario.system.clone(),
            REPLAY_MAX_STEPS,
        );
        let (endpoint, template) = in_proc_channel(32, 256);
        tokio::spawn(session.run(endpoint));
        // The template stays alive inside this closure, so the engine's command channel never
        // closes between connections and the session persists across reconnects. ~keep
        serve(listener, move || template.new_client())
            .await
            .expect("serve accept loop");
    });

    socket_path
}

/// Streamed assistant text across all `TextDelta`s in `events`, concatenated.
fn streamed_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Drain events until the turn finishes (or the stream closes / a single event times out).
async fn collect_until_finished(client: &mut UdsAgentClient) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event())
            .await
            .expect("an event arrives within the timeout");
        match event {
            Some(event) => {
                let finished = matches!(event, AgentEvent::TurnFinished { .. });
                events.push(event);
                if finished {
                    return events;
                }
            }
            None => return events,
        }
    }
}

#[tokio::test]
async fn a_text_turn_round_trips_over_the_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = spawn_scripted_daemon(
        dir.path(),
        r#"{ "user": "hi", "turns": [ { "text": "hello over the wire" } ] }"#,
    );

    let mut client = UdsAgentClient::connect(&socket).await.expect("connect");
    client
        .send_command(AgentCommand::UserMessage { text: "hi".into() })
        .await
        .expect("send user message");

    let events = collect_until_finished(&mut client).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnStarted { turn: 1 })),
        "expected a TurnStarted event: {events:?}"
    );
    assert_eq!(
        streamed_text(&events),
        "hello over the wire",
        "streamed text: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::TurnFinished {
                reason: StopReason::Stop,
                ..
            })
        ),
        "expected a clean TurnFinished(Stop): {events:?}"
    );
}

#[tokio::test]
async fn a_permission_decision_flows_from_client_to_engine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = spawn_scripted_daemon(
        dir.path(),
        r#"{
            "user": "run it",
            "turns": [
                { "text": "running", "tools": [ { "id": "c1", "name": "shell:exec", "args": { "command": "echo NOPE" } } ] },
                { "text": "done" }
            ]
        }"#,
    );

    let mut client = UdsAgentClient::connect(&socket).await.expect("connect");
    client
        .send_command(AgentCommand::UserMessage { text: "run it".into() })
        .await
        .expect("send user message");

    // The gated shell:exec suspends the turn; catch the request id off the socket and answer Deny. ~keep
    let req_id = loop {
        let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event())
            .await
            .expect("an event arrives")
            .expect("the stream stays open until the permission request");
        if let AgentEvent::PermissionRequested { req_id, .. } = event {
            break req_id;
        }
    };
    client
        .send_command(AgentCommand::PermissionDecision {
            req_id,
            decision: PermissionDecision::Deny,
        })
        .await
        .expect("send permission decision");

    let events = collect_until_finished(&mut client).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolResult { ok: false, .. })),
        "expected a failed (denied) ToolResult: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished { .. })),
        "expected the turn to finish: {events:?}"
    );
}

#[tokio::test]
async fn a_persistent_session_survives_disconnect_and_advances_on_reattach() {
    let dir = tempfile::tempdir().expect("tempdir");
    // One session, two scripted turns: turn 1 for the first attach, turn 2 for the second. ~keep
    let socket = spawn_persistent_daemon(
        dir.path(),
        r#"{ "user": "hi", "turns": [ { "text": "reply one" }, { "text": "reply two" } ] }"#,
    );

    // First attach drives turn 1, then detaches the way the real UI does on exit: it sends
    // `Shutdown` before disconnecting. That must NOT kill the shared session. ~keep
    let mut first = UdsAgentClient::connect(&socket).await.expect("connect first");
    first
        .send_command(AgentCommand::UserMessage { text: "first".into() })
        .await
        .expect("send first message");
    let events = collect_until_finished(&mut first).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnStarted { turn: 1 })),
        "first attach is turn 1: {events:?}"
    );
    assert_eq!(streamed_text(&events), "reply one", "first attach text: {events:?}");
    first
        .send_command(AgentCommand::Shutdown)
        .await
        .expect("send shutdown on detach");
    drop(first);

    // A fresh attach reuses the same daemon session: it is turn 2 (the counter advanced) and the
    // scripted model has moved on to the second reply — proof the session outlived the disconnect. ~keep
    let mut second = UdsAgentClient::connect(&socket).await.expect("connect second");
    second
        .send_command(AgentCommand::UserMessage { text: "second".into() })
        .await
        .expect("send second message");
    let events = collect_until_finished(&mut second).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnStarted { turn: 2 })),
        "second attach is turn 2 (same session advanced): {events:?}"
    );
    assert_eq!(streamed_text(&events), "reply two", "second attach text: {events:?}");
}
