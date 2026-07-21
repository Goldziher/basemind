//! Room integration tests: drive the real [`Session::run`] loop with a [`ScriptedRoom`] over the
//! [`AgentClient`] transport, with no broker and no network. The first test covers the base seam —
//! the roster is published, a scripted incoming peer message rides the event broadcast, and a
//! `RoomPost` issued while idle reaches the room's post-log. The second covers auto-respond — an
//! incoming message wakes a turn whose scripted reply is emitted.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use basemind_agent::replay::Scenario;
use basemind_agent::room::{ScriptedIncoming, ScriptedRoom};
use basemind_agent::{
    AgentClient, AgentCommand, AgentEvent, RoomMessage, RoomPeer, Session, ToolRegistry, in_proc_channel,
};

/// Step budget for the (unused) scripted turn.
const MAX_STEPS: u32 = 4;

/// How long to wait for an expected event / post before failing.
const DEADLINE: Duration = Duration::from_secs(2);

/// A minimal scenario: a single closing text turn. No turn is driven — the room assertions only
/// need the session running so `spawn_incoming` fires and the command loop accepts a `RoomPost`.
fn idle_scenario() -> Scenario {
    Scenario::from_json(r#"{ "user": "noop", "turns": [ { "text": "ok" } ] }"#).expect("scenario parses")
}

#[tokio::test]
async fn scripted_room_publishes_roster_streams_incoming_and_captures_a_post() {
    let alice = RoomPeer {
        id: "alice".into(),
        display: "alice".into(),
    };
    let bob = RoomPeer {
        id: "bob".into(),
        display: "bob".into(),
    };
    let incoming = vec![ScriptedIncoming {
        message: RoomMessage {
            from: "alice".into(),
            subject: "sync".into(),
            body: "ROOM-IN-42".into(),
        },
        after: Duration::from_millis(20),
    }];
    let room = Arc::new(ScriptedRoom::new(vec![alice.clone(), bob.clone()], incoming));

    let scenario = idle_scenario();
    let session = Session::with_provider(
        scenario.provider(),
        PathBuf::from("."),
        None,
        ToolRegistry::new(),
        scenario.system.clone(),
        MAX_STEPS,
    )
    .with_room(room.clone());

    let (endpoint, mut client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    // Observe both the roster and the scripted incoming message, filtering past unrelated events. ~keep
    let mut saw_roster = false;
    let mut saw_incoming = false;
    while !(saw_roster && saw_incoming) {
        let event = tokio::time::timeout(DEADLINE, client.next_event())
            .await
            .expect("an event arrived within the deadline")
            .expect("the engine is still running");
        match event {
            AgentEvent::RoomRoster { peers } => {
                assert_eq!(
                    peers,
                    vec![alice.clone(), bob.clone()],
                    "roster carries the full peer set"
                );
                saw_roster = true;
            }
            AgentEvent::RoomMessage(message) => {
                assert_eq!(message.from, "alice");
                assert_eq!(message.body, "ROOM-IN-42");
                saw_incoming = true;
            }
            _ => {}
        }
    }

    // A RoomPost issued while idle must reach the room's post-log. ~keep
    client
        .send_command(AgentCommand::RoomPost {
            subject: None,
            text: "hi peers".into(),
        })
        .await
        .expect("send the room post");

    let posted = tokio::time::timeout(DEADLINE, async {
        loop {
            if room
                .posts()
                .iter()
                .any(|(subject, text)| subject.is_none() && text == "hi peers")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        posted.is_ok(),
        "the RoomPost reached the room, posts={:?}",
        room.posts()
    );

    let _ = client.send_command(AgentCommand::Shutdown).await;
    let _ = engine.await;
}

#[tokio::test]
async fn auto_respond_wakes_a_turn_on_an_incoming_message() {
    let incoming = vec![ScriptedIncoming {
        message: RoomMessage {
            from: "alice".into(),
            subject: "ping".into(),
            body: "you there?".into(),
        },
        after: Duration::from_millis(20),
    }];
    let room = ScriptedRoom::new(Vec::new(), incoming);

    // One scripted turn is available for the wake to consume; no UserMessage is ever sent, so the
    // only way a turn starts is the incoming message waking it.
    let scenario =
        Scenario::from_json(r#"{ "user": "n/a", "turns": [ { "text": "AUTO-REPLY" } ] }"#).expect("scenario parses");
    let session = Session::with_provider(
        scenario.provider(),
        PathBuf::from("."),
        None,
        ToolRegistry::new(),
        scenario.system.clone(),
        MAX_STEPS,
    )
    .with_room(Arc::new(room))
    .with_room_auto_respond(true);

    let (endpoint, mut client) = in_proc_channel(32, 256);
    let engine = tokio::spawn(session.run(endpoint));

    // The wake must drive a real turn: observe TurnStarted followed by the scripted reply text. ~keep
    let mut started = false;
    let mut replied = false;
    while !replied {
        let event = tokio::time::timeout(DEADLINE, client.next_event())
            .await
            .expect("an event arrived within the deadline")
            .expect("the engine is still running");
        match event {
            AgentEvent::TurnStarted { .. } => started = true,
            AgentEvent::TextDelta { text, .. } if text.contains("AUTO-REPLY") => replied = true,
            _ => {}
        }
    }
    assert!(started, "a turn was started by the incoming message");

    let _ = client.send_command(AgentCommand::Shutdown).await;
    let _ = engine.await;
}
