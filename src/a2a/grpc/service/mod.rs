//! A2A service implementation backed by basemind's core task domain.
//!
//! Implements the official A2A protocol (`lf.a2a.v1.A2AService`) with
//! basemind's task facade, message bus, and push-notification store as the
//! backing stores.

use std::pin::Pin;
use std::sync::Arc;

use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::a2a::core::push_notifications::{
    PushNotificationAuth, PushNotificationConfig, PushNotificationId,
};
use crate::a2a::core::task_facade::{FacadeError, TaskFacade};
use crate::a2a::core::task_manager::TaskError;
use crate::a2a::core::task_types::{ContextId, TaskFilter, TaskId};
use crate::a2a::grpc::convert;
use crate::a2a::state::A2aState;
use crate::a2a::v1 as proto;

/// Buffer size for the per-stream channel between the broadcast subscription
/// and the gRPC client. Slow consumers cap memory at this many pending events
/// before back-pressure pushes them off the broadcast bus instead.
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// basemind's implementation of the A2A protocol service.
#[derive(Clone)]
pub(crate) struct BasemindA2aService {
    state: A2aState,
}

impl BasemindA2aService {
    /// Create a new service backed by the given shared state.
    pub fn new(state: A2aState) -> Self {
        Self { state }
    }

    /// Borrow the shared task facade owned by [`A2aState`]. Returns a cheap
    /// `Arc` clone — every transport adapter shares the same facade
    /// instance so we don't allocate a fresh router per request.
    fn facade(&self) -> Arc<TaskFacade> {
        Arc::clone(&self.state.task_facade)
    }

    /// Spawn a background task that subscribes to the message bus, filters
    /// events for `task_id`, converts them to [`proto::StreamResponse`], and
    /// forwards them on a freshly-created mpsc channel returned as a
    /// [`ReceiverStream`].
    ///
    /// If `initial_task` is `Some`, the stream begins by yielding the full
    /// task snapshot so the client never misses the initial state. Subsequent
    /// status/artifact events carry their own post-mutation `Arc<Task>`
    /// snapshot, so the loop reads the artifact body straight off the event
    /// rather than re-fetching the task through the facade under lock.
    fn spawn_task_stream(
        &self,
        task_id: TaskId,
        context_id: ContextId,
        initial_task: Option<crate::a2a::core::task_types::Task>,
    ) -> ReceiverStream<Result<proto::StreamResponse, Status>> {
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);
        let mut bus_rx = self.state.bus.subscribe();

        tokio::spawn(async move {
            // Emit initial task snapshot so the client never misses state.
            if let Some(task) = initial_task.as_ref() {
                let envelope = proto::StreamResponse {
                    payload: Some(proto::stream_response::Payload::Task(
                        convert::core_task_to_proto(task),
                    )),
                };
                if tx.send(Ok(envelope)).await.is_err() {
                    return;
                }
            }

            // The initial snapshot has been emitted; subsequent events carry
            // their own post-mutation task snapshot via `Arc<Task>`.
            drop(initial_task);
            loop {
                // Wake on either a bus event or the client dropping its end of
                // the channel. Without the `tx.closed()` arm a *quiet* task
                // (no further events after subscription) would block on
                // `recv()` forever even after the client disconnected, leaking
                // the spawned task; `closed()` resolves the moment the
                // `ReceiverStream` is dropped.
                let event = tokio::select! {
                    biased;
                    _ = tx.closed() => break,
                    recv = bus_rx.recv() => match recv {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                task_id = %task_id,
                                skipped = n,
                                "stream subscriber lagged; events were dropped — client should resubscribe"
                            );
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                };

                if let Some(envelope) =
                    convert::task_event_to_stream_response(&event, &task_id, &context_id)
                    && tx.send(Ok(envelope)).await.is_err()
                {
                    break;
                }
            }
        });

        ReceiverStream::new(rx)
    }
}

type StreamResult<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl proto::a2a_service_server::A2aService for BasemindA2aService {
    async fn send_message(
        &self,
        request: Request<proto::SendMessageRequest>,
    ) -> Result<Response<proto::SendMessageResponse>, Status> {
        let req = request.into_inner();
        let proto_msg = req
            .message
            .ok_or_else(|| Status::invalid_argument("message is required"))?;

        let core_msg = convert::proto_message_to_core(&proto_msg)?;

        // Extract context_id if provided on the message.
        let context_id = if proto_msg.context_id.is_empty() {
            None
        } else {
            Some(
                proto_msg
                    .context_id
                    .parse()
                    .map_err(|_| Status::invalid_argument("invalid context_id"))?,
            )
        };

        let facade = self.facade();
        let task = facade
            .submit_task(core_msg, context_id, None, None)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let proto_task = convert::core_task_to_proto(&task);
        Ok(Response::new(proto::SendMessageResponse {
            payload: Some(proto::send_message_response::Payload::Task(proto_task)),
        }))
    }

    type SendStreamingMessageStream = StreamResult<proto::StreamResponse>;

    async fn send_streaming_message(
        &self,
        request: Request<proto::SendMessageRequest>,
    ) -> Result<Response<Self::SendStreamingMessageStream>, Status> {
        let req = request.into_inner();
        let proto_msg = req
            .message
            .ok_or_else(|| Status::invalid_argument("message is required"))?;

        let core_msg = convert::proto_message_to_core(&proto_msg)?;

        let context_id = if proto_msg.context_id.is_empty() {
            None
        } else {
            Some(
                proto_msg
                    .context_id
                    .parse()
                    .map_err(|_| Status::invalid_argument("invalid context_id"))?,
            )
        };

        let facade = self.facade();
        let task = facade
            .submit_task(core_msg, context_id, None, None)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let stream = self.spawn_task_stream(task.id, task.context_id, Some(task));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_task(
        &self,
        request: Request<proto::GetTaskRequest>,
    ) -> Result<Response<proto::Task>, Status> {
        let req = request.into_inner();
        let task_id = req
            .id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;

        let facade = self.facade();
        let task = facade
            .get_task(&task_id)
            .await
            .map_err(|e| get_task_error_to_status(&e))?;

        Ok(Response::new(convert::core_task_to_proto(&task)))
    }

    async fn list_tasks(
        &self,
        request: Request<proto::ListTasksRequest>,
    ) -> Result<Response<proto::ListTasksResponse>, Status> {
        let req = request.into_inner();

        let context_id = if req.context_id.is_empty() {
            None
        } else {
            Some(
                req.context_id
                    .parse()
                    .map_err(|_| Status::invalid_argument("invalid context_id"))?,
            )
        };

        let state_filter = if req.status == 0 {
            None
        } else {
            Some(convert::proto_state_to_core(req.status)?)
        };

        let filter = TaskFilter {
            context_id,
            state: state_filter,
            assignee: None,
        };

        // Cursor pagination: tasks are returned in stable id order; the
        // page_token is the id of the last item from the previous page.
        // Clients should treat it as opaque and round-trip it unchanged.
        let page_size = req.page_size.unwrap_or(50).clamp(1, 100) as usize;

        let facade = self.facade();
        let mut tasks = facade.list_tasks(&filter).await;
        tasks.sort_by_key(|t| t.id);

        let total_size = i32::try_from(tasks.len()).unwrap_or_else(|_| {
            tracing::warn!(
                count = tasks.len(),
                "task count exceeds i32::MAX; reporting i32::MAX"
            );
            i32::MAX
        });

        let start_idx = if req.page_token.is_empty() {
            0
        } else {
            tasks
                .iter()
                .position(|t| t.id.to_string() > req.page_token)
                .unwrap_or(tasks.len())
        };
        let end_idx = start_idx.saturating_add(page_size).min(tasks.len());
        let page_slice = &tasks[start_idx..end_idx];

        let next_page_token = if end_idx < tasks.len() {
            page_slice
                .last()
                .map(|t| t.id.to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let proto_tasks: Vec<proto::Task> =
            page_slice.iter().map(convert::core_task_to_proto).collect();
        let returned_page_size = i32::try_from(proto_tasks.len()).unwrap_or(i32::MAX);

        Ok(Response::new(proto::ListTasksResponse {
            tasks: proto_tasks,
            next_page_token,
            page_size: returned_page_size,
            total_size,
        }))
    }

    async fn cancel_task(
        &self,
        request: Request<proto::CancelTaskRequest>,
    ) -> Result<Response<proto::Task>, Status> {
        let req = request.into_inner();
        let task_id = req
            .id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;

        let facade = self.facade();
        let task = facade
            .cancel_task(&task_id, None)
            .await
            .map_err(|e| match e {
                FacadeError::Task(TaskError::TaskNotFound { .. }) => {
                    Status::not_found(e.to_string())
                }
                // An illegal transition (e.g. cancelling a task that is already
                // terminal or in a non-cancelable state) is client-induced, so
                // it maps to FAILED_PRECONDITION rather than INTERNAL.
                FacadeError::Task(
                    TaskError::TaskAlreadyTerminal { .. } | TaskError::TaskInvalidTransition { .. },
                ) => Status::failed_precondition(e.to_string()),
            })?;

        Ok(Response::new(convert::core_task_to_proto(&task)))
    }

    type SubscribeToTaskStream = StreamResult<proto::StreamResponse>;

    async fn subscribe_to_task(
        &self,
        request: Request<proto::SubscribeToTaskRequest>,
    ) -> Result<Response<Self::SubscribeToTaskStream>, Status> {
        let req = request.into_inner();
        let task_id: TaskId = req
            .id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;

        let facade = self.facade();
        let task = facade
            .get_task(&task_id)
            .await
            .map_err(|e| get_task_error_to_status(&e))?;

        let stream = self.spawn_task_stream(task.id, task.context_id, Some(task));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn create_task_push_notification_config(
        &self,
        request: Request<proto::TaskPushNotificationConfig>,
    ) -> Result<Response<proto::TaskPushNotificationConfig>, Status> {
        let req = request.into_inner();
        let task_id: TaskId = req
            .task_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;

        // Verify the task exists before registering a webhook against it.
        let facade = self.facade();
        facade
            .get_task(&task_id)
            .await
            .map_err(|e| get_task_error_to_status(&e))?;

        let auth = req.authentication.as_ref().map(|a| PushNotificationAuth {
            scheme: a.scheme.clone(),
            credentials: a.credentials.clone(),
        });

        let mut store = self.state.push_notifications.write().await;
        let cfg = store
            .create(task_id, req.url.clone(), req.token.clone(), auth)
            .map_err(|e| match e {
                crate::a2a::core::push_notifications::PushNotificationError::InvalidInput {
                    reason,
                } => Status::invalid_argument(reason),
            })?;

        Ok(Response::new(push_config_to_proto(&cfg)))
    }

    async fn get_task_push_notification_config(
        &self,
        request: Request<proto::GetTaskPushNotificationConfigRequest>,
    ) -> Result<Response<proto::TaskPushNotificationConfig>, Status> {
        let req = request.into_inner();
        let task_id: TaskId = req
            .task_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;
        let cfg_id: PushNotificationId = req
            .id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid push notification config id"))?;

        let store = self.state.push_notifications.read().await;
        let cfg = store
            .get(&task_id, &cfg_id)
            .ok_or_else(|| Status::not_found("push notification config not found"))?;

        Ok(Response::new(push_config_to_proto(cfg)))
    }

    async fn list_task_push_notification_configs(
        &self,
        request: Request<proto::ListTaskPushNotificationConfigsRequest>,
    ) -> Result<Response<proto::ListTaskPushNotificationConfigsResponse>, Status> {
        let req = request.into_inner();
        let task_id: TaskId = req
            .task_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;

        let store = self.state.push_notifications.read().await;
        let configs: Vec<proto::TaskPushNotificationConfig> = store
            .list(&task_id)
            .iter()
            .map(push_config_to_proto)
            .collect();

        Ok(Response::new(
            proto::ListTaskPushNotificationConfigsResponse {
                configs,
                // No pagination yet — push-notification config lists are typically
                // very small per task.
                next_page_token: String::new(),
            },
        ))
    }

    async fn get_extended_agent_card(
        &self,
        _request: Request<proto::GetExtendedAgentCardRequest>,
    ) -> Result<Response<proto::AgentCard>, Status> {
        let card = proto::AgentCard {
            name: self.state.card.name.clone(),
            description: self.state.card.description.clone(),
            supported_interfaces: vec![proto::AgentInterface {
                url: self.state.card.grpc_url.clone(),
                protocol_binding: "GRPC".to_owned(),
                tenant: String::new(),
                protocol_version: "0.3".to_owned(),
            }],
            provider: Some(proto::AgentProvider {
                url: String::new(),
                organization: "basemind".to_owned(),
            }),
            version: self.state.card.version.clone(),
            documentation_url: None,
            capabilities: Some(proto::AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(true),
                extensions: vec![],
                extended_agent_card: Some(true),
            }),
            security_schemes: Default::default(),
            security_requirements: vec![],
            default_input_modes: vec!["text/plain".to_owned()],
            default_output_modes: vec!["text/plain".to_owned()],
            // B4: populate from basemind's tool/skill catalog once the agent
            // registry surface is exposed on `A2aState`. The upstream nexus
            // enumerated its tool registry here; basemind has none under the
            // `a2a` feature yet, so the card advertises no skills for now.
            skills: vec![],
            signatures: vec![],
            icon_url: None,
        };

        Ok(Response::new(card))
    }

    async fn delete_task_push_notification_config(
        &self,
        request: Request<proto::DeleteTaskPushNotificationConfigRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let task_id: TaskId = req
            .task_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid task id"))?;
        let cfg_id: PushNotificationId = req
            .id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid push notification config id"))?;

        let mut store = self.state.push_notifications.write().await;
        if store.delete(&task_id, &cfg_id) {
            Ok(Response::new(()))
        } else {
            Err(Status::not_found("push notification config not found"))
        }
    }
}

/// Map a [`TaskFacade::get_task`](crate::a2a::core::task_facade::TaskFacade::get_task)
/// failure to a gRPC [`Status`].
///
/// A lookup only yields [`TaskError::TaskNotFound`] today, but matching it
/// explicitly keeps any future [`FacadeError`] variant from silently
/// masquerading as `NOT_FOUND` — anything unexpected surfaces as `INTERNAL`.
fn get_task_error_to_status(err: &FacadeError) -> Status {
    match err {
        FacadeError::Task(TaskError::TaskNotFound { .. }) => Status::not_found(err.to_string()),
        _ => Status::internal(err.to_string()),
    }
}

/// Convert a core [`PushNotificationConfig`] to the proto wire type used by the
/// four push-notification RPCs.
fn push_config_to_proto(cfg: &PushNotificationConfig) -> proto::TaskPushNotificationConfig {
    proto::TaskPushNotificationConfig {
        tenant: String::new(),
        id: cfg.id.to_string(),
        task_id: cfg.task_id.to_string(),
        url: cfg.url.clone(),
        token: cfg.token.clone(),
        authentication: cfg
            .authentication
            .as_ref()
            .map(|a| proto::AuthenticationInfo {
                scheme: a.scheme.clone(),
                credentials: a.credentials.clone(),
            }),
    }
}

#[cfg(test)]
mod tests;
