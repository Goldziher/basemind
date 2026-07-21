//! PTY end-to-end: the multi-agent room. Drives the real `basemind-tui --replay` binary under a
//! pseudo-terminal with a scenario that declares a room, and asserts the three MVP behaviors on the
//! `vt100`-parsed screen: the roster bar publishes its peers, a scripted incoming peer message lands
//! in the transcript on its timer, and a human `/post` is echoed locally. No broker, no network — the
//! `ScriptedRoom` behind the `--replay` path supplies the roster + timed incoming feed.
#![cfg(all(feature = "replay", unix))]

mod common;

use std::time::Duration;

use common::PtySession;

/// A scenario with a trivial opening turn plus a room: two peers and one incoming message delivered
/// after 300 ms.
const SCENARIO: &str = r#"{
    "user": "stand by",
    "turns": [ { "text": "Standing by." } ],
    "room": {
        "roster": [ { "id": "alice", "display": "alice" }, { "id": "bob", "display": "bob" } ],
        "incoming": [ { "from": "alice", "subject": "sync", "body": "ROOM-IN-42", "after_ms": 300 } ]
    }
}"#;

#[test]
fn room_shows_roster_incoming_and_a_human_post() {
    let mut session = PtySession::spawn(SCENARIO);

    // The roster bar publishes both peers as soon as the session starts. ~keep
    session.expect_all(&["alice", "bob"]);
    // The scripted peer message arrives on its timer and lands in the transcript. ~keep
    session.expect_screen("ROOM-IN-42");
    // Let the opening turn finish so the post below is accepted, not held mid-turn. ~keep
    session.expect_screen("Standing by.");
    session.expect_screen("idle");

    // A human /post is echoed locally; the input line clears (the raw command does not linger). ~keep
    session.type_str("/post hello team");
    session.enter();
    session.expect_screen("hello team");
    session.expect_absent("/post hello team", Duration::from_millis(500));
}
