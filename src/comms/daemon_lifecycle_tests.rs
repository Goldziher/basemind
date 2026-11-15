//! Lifecycle tests split from `daemon_tests.rs` to keep both source files under the line cap.

use super::*;

#[tokio::test]
async fn stop_refuses_to_disconnect_another_live_link() {
    let (_dir, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let active_relay = broker
        .try_register_relay()
        .await
        .expect("active broker accepts a relay");
    let stop_client = broker.register_link();

    let refused = broker.handle(CommsRequest::Stop, &mut session, &tx).await;
    assert!(
        matches!(refused, CommsResponse::Error { ref code, .. } if code == "daemon_busy"),
        "a stop request must not tear down another agent's live relay: {refused:?}"
    );
    assert_eq!(broker.state().await, LifecycleState::Starting);

    drop(active_relay);
    let accepted = broker.handle(CommsRequest::Stop, &mut session, &tx).await;
    assert_eq!(accepted, CommsResponse::Ok);
    assert_eq!(broker.state().await, LifecycleState::Draining);
    drop(stop_client);
}

#[tokio::test]
async fn stop_refuses_to_disconnect_an_active_http_connection() {
    let (_dir, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let active_http = broker
        .try_begin_http_connection()
        .await
        .expect("active broker accepts an HTTP connection");

    let refused = broker.handle(CommsRequest::Stop, &mut session, &tx).await;
    assert!(
        matches!(refused, CommsResponse::Error { ref code, .. } if code == "daemon_busy"),
        "a stop request must not tear down an active HTTP connection: {refused:?}"
    );
    assert_eq!(broker.state().await, LifecycleState::Starting);

    drop(active_http);
    let accepted = broker.handle(CommsRequest::Stop, &mut session, &tx).await;
    assert_eq!(accepted, CommsResponse::Ok);
    assert_eq!(broker.state().await, LifecycleState::Draining);
}

/// A relay link that has sent its preamble and nothing more is a session being set up. A `Stop`
/// arriving then must be refused, as it is for a session already registered; once the link is gone
/// the same `Stop` is accepted.
#[tokio::test]
async fn stop_is_refused_while_a_relay_session_is_still_being_set_up() {
    use tokio::io::AsyncWriteExt as _;

    let (_dir, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let (mut client, server) = tokio::io::duplex(4096);
    client
        .write_all(&crate::comms::relay::RELAY_MAGIC)
        .await
        .expect("send the relay preamble");
    let handshake = broker.clone().serve_relay_connection(server, broker.register_link());
    tokio::pin!(handshake);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut handshake)
            .await
            .is_err(),
        "the handshake waits for the hello"
    );

    let refused = broker.handle(CommsRequest::Stop, &mut session, &tx).await;
    assert!(
        matches!(refused, CommsResponse::Error { ref code, .. } if code == "daemon_busy"),
        "a stop must not be accepted under a session being set up: {refused:?}"
    );
    assert_ne!(broker.state().await, LifecycleState::Draining);

    drop(client);
    handshake.await;
    assert_eq!(
        broker.handle(CommsRequest::Stop, &mut session, &tx).await,
        CommsResponse::Ok,
        "a handshake that ended without a session no longer holds the daemon"
    );
}

/// The other order of the same race: the daemon accepted a `Stop` first. The session is told the
/// daemon is draining, which its relay retries against the successor.
#[tokio::test]
async fn a_relay_handshake_that_loses_to_a_stop_is_declined_as_draining() {
    let (_dir, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = Session::default();
    let workspace = tempfile::tempdir().expect("workspace");
    crate::store::init_isolated_cache();
    std::fs::write(workspace.path().join("basemind.toml"), "\"$schema\" = \"v1\"\n").expect("mark the workspace root");
    assert_eq!(
        broker.handle(CommsRequest::Stop, &mut session, &tx).await,
        CommsResponse::Ok
    );

    let (mut client, server) = tokio::io::duplex(4096);
    let serving = tokio::spawn(broker.clone().serve_relay_connection(server, broker.register_link()));
    let hello = crate::comms::relay::RelayHello {
        relay_proto_ver: crate::comms::relay::RELAY_PROTO_VER,
        root: workspace.path().to_path_buf(),
        view: crate::store::VIEW_WORKING.to_string(),
        agent: crate::comms::ids::AgentId::parse("claude-code").expect("agent"),
    };
    let welcome = crate::comms::relay::client_handshake(&mut client, &hello)
        .await
        .expect("the daemon answers the handshake");
    assert!(!welcome.accepted);
    assert_eq!(
        welcome.code.as_deref(),
        Some(crate::comms::relay::decline::DAEMON_DRAINING),
        "{welcome:?}"
    );
    serving.await.expect("the relay task ends");
}
