//! PTY end-to-end: the model-facing `room:post` tool. A scripted turn calls `room:post`, which
//! carries a `comms` permission claim (Ask by default), so the permission overlay appears; approving
//! it runs the tool against the scenario's `ScriptedRoom` and the turn stops. No broker, no network.
#![cfg(all(feature = "replay", unix))]

mod common;

use common::PtySession;

/// A scenario whose first turn calls `room:post` (gated), then a closing turn that stops. A room is
/// declared so the tool has somewhere to post.
const SCENARIO: &str = r#"{
    "user": "ping the room",
    "turns": [
        { "tools": [ { "id": "c1", "name": "room:post", "args": { "text": "PEER-PING" } } ] },
        { "text": "Pinged." }
    ],
    "room": { "roster": [ { "id": "alice", "display": "alice" } ] }
}"#;

#[test]
fn room_post_tool_is_permission_gated_then_runs() {
    let mut session = PtySession::spawn(SCENARIO);

    // room:post is a comms claim, so it prompts rather than auto-running. ~keep
    session.expect_screen("permission required");
    session.allow_session();

    // Approval lets the tool run: the post body and its success summary land in the transcript, and
    // the turn stops. ~keep
    session.expect_all(&["PEER-PING", "posted to the room"]);
    session.expect_screen("idle");
}
