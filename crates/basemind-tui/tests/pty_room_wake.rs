//! PTY end-to-end: room auto-respond. With `auto_respond` on, an incoming peer message received
//! while the agent is idle starts a turn, and the scripted reply renders — proving the runner's wake
//! path drives a turn from the event broadcast. No broker, no network.
#![cfg(all(feature = "replay", unix))]

mod common;

use common::PtySession;

/// The opening prompt drives turn 1; the 400 ms incoming (after turn 1 is idle) wakes turn 2.
const SCENARIO: &str = r#"{
    "user": "start",
    "turns": [ { "text": "Ready." }, { "text": "WOKE-REPLY" } ],
    "room": {
        "auto_respond": true,
        "roster": [ { "id": "alice", "display": "alice" } ],
        "incoming": [ { "from": "alice", "subject": "ping", "body": "you there?", "after_ms": 400 } ]
    }
}"#;

#[test]
fn an_incoming_message_wakes_a_reply_turn() {
    let session = PtySession::spawn(SCENARIO);

    // Turn 1 (the opening prompt) settles first. ~keep
    session.expect_screen("Ready.");
    // The peer message is surfaced in the transcript... ~keep
    session.expect_screen("you there?");
    // ...and, because auto-respond is on, it wakes a second turn whose scripted reply renders. ~keep
    session.expect_screen("WOKE-REPLY");
}
