//! Contract test for the in-process transport: events flow engine→UI, commands flow UI→engine,
//! and the UI sees `None` once the engine drops its event sender.

use basemind_agent::{AgentClient, AgentCommand, AgentEvent, StopReason, in_proc_channel};

#[tokio::test]
async fn in_proc_client_round_trips_events_and_commands() {
    let (mut engine, mut client) = in_proc_channel(8, 16);

    engine.events.send(AgentEvent::TurnStarted { turn: 1 }).unwrap();
    assert_eq!(client.next_event().await, Some(AgentEvent::TurnStarted { turn: 1 }));

    client
        .send_command(AgentCommand::UserMessage { text: "hi".into() })
        .await
        .unwrap();
    assert_eq!(
        engine.commands.recv().await,
        Some(AgentCommand::UserMessage { text: "hi".into() })
    );

    // a later event still arrives ~keep
    engine
        .events
        .send(AgentEvent::TurnFinished {
            turn: 1,
            reason: StopReason::Stop,
            steps: 1,
        })
        .unwrap();
    assert!(matches!(
        client.next_event().await,
        Some(AgentEvent::TurnFinished { .. })
    ));

    // dropping the engine closes the stream ~keep
    drop(engine);
    assert_eq!(client.next_event().await, None);
}

#[tokio::test]
async fn send_command_errors_after_engine_drops() {
    let (engine, client) = in_proc_channel(8, 16);
    drop(engine);
    let err = client
        .send_command(AgentCommand::Cancel)
        .await
        .expect_err("sending to a dropped engine must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
}
