//! axum HTTP/SSE handlers for the A2A JSON-RPC 2.0 binding.
//!
//! [`jsonrpc_handler`] is the single POST entrypoint: it deserializes the
//! JSON-RPC envelope, branches streaming vs unary on the method name, and
//! dispatches onto the shared [`TaskFacade`](crate::a2a::core::task_facade::TaskFacade)
//! and push-notification store held by [`A2aState`]. Unary methods return a
//! [`JsonRpcResponse`] as JSON; the two streaming methods (`message/stream`,
//! `tasks/resubscribe`) return an SSE stream of full-task snapshots, mirroring
//! the gRPC `spawn_task_stream` template (the `tx.closed()` leak guard + bus
//! filtering by `task_id`).
//!
//! [`agent_card_handler`] serves the public agent card at its well-known route.

use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use tokio_stream::wrappers::ReceiverStream;

use crate::a2a::core::bus::Event;
use crate::a2a::core::push_notifications::{PushNotificationAuth, PushNotificationId};
use crate::a2a::core::task_types::{ContextId, TaskId};
use crate::a2a::jsonrpc::protocol::{self, JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::a2a::jsonrpc::{convert, dto};
use crate::a2a::state::A2aState;

/// Buffer size for the per-stream SSE channel between the broadcast
/// subscription and the HTTP client. Slow consumers cap pending events at this
/// many before back-pressure pushes them off the broadcast bus instead.
const SSE_CHANNEL_CAPACITY: usize = 64;

// ── Dispatch entrypoint ──────────────────────────────────────────────────────

/// The single JSON-RPC POST entrypoint.
///
/// Branches on `req.method`: streaming methods (`message/stream`,
/// `tasks/resubscribe`) return an SSE [`Response`]; every other method is a
/// unary call that returns a [`JsonRpcResponse`] serialized as JSON. A
/// streaming method whose preflight fails (bad params, task not found) returns
/// the error as a plain JSON response rather than opening an empty SSE stream.
pub(crate) async fn jsonrpc_handler(
    State(state): State<A2aState>,
    body: axum::body::Bytes,
) -> Response {
    // Parse the envelope by hand (rather than via the `Json` extractor) so a
    // malformed body returns a JSON-RPC `parse_error` envelope instead of axum's
    // bare 400 — the binding stays spec-correct on bad input.
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => {
            return Json(JsonRpcResponse::failure(
                serde_json::Value::Null,
                protocol::parse_error(),
            ))
            .into_response();
        }
    };

    // JSON-RPC 2.0 requires the version marker to be exactly "2.0".
    if req.jsonrpc != "2.0" {
        return Json(JsonRpcResponse::failure(
            req.id.clone(),
            protocol::invalid_request(format!(
                "unsupported jsonrpc version: '{}', expected '2.0'",
                req.jsonrpc
            )),
        ))
        .into_response();
    }

    match req.method.as_str() {
        "message/stream" => stream_message(&state, req.params, req.id).await,
        "tasks/resubscribe" => resubscribe(&state, req.params, req.id).await,
        _ => {
            let resp = dispatch_unary(&state, &req.method, req.params, req.id.clone()).await;
            Json(resp).into_response()
        }
    }
}

// ── Unary dispatch ───────────────────────────────────────────────────────────

/// Serialize a DTO into a `serde_json::Value`, mapping the (practically
/// unreachable) serialize failure onto an internal JSON-RPC error.
fn to_value<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, JsonRpcError> {
    serde_json::to_value(value).map_err(|e| protocol::internal(e.to_string()))
}

/// Deserialize JSON-RPC `params` into a param DTO, mapping a deserialize error
/// onto `INVALID_PARAMS`.
fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, JsonRpcError> {
    serde_json::from_value(params).map_err(|e| protocol::invalid_params(e.to_string()))
}

/// Parse a string id field into a [`TaskId`], mapping failure to `INVALID_PARAMS`.
fn parse_task_id(raw: &str) -> Result<TaskId, JsonRpcError> {
    raw.parse()
        .map_err(|_| protocol::invalid_params(format!("invalid task id: {raw}")))
}

/// Parse an optional message `contextId` into a [`ContextId`].
///
/// An absent or empty value yields `None`; a non-empty value must parse,
/// otherwise an `INVALID_PARAMS` error is returned.
fn parse_context_id(raw: Option<&str>) -> Result<Option<ContextId>, JsonRpcError> {
    match raw {
        None | Some("") => Ok(None),
        Some(s) => s
            .parse()
            .map(Some)
            .map_err(|_| protocol::invalid_params(format!("invalid contextId: {s}"))),
    }
}

/// Dispatch a single unary JSON-RPC method to its handler, returning the
/// success or error [`JsonRpcResponse`] echoing `id`.
pub(crate) async fn dispatch_unary(
    state: &A2aState,
    method: &str,
    params: serde_json::Value,
    id: serde_json::Value,
) -> JsonRpcResponse {
    let result: Result<serde_json::Value, JsonRpcError> = match method {
        "message/send" => message_send(state, params).await,
        "tasks/get" => tasks_get(state, params).await,
        "tasks/cancel" => tasks_cancel(state, params).await,
        "tasks/pushNotificationConfig/set" => push_config_set(state, params).await,
        "tasks/pushNotificationConfig/get" => push_config_get(state, params).await,
        "tasks/pushNotificationConfig/list" => push_config_list(state, params).await,
        "tasks/pushNotificationConfig/delete" => push_config_delete(state, params).await,
        "agent/getAuthenticatedExtendedCard" => to_value(&convert::core_card_to_dto(&state.card)),
        other => Err(protocol::method_not_found(other)),
    };

    match result {
        Ok(value) => JsonRpcResponse::success(id, value),
        Err(err) => JsonRpcResponse::failure(id, err),
    }
}

/// `message/send`: convert + submit a task, returning the resulting task DTO.
async fn message_send(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let task = submit_from_params(state, params).await?;
    to_value(&convert::core_task_to_dto(&task))
}

/// Shared `message/send` + `message/stream` body: parse params, convert the
/// message to core, extract the optional context id, and submit the task.
async fn submit_from_params(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<crate::a2a::core::task_types::Task, JsonRpcError> {
    let p: dto::MessageSendParams = parse_params(params)?;
    let core_msg = convert::dto_message_to_core(&p.message)
        .map_err(|e| protocol::convert_error_to_jsonrpc(&e))?;
    let context_id = parse_context_id(p.message.context_id.as_deref())?;

    state
        .task_facade
        .submit_task(core_msg, context_id, None, p.metadata)
        .await
        .map_err(|e| protocol::facade_error_to_jsonrpc(&e))
}

/// `tasks/get`: fetch a task by id.
async fn tasks_get(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let p: dto::TaskQueryParams = parse_params(params)?;
    let tid = parse_task_id(&p.id)?;
    let task = state
        .task_facade
        .get_task(&tid)
        .await
        .map_err(|e| protocol::facade_error_to_jsonrpc(&e))?;
    to_value(&convert::core_task_to_dto(&task))
}

/// `tasks/cancel`: cancel a task by id.
async fn tasks_cancel(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let p: dto::TaskIdParams = parse_params(params)?;
    let tid = parse_task_id(&p.id)?;
    let task = state
        .task_facade
        .cancel_task(&tid, None)
        .await
        .map_err(|e| protocol::facade_error_to_jsonrpc(&e))?;
    to_value(&convert::core_task_to_dto(&task))
}

/// `tasks/pushNotificationConfig/set`: register a webhook for a task.
async fn push_config_set(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let p: dto::TaskPushConfigParams = parse_params(params)?;
    let tid = parse_task_id(&p.task_id)?;
    let cfg = p.push_notification_config;
    let auth = cfg.authentication.map(|a| PushNotificationAuth {
        scheme: a.scheme,
        credentials: a.credentials,
    });

    let created = state
        .push_notifications
        .write()
        .await
        .create(tid, cfg.url, cfg.token, auth)
        .map_err(|e| protocol::push_error_to_jsonrpc(&e))?;

    to_value(&convert::core_push_config_to_dto(&created))
}

/// `tasks/pushNotificationConfig/get`: fetch one config for a task.
///
/// With no `pushNotificationConfigId` the first config registered for the task
/// is returned; otherwise the matching config is looked up by id.
async fn push_config_get(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let p: dto::GetTaskPushConfigParams = parse_params(params)?;
    let tid = parse_task_id(&p.id)?;
    let cfg_id = match p.push_notification_config_id.as_deref() {
        None => None,
        Some(raw) => Some(raw.parse::<PushNotificationId>().map_err(|_| {
            protocol::invalid_params(format!("invalid push notification config id: {raw}"))
        })?),
    };

    let store = state.push_notifications.read().await;
    let config = match cfg_id {
        // No id supplied: return the first config registered for the task.
        None => store.list(&tid).first().cloned(),
        Some(id) => store.get(&tid, &id).cloned(),
    };

    let config = config.ok_or_else(|| {
        // `TASK_NOT_FOUND` is the closest A2A code for a missing config.
        JsonRpcError::new(
            protocol::TASK_NOT_FOUND,
            "push notification config not found",
        )
    })?;

    to_value(&convert::core_push_config_to_dto(&config))
}

/// `tasks/pushNotificationConfig/list`: list all configs for a task.
async fn push_config_list(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let p: dto::ListTaskPushConfigParams = parse_params(params)?;
    let tid = parse_task_id(&p.id)?;

    let store = state.push_notifications.read().await;
    let dtos: Vec<dto::TaskPushNotificationConfigDto> = store
        .list(&tid)
        .iter()
        .map(convert::core_push_config_to_dto)
        .collect();
    to_value(&dtos)
}

/// `tasks/pushNotificationConfig/delete`: remove a config by id.
async fn push_config_delete(
    state: &A2aState,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let p: dto::DeleteTaskPushConfigParams = parse_params(params)?;
    let tid = parse_task_id(&p.id)?;
    let cfg_id = p
        .push_notification_config_id
        .parse::<PushNotificationId>()
        .map_err(|_| {
            protocol::invalid_params(format!(
                "invalid push notification config id: {}",
                p.push_notification_config_id
            ))
        })?;

    let removed = state.push_notifications.write().await.delete(&tid, &cfg_id);
    if removed {
        Ok(serde_json::Value::Null)
    } else {
        Err(JsonRpcError::new(
            protocol::TASK_NOT_FOUND,
            "push notification config not found",
        ))
    }
}

// ── SSE streaming ────────────────────────────────────────────────────────────

/// `message/stream`: convert + submit a task (same as `message/send`), then
/// open an SSE stream of full-task snapshots. Preflight failures return a plain
/// JSON error response.
async fn stream_message(
    state: &A2aState,
    params: serde_json::Value,
    id: serde_json::Value,
) -> Response {
    match submit_from_params(state, params).await {
        Ok(task) => task_stream_response(state, task, id),
        Err(err) => Json(JsonRpcResponse::failure(id, err)).into_response(),
    }
}

/// `tasks/resubscribe`: re-open an SSE stream for an existing task. Preflight
/// failures return a plain JSON error response.
async fn resubscribe(
    state: &A2aState,
    params: serde_json::Value,
    id: serde_json::Value,
) -> Response {
    let preflight = async {
        let p: dto::TaskIdParams = parse_params(params)?;
        let tid = parse_task_id(&p.id)?;
        state
            .task_facade
            .get_task(&tid)
            .await
            .map_err(|e| protocol::facade_error_to_jsonrpc(&e))
    }
    .await;

    match preflight {
        Ok(task) => task_stream_response(state, task, id),
        Err(err) => Json(JsonRpcResponse::failure(id, err)).into_response(),
    }
}

/// Build an SSE [`Response`] that emits a full-task snapshot now and on every
/// subsequent lifecycle event for `task`, mirroring the gRPC `spawn_task_stream`
/// template (the `tx.closed()` leak guard + bus filtering by `task_id`).
///
/// B4: each event re-emits a full `Task` snapshot (the A2A streaming union
/// accepts `Task`). Granular `TaskStatusUpdateEvent` / `TaskArtifactUpdateEvent`
/// DTOs are a B4 refinement.
fn task_stream_response(
    state: &A2aState,
    task: crate::a2a::core::task_types::Task,
    rpc_id: serde_json::Value,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(SSE_CHANNEL_CAPACITY);
    let mut bus_rx = state.bus.subscribe();
    let task_id = task.id;
    let context_id = task.context_id;

    // Build a JSON-RPC-success SSE event from a task snapshot. Returns `None`
    // (logging) on the practically-unreachable serialize failure rather than
    // unwrapping inside the spawned task.
    let snapshot_event = move |task: &crate::a2a::core::task_types::Task,
                               rpc_id: &serde_json::Value|
          -> Option<SseEvent> {
        let dto_value = match serde_json::to_value(convert::core_task_to_dto(task)) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(%error, "failed to serialize task DTO for SSE; skipping event");
                return None;
            }
        };
        let envelope = JsonRpcResponse::success(rpc_id.clone(), dto_value);
        match serde_json::to_string(&envelope) {
            Ok(json) => Some(SseEvent::default().event("message").data(json)),
            Err(error) => {
                tracing::error!(%error, "failed to serialize SSE envelope; skipping event");
                None
            }
        }
    };

    tokio::spawn(async move {
        // Emit the initial snapshot so the client never misses the first state.
        if let Some(event) = snapshot_event(&task, &rpc_id)
            && tx.send(event).await.is_err()
        {
            return;
        }

        loop {
            // Wake on either a bus event or the client dropping its end of the
            // channel. Without the `tx.closed()` arm a quiet task would block on
            // `recv()` forever after the client disconnected, leaking the task.
            let event = tokio::select! {
                biased;
                _ = tx.closed() => break,
                recv = bus_rx.recv() => match recv {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // The subscriber fell behind and the broadcast buffer
                        // overwrote events it never saw. Don't silently swallow
                        // the gap: surface it so the client knows the snapshot
                        // sequence is no longer contiguous and can resubscribe.
                        tracing::warn!(
                            task_id = %task_id,
                            skipped = n,
                            "SSE subscriber lagged; events were dropped — client should resubscribe"
                        );
                        if tx
                            .send(
                                SseEvent::default()
                                    .event("lagged")
                                    .data(n.to_string()),
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };

            // Only react to lifecycle events for this task. The bus event now
            // carries the post-mutation `Arc<Task>` snapshot directly, so emit
            // it without re-fetching under lock or re-serializing per
            // subscriber.
            let snapshot = match &event {
                Event::TaskStatusChanged {
                    task_id: tid, task, ..
                } if *tid == task_id => task.as_ref(),
                _ => continue,
            };

            if let Some(sse) = snapshot_event(snapshot, &rpc_id)
                && tx.send(sse).await.is_err()
            {
                break;
            }
        }
    });

    // `context_id` is bound to keep the streaming contract symmetric with the
    // gRPC template; the JSON-RPC snapshot already carries it inside the task.
    let _ = context_id;

    use tokio_stream::StreamExt as _;
    let stream = ReceiverStream::new(rx).map(Ok::<SseEvent, std::convert::Infallible>);
    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

// ── Agent card route ─────────────────────────────────────────────────────────

/// Serve the public agent card DTO (the JSON-RPC-preferred descriptor).
pub(crate) async fn agent_card_handler(State(state): State<A2aState>) -> Json<dto::AgentCardDto> {
    Json(convert::core_card_to_dto(&state.card))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_message_params() -> serde_json::Value {
        json!({
            "message": {
                "messageId": "",
                "role": "user",
                "parts": [{"kind": "text", "text": "do something"}]
            }
        })
    }

    #[tokio::test]
    async fn message_send_returns_task_result() {
        let state = A2aState::default();
        let resp = dispatch_unary(&state, "message/send", text_message_params(), json!(1)).await;

        assert!(
            resp.error.is_none(),
            "message/send must not error: {resp:?}"
        );
        let result = resp.result.expect("message/send must carry a result");
        assert_eq!(result["kind"], json!("task"));
        assert!(
            result["id"].as_str().is_some_and(|s| !s.is_empty()),
            "task result must carry a non-empty id"
        );
    }

    #[tokio::test]
    async fn tasks_get_unknown_id_maps_to_task_not_found() {
        let state = A2aState::default();
        let params = json!({"id": uuid::Uuid::new_v4().to_string()});
        let resp = dispatch_unary(&state, "tasks/get", params, json!(2)).await;

        let error = resp.error.expect("get of unknown task must error");
        assert_eq!(error.code, protocol::TASK_NOT_FOUND);
        assert_eq!(error.code, -32001);
    }

    #[tokio::test]
    async fn extended_card_reports_basemind_jsonrpc() {
        let state = A2aState::default();
        let resp = dispatch_unary(
            &state,
            "agent/getAuthenticatedExtendedCard",
            json!(null),
            json!(3),
        )
        .await;

        let result = resp.result.expect("card method must carry a result");
        assert_eq!(result["name"], json!("basemind"));
        assert_eq!(result["preferredTransport"], json!("JSONRPC"));
    }

    #[tokio::test]
    async fn unknown_method_maps_to_method_not_found() {
        let state = A2aState::default();
        let resp = dispatch_unary(&state, "foo/bar", json!(null), json!(4)).await;

        let error = resp.error.expect("unknown method must error");
        assert_eq!(error.code, protocol::METHOD_NOT_FOUND);
        assert_eq!(error.code, -32601);
    }

    #[tokio::test]
    async fn push_config_set_then_list_round_trips() {
        let state = A2aState::default();

        // Create a task via message/send and extract its id.
        let send = dispatch_unary(&state, "message/send", text_message_params(), json!(1)).await;
        let task_id = send
            .result
            .expect("send must succeed")
            .get("id")
            .and_then(|v| v.as_str())
            .expect("task result must carry an id")
            .to_owned();

        // Register a webhook config for that task.
        let set_params = json!({
            "taskId": task_id,
            "pushNotificationConfig": {
                "url": "https://hook.example/webhook",
                "token": "tok"
            }
        });
        let set = dispatch_unary(
            &state,
            "tasks/pushNotificationConfig/set",
            set_params,
            json!(2),
        )
        .await;
        assert!(set.error.is_none(), "set must not error: {set:?}");
        let set_result = set.result.expect("set must carry a result");
        assert_eq!(set_result["taskId"], json!(task_id));

        // List configs for the task — must return exactly the one we created.
        let list_params = json!({"id": task_id});
        let list = dispatch_unary(
            &state,
            "tasks/pushNotificationConfig/list",
            list_params,
            json!(3),
        )
        .await;
        assert!(list.error.is_none(), "list must not error: {list:?}");
        let configs = list.result.expect("list must carry a result");
        let arr = configs.as_array().expect("list result must be an array");
        assert_eq!(arr.len(), 1, "exactly one config must be listed");
        assert_eq!(arr[0]["taskId"], json!(task_id));
    }
}
