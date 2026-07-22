//! PTY end-to-end: the multi-agent room UX commands. Drives the real `basemind-tui --replay` binary
//! under a pseudo-terminal with a scenario that declares a room (a roster plus a timed presence
//! delta), and asserts the room UX on the `vt100`-parsed screen: a `/post subject: body` echoes a
//! room line carrying the subject, `/roster` dumps the peers as a notice, and a scripted peer join
//! renders a "joined the room" notice. No broker, no network — the `ScriptedRoom` behind the
//! `--replay` path supplies the roster, incoming feed, and presence timeline.
#![cfg(all(feature = "replay", unix))]

mod common;

use std::time::Duration;

use common::PtySession;

/// A scenario with a trivial opening turn plus a room: one peer at start and a scripted join
/// (`carol`) delivered after 500 ms on the presence timeline.
const SCENARIO: &str = r#"{
    "user": "stand by",
    "turns": [ { "text": "Standing by." } ],
    "room": {
        "roster": [ { "id": "alice", "display": "alice" } ],
        "presence": [ { "joined": { "id": "carol", "display": "carol" }, "after_ms": 500 } ]
    }
}"#;

#[test]
fn room_ux_post_subject_roster_and_join_notice() {
    let mut session = PtySession::spawn(SCENARIO);

    // The roster bar publishes the starting peer, and the opening turn goes idle so a post lands. ~keep
    session.expect_screen("alice");
    session.expect_screen("Standing by.");
    session.expect_screen("idle");

    // (a) `/post subject: body` echoes a room line carrying the parsed subject and body. ~keep
    session.type_str("/post ping: hello world");
    session.enter();
    session.expect_all(&["ping", "hello world"]);
    // The raw command does not linger in the input box after submit. ~keep
    session.expect_absent("/post ping", Duration::from_millis(500));

    // (b) `/roster` dumps the current peers as a notice. ~keep
    session.type_str("/roster");
    session.enter();
    session.expect_all(&["room peers", "alice"]);

    // (c) the scripted presence delta renders a "joined the room" notice. ~keep
    session.expect_screen("carol joined the room");
}
