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
