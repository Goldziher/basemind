//! Real-daemon smoke for [`CommsRoom`] — NOT hermetic, and deliberately `#[ignore]`d.
//!
//! `CommsClient` is hard-wired to a real per-user socket (it spawns/attaches the singleton comms
//! daemon), so this test needs a live broker and cannot run in CI. Run it by hand with:
//!
//! ```text
//! cargo test -p basemind-agent --features comms --test comms_room_smoke -- --ignored
//! ```
//!
//! A future `CommsClient::from_link` taking an in-process `CommsLink` (a root-crate change, out of
//! scope for this slice) would let two rooms share an in-memory broker and make this hermetic.

#![cfg(feature = "comms")]

use std::time::Duration;

use basemind_agent::room::{CommsRoom, RoomClient};

#[tokio::test]
#[ignore = "requires a live comms daemon; run with --features comms -- --ignored"]
async fn a_posted_message_is_visible_in_a_peers_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let poster = CommsRoom::connect(root).await.expect("connect poster");
    let reader = CommsRoom::connect(root).await.expect("connect reader");

    let body = "hello from the comms-room smoke";
    poster
        .post(Some("sync".to_string()), body.to_string())
        .await
        .expect("post");

    // The broker is eventually-consistent across two independent connections, so poll briefly. ~keep
    let mut found = false;
    for _ in 0..20 {
        let history = reader.history(None).await.expect("history");
        if history.iter().any(|message| message.body == body) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(found, "the reader's history should contain the posted message");
}
