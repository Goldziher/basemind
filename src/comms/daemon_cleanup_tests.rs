use super::*;

#[tokio::test]
async fn agents_cleanup_defaults_can_preview_then_apply_and_status_reports_completion() {
    let (_dir, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = hello(&broker, &tx, "operator").await;
    let request = |apply| CommsRequest::Cleanup {
        apply,
        message_ttl_secs: store::MESSAGE_TTL.as_secs(),
        thread_idle_ttl_secs: store::THREAD_IDLE_TTL.as_secs(),
        thread_retention_ttl_secs: store::THREAD_RETENTION_TTL.as_secs(),
        agent_ttl_secs: store::EPHEMERAL_AGENT_TTL.as_secs(),
        claim_ttl_secs: crate::comms::daemon_handlers::MAX_RETENTION_SECS,
    };
    let preview = broker.handle(request(false), &mut session, &tx).await;
    assert!(matches!(preview, CommsResponse::Cleanup(report) if !report.applied));
    let applied = broker.handle(request(true), &mut session, &tx).await;
    assert!(matches!(applied, CommsResponse::Cleanup(report) if report.applied));
    let status = broker
        .handle(
            CommsRequest::AgentsStatus {
                agent_ttl_secs: store::EPHEMERAL_AGENT_TTL.as_secs(),
            },
            &mut session,
            &tx,
        )
        .await;
    assert!(matches!(status, CommsResponse::AgentsStatus(report) if report.last_maintenance_micros.is_some()));
}

#[tokio::test]
async fn agents_cleanup_rejects_unsafe_retention_policies_at_daemon_boundary() {
    let (_dir, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut session = hello(&broker, &tx, "operator").await;
    for (message_ttl_secs, thread_idle_ttl_secs, thread_retention_ttl_secs, expected) in [
        (0, 60, 60, "message_ttl_secs"),
        (60, 120, 60, "thread_retention_ttl_secs"),
        (
            crate::comms::daemon_handlers::MAX_RETENTION_SECS + 1,
            60,
            60,
            "message_ttl_secs",
        ),
    ] {
        let response = broker
            .handle(
                CommsRequest::Cleanup {
                    apply: false,
                    message_ttl_secs,
                    thread_idle_ttl_secs,
                    thread_retention_ttl_secs,
                    agent_ttl_secs: 60,
                    claim_ttl_secs: 60,
                },
                &mut session,
                &tx,
            )
            .await;
        assert!(matches!(response, CommsResponse::Error { code, message }
                if code == "invalid_retention_policy" && message.contains(expected)));
    }
}
