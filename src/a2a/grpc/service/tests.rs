use super::*;

fn make_service() -> BasemindA2aService {
    BasemindA2aService::new(crate::a2a::state::A2aState::default())
}

#[tokio::test]
async fn send_message_creates_task() {
    let svc = make_service();
    let msg = proto::Message {
        message_id: uuid::Uuid::new_v4().to_string(),
        role: proto::Role::User.into(),
        parts: vec![proto::Part {
            content: Some(proto::part::Content::Text("hello".to_owned())),
            ..Default::default()
        }],
        ..Default::default()
    };
    let req = Request::new(proto::SendMessageRequest {
        message: Some(msg),
        ..Default::default()
    });
    let resp = proto::a2a_service_server::A2aService::send_message(&svc, req)
        .await
        .expect("send_message must succeed");
    let inner = resp.into_inner();
    assert!(
        matches!(
            inner.payload,
            Some(proto::send_message_response::Payload::Task(_))
        ),
        "response must contain a task"
    );
}

#[tokio::test]
async fn get_task_not_found() {
    let svc = make_service();
    let req = Request::new(proto::GetTaskRequest {
        id: uuid::Uuid::new_v4().to_string(),
        ..Default::default()
    });
    let result = proto::a2a_service_server::A2aService::get_task(&svc, req).await;
    assert!(result.is_err(), "unknown task must return error");
    assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn list_tasks_returns_empty_initially() {
    let svc = make_service();
    let req = Request::new(proto::ListTasksRequest::default());
    let resp = proto::a2a_service_server::A2aService::list_tasks(&svc, req)
        .await
        .expect("list_tasks must succeed");
    assert!(
        resp.into_inner().tasks.is_empty(),
        "no tasks should exist initially"
    );
}

#[tokio::test]
async fn get_extended_agent_card_returns_card() {
    let svc = make_service();
    let req = Request::new(proto::GetExtendedAgentCardRequest::default());
    let resp = proto::a2a_service_server::A2aService::get_extended_agent_card(&svc, req)
        .await
        .expect("get_extended_agent_card must succeed");
    let card = resp.into_inner();
    assert_eq!(card.name, "basemind");
    assert!(!card.version.is_empty());
}

#[tokio::test]
async fn send_message_without_message_field_returns_error() {
    let svc = make_service();
    let req = Request::new(proto::SendMessageRequest::default());
    let result = proto::a2a_service_server::A2aService::send_message(&svc, req).await;
    assert!(result.is_err(), "missing message must return error");
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

// ── pagination ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_tasks_paginates_with_cursor() {
    let svc = make_service();

    // Create 5 tasks via send_message.
    for i in 0..5 {
        let msg = proto::Message {
            message_id: uuid::Uuid::new_v4().to_string(),
            role: proto::Role::User.into(),
            parts: vec![proto::Part {
                content: Some(proto::part::Content::Text(format!("msg{i}"))),
                ..Default::default()
            }],
            ..Default::default()
        };
        proto::a2a_service_server::A2aService::send_message(
            &svc,
            Request::new(proto::SendMessageRequest {
                message: Some(msg),
                ..Default::default()
            }),
        )
        .await
        .expect("send must succeed");
    }

    // First page: size 2.
    let page1 = proto::a2a_service_server::A2aService::list_tasks(
        &svc,
        Request::new(proto::ListTasksRequest {
            page_size: Some(2),
            ..Default::default()
        }),
    )
    .await
    .expect("list must succeed")
    .into_inner();

    assert_eq!(page1.tasks.len(), 2, "first page must contain 2 items");
    assert_eq!(page1.page_size, 2, "page_size must reflect actual count");
    assert_eq!(page1.total_size, 5, "total_size must reflect total count");
    assert!(
        !page1.next_page_token.is_empty(),
        "next_page_token must be non-empty when more pages exist"
    );

    // Second page using the cursor.
    let page2 = proto::a2a_service_server::A2aService::list_tasks(
        &svc,
        Request::new(proto::ListTasksRequest {
            page_size: Some(2),
            page_token: page1.next_page_token.clone(),
            ..Default::default()
        }),
    )
    .await
    .expect("list page 2 must succeed")
    .into_inner();

    assert_eq!(page2.tasks.len(), 2, "second page must contain 2 items");
    assert!(
        !page2.next_page_token.is_empty(),
        "third page should still exist (1 item left)"
    );
    // Different items than page 1.
    let page1_ids: std::collections::HashSet<_> =
        page1.tasks.iter().map(|t| t.id.clone()).collect();
    for t in &page2.tasks {
        assert!(
            !page1_ids.contains(&t.id),
            "page 2 must not duplicate page 1 items"
        );
    }

    // Third page: 1 remaining item, no further token.
    let page3 = proto::a2a_service_server::A2aService::list_tasks(
        &svc,
        Request::new(proto::ListTasksRequest {
            page_size: Some(2),
            page_token: page2.next_page_token.clone(),
            ..Default::default()
        }),
    )
    .await
    .expect("list page 3 must succeed")
    .into_inner();

    assert_eq!(page3.tasks.len(), 1, "third page must contain 1 item");
    assert!(
        page3.next_page_token.is_empty(),
        "next_page_token must be empty on the last page"
    );
}

// ── push notifications ─────────────────────────────────────────────────────

#[tokio::test]
async fn push_notification_create_and_list_round_trip() {
    let svc = make_service();

    // Create a task first.
    let send_resp = proto::a2a_service_server::A2aService::send_message(
        &svc,
        Request::new(proto::SendMessageRequest {
            message: Some(text_message()),
            ..Default::default()
        }),
    )
    .await
    .expect("send must succeed");
    let task_id = match send_resp.into_inner().payload {
        Some(proto::send_message_response::Payload::Task(t)) => t.id,
        other => panic!("expected Task, got: {other:?}"),
    };

    // Register two webhooks.
    for url in ["https://hook-a.example/", "https://hook-b.example/"] {
        proto::a2a_service_server::A2aService::create_task_push_notification_config(
            &svc,
            Request::new(proto::TaskPushNotificationConfig {
                task_id: task_id.clone(),
                url: url.to_owned(),
                ..Default::default()
            }),
        )
        .await
        .expect("create push config must succeed");
    }

    // List.
    let listed = proto::a2a_service_server::A2aService::list_task_push_notification_configs(
        &svc,
        Request::new(proto::ListTaskPushNotificationConfigsRequest {
            task_id: task_id.clone(),
            ..Default::default()
        }),
    )
    .await
    .expect("list must succeed")
    .into_inner();
    assert_eq!(listed.configs.len(), 2);
}

#[tokio::test]
async fn push_notification_create_rejects_invalid_url() {
    let svc = make_service();

    // Create a task to attach to.
    let send_resp = proto::a2a_service_server::A2aService::send_message(
        &svc,
        Request::new(proto::SendMessageRequest {
            message: Some(text_message()),
            ..Default::default()
        }),
    )
    .await
    .expect("send must succeed");
    let task_id = match send_resp.into_inner().payload {
        Some(proto::send_message_response::Payload::Task(t)) => t.id,
        other => panic!("expected Task, got: {other:?}"),
    };

    let result = proto::a2a_service_server::A2aService::create_task_push_notification_config(
        &svc,
        Request::new(proto::TaskPushNotificationConfig {
            task_id,
            url: "not-a-valid-url".to_owned(),
            ..Default::default()
        }),
    )
    .await;
    let err = result.expect_err("invalid url must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn push_notification_get_unknown_returns_not_found() {
    let svc = make_service();
    let result = proto::a2a_service_server::A2aService::get_task_push_notification_config(
        &svc,
        Request::new(proto::GetTaskPushNotificationConfigRequest {
            task_id: uuid::Uuid::new_v4().to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        }),
    )
    .await;
    let err = result.expect_err("unknown config must return error");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

// ── streaming ──────────────────────────────────────────────────────────────

fn text_message() -> proto::Message {
    proto::Message {
        message_id: uuid::Uuid::new_v4().to_string(),
        role: proto::Role::User.into(),
        parts: vec![proto::Part {
            content: Some(proto::part::Content::Text("hello".to_owned())),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn send_streaming_message_yields_initial_task() {
    use tokio_stream::StreamExt as _;

    let svc = make_service();
    let req = Request::new(proto::SendMessageRequest {
        message: Some(text_message()),
        ..Default::default()
    });
    let resp = proto::a2a_service_server::A2aService::send_streaming_message(&svc, req)
        .await
        .expect("streaming send must start");

    let mut stream = resp.into_inner();
    let first = tokio::time::timeout(std::time::Duration::from_millis(200), stream.next())
        .await
        .expect("initial event must arrive within 200ms")
        .expect("stream must yield at least one item")
        .expect("first item must be Ok");

    assert!(matches!(
        first.payload,
        Some(proto::stream_response::Payload::Task(_))
    ));
}

#[tokio::test]
async fn subscribe_to_unknown_task_returns_not_found() {
    let svc = make_service();
    let req = Request::new(proto::SubscribeToTaskRequest {
        id: uuid::Uuid::new_v4().to_string(),
        ..Default::default()
    });
    let result = proto::a2a_service_server::A2aService::subscribe_to_task(&svc, req).await;
    // Stream response type is not Debug, so use a manual match instead of
    // unwrap_err.
    match result {
        Err(status) => assert_eq!(status.code(), tonic::Code::NotFound),
        Ok(_) => panic!("expected NotFound, got Ok"),
    }
}

#[tokio::test]
async fn subscribe_yields_status_update_when_task_progresses() {
    use tokio_stream::StreamExt as _;

    let svc = make_service();

    // Create a task first.
    let send = proto::a2a_service_server::A2aService::send_message(
        &svc,
        Request::new(proto::SendMessageRequest {
            message: Some(text_message()),
            ..Default::default()
        }),
    )
    .await
    .expect("send_message must succeed");

    let task_id = match send.into_inner().payload {
        Some(proto::send_message_response::Payload::Task(t)) => t.id,
        other => panic!("expected Task payload, got: {other:?}"),
    };

    // Subscribe.
    let resp = proto::a2a_service_server::A2aService::subscribe_to_task(
        &svc,
        Request::new(proto::SubscribeToTaskRequest {
            id: task_id.clone(),
            ..Default::default()
        }),
    )
    .await
    .expect("subscribe must succeed");

    let mut stream = resp.into_inner();

    // Drain the initial task snapshot.
    let _initial = tokio::time::timeout(std::time::Duration::from_millis(200), stream.next())
        .await
        .expect("initial event must arrive")
        .expect("stream must yield")
        .expect("first item must be Ok");

    // Trigger a state change via the facade.
    let facade = svc.facade();
    let parsed: TaskId = task_id.parse().expect("task id must parse");
    facade
        .update_status(
            &parsed,
            crate::a2a::core::task_types::TaskState::Working,
            None,
        )
        .await
        .expect("state update must succeed");

    let next = tokio::time::timeout(std::time::Duration::from_millis(500), stream.next())
        .await
        .expect("status update must arrive within 500ms")
        .expect("stream must yield")
        .expect("event must be Ok");

    match next.payload {
        Some(proto::stream_response::Payload::StatusUpdate(update)) => {
            assert_eq!(update.task_id, task_id);
            assert_eq!(
                update.status.expect("status").state,
                proto::TaskState::Working as i32
            );
        }
        other => panic!("expected StatusUpdate, got: {other:?}"),
    }
}
