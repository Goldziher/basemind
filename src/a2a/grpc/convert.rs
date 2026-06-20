//! Proto ↔ core type conversion for the A2A gRPC service.
//!
//! All conversions between `proto::*` (prost-generated) and `core::*`
//! (domain) types live here, keeping the service implementation clean.

// `tonic::Status` is ~176 bytes, so `Result<T, Status>` trips
// `clippy::result_large_err`. The error type is not ours to shrink: the
// generated `A2aService` trait mandates `Status` as its error, and these
// helpers exist precisely to feed those trait methods. Boxing here would only
// force an unbox at the trait boundary, so the lint is suppressed module-wide.
#![allow(clippy::result_large_err)]

use tonic::Status;

use crate::a2a::core::bus::Event;
use crate::a2a::core::task_types::{
    Artifact, ContextId, MessageRole, Part, Task, TaskId, TaskMessage, TaskState, TaskStatus,
};
use crate::a2a::core::types::MessageId;
use crate::a2a::v1 as proto;

// ── TaskState ───────────────────────────────────────────────────────────────

/// Convert a core [`TaskState`] to the proto enum value.
pub(crate) fn core_state_to_proto(state: TaskState) -> i32 {
    match state {
        TaskState::Submitted => proto::TaskState::Submitted.into(),
        TaskState::Working => proto::TaskState::Working.into(),
        TaskState::Completed => proto::TaskState::Completed.into(),
        TaskState::Failed => proto::TaskState::Failed.into(),
        TaskState::Canceled => proto::TaskState::Canceled.into(),
        TaskState::InputRequired => proto::TaskState::InputRequired.into(),
        TaskState::Rejected => proto::TaskState::Rejected.into(),
        TaskState::AuthRequired => proto::TaskState::AuthRequired.into(),
    }
}

/// Convert a proto task state i32 to a core [`TaskState`].
pub(crate) fn proto_state_to_core(value: i32) -> Result<TaskState, Status> {
    match proto::TaskState::try_from(value) {
        Ok(proto::TaskState::Submitted) => Ok(TaskState::Submitted),
        Ok(proto::TaskState::Working) => Ok(TaskState::Working),
        Ok(proto::TaskState::Completed) => Ok(TaskState::Completed),
        Ok(proto::TaskState::Failed) => Ok(TaskState::Failed),
        Ok(proto::TaskState::Canceled) => Ok(TaskState::Canceled),
        Ok(proto::TaskState::InputRequired) => Ok(TaskState::InputRequired),
        Ok(proto::TaskState::Rejected) => Ok(TaskState::Rejected),
        Ok(proto::TaskState::AuthRequired) => Ok(TaskState::AuthRequired),
        Ok(proto::TaskState::Unspecified) | Err(_) => Err(Status::invalid_argument(format!(
            "unknown task state: {value}"
        ))),
    }
}

// ── Role ────────────────────────────────────────────────────────────────────

fn core_role_to_proto(role: &MessageRole) -> i32 {
    match *role {
        MessageRole::User => proto::Role::User.into(),
        MessageRole::Agent => proto::Role::Agent.into(),
    }
}

fn proto_role_to_core(value: i32) -> Result<MessageRole, Status> {
    match proto::Role::try_from(value) {
        Ok(proto::Role::User) => Ok(MessageRole::User),
        Ok(proto::Role::Agent) => Ok(MessageRole::Agent),
        _ => Err(Status::invalid_argument(format!("unknown role: {value}"))),
    }
}

// ── Part ────────────────────────────────────────────────────────────────────

fn core_part_to_proto(part: &Part) -> proto::Part {
    let content = match part {
        Part::Text { text } => Some(proto::part::Content::Text(text.clone())),
        Part::Url { url } => Some(proto::part::Content::Url(url.clone())),
        Part::Data { data } => {
            let prost_val = json_to_prost_value(data.clone());
            Some(proto::part::Content::Data(prost_val))
        }
        Part::Bytes { bytes } => Some(proto::part::Content::Raw(bytes.clone())),
    };
    proto::Part {
        content,
        metadata: None,
        filename: String::new(),
        media_type: String::new(),
    }
}

fn proto_part_to_core(part: &proto::Part) -> Result<Part, Status> {
    match &part.content {
        Some(proto::part::Content::Text(text)) => Ok(Part::Text { text: text.clone() }),
        Some(proto::part::Content::Url(url)) => Ok(Part::Url { url: url.clone() }),
        Some(proto::part::Content::Data(val)) => {
            let json = prost_value_to_json(val);
            Ok(Part::Data { data: json })
        }
        Some(proto::part::Content::Raw(bytes)) => Ok(Part::Bytes {
            bytes: bytes.to_vec(),
        }),
        None => Err(Status::invalid_argument("part has no content")),
    }
}

// ── Message ─────────────────────────────────────────────────────────────────

/// Convert a core [`TaskMessage`] to a proto [`Message`].
pub(crate) fn core_message_to_proto(msg: &TaskMessage) -> proto::Message {
    proto::Message {
        message_id: msg.id.to_string(),
        context_id: String::new(),
        task_id: String::new(),
        role: core_role_to_proto(&msg.role),
        parts: msg.parts.iter().map(core_part_to_proto).collect(),
        metadata: msg
            .metadata
            .as_ref()
            .map(|m| json_to_prost_struct(m.clone())),
        extensions: vec![],
        reference_task_ids: vec![],
    }
}

/// Convert a proto [`Message`] to a core [`TaskMessage`].
pub(crate) fn proto_message_to_core(msg: &proto::Message) -> Result<TaskMessage, Status> {
    let role = proto_role_to_core(msg.role)?;
    let parts: Result<Vec<Part>, Status> = msg.parts.iter().map(proto_part_to_core).collect();
    let metadata = msg.metadata.as_ref().map(prost_struct_to_json);

    Ok(TaskMessage {
        id: if msg.message_id.is_empty() {
            MessageId::new()
        } else {
            msg.message_id
                .parse()
                .map_err(|_| Status::invalid_argument("invalid message_id UUID"))?
        },
        role,
        parts: parts?,
        metadata,
    })
}

// ── TaskStatus ──────────────────────────────────────────────────────────────

fn core_status_to_proto(status: &TaskStatus) -> proto::TaskStatus {
    proto::TaskStatus {
        state: core_state_to_proto(status.state),
        message: status.message.as_ref().map(core_message_to_proto),
        timestamp: Some(datetime_to_timestamp(status.timestamp)),
    }
}

// ── Artifact ────────────────────────────────────────────────────────────────

fn core_artifact_to_proto(artifact: &Artifact) -> proto::Artifact {
    proto::Artifact {
        artifact_id: artifact.id.to_string(),
        name: artifact.name.clone().unwrap_or_default(),
        description: artifact.description.clone().unwrap_or_default(),
        parts: artifact.parts.iter().map(core_part_to_proto).collect(),
        metadata: artifact
            .metadata
            .as_ref()
            .map(|m| json_to_prost_struct(m.clone())),
        extensions: vec![],
    }
}

// ── Task ────────────────────────────────────────────────────────────────────

/// Convert a core [`Task`] to a proto [`Task`].
pub(crate) fn core_task_to_proto(task: &Task) -> proto::Task {
    proto::Task {
        id: task.id.to_string(),
        context_id: task.context_id.to_string(),
        status: Some(core_status_to_proto(&task.status)),
        artifacts: task.artifacts.iter().map(core_artifact_to_proto).collect(),
        history: task.history.iter().map(core_message_to_proto).collect(),
        metadata: task
            .metadata
            .as_ref()
            .map(|m| json_to_prost_struct(m.clone())),
    }
}

// ── Stream event filtering ──────────────────────────────────────────────────

/// Filter a bus [`Event`] for a specific task and convert it to the proto
/// streaming envelope used by `SendStreamingMessage` / `SubscribeToTask`.
///
/// Returns `None` when the event does not pertain to the requested task or
/// is not relevant to the stream (e.g. agent or chat events). The caller is
/// expected to supply `context_id` because the bus event carries only the
/// task id; the context is stable across a task's lifetime so callers cache
/// it from the initial task fetch.
///
/// `task` is consulted only when an artifact update arrives, to look up the
/// artifact body that the bus event references by id alone. Pass the most
/// recently observed task snapshot.
pub(crate) fn task_event_to_stream_response(
    event: &Event,
    task_id: &TaskId,
    context_id: &ContextId,
    task: Option<&Task>,
) -> Option<proto::StreamResponse> {
    use proto::stream_response::Payload;

    match event {
        Event::TaskCreated(t) if &t.id == task_id => Some(proto::StreamResponse {
            payload: Some(Payload::Task(core_task_to_proto(t))),
        }),
        Event::TaskStatusChanged {
            task_id: tid,
            new_state,
            ..
        } if tid == task_id => {
            let status = proto::TaskStatus {
                state: core_state_to_proto(*new_state),
                message: None,
                timestamp: Some(datetime_to_timestamp(chrono::Utc::now())),
            };
            Some(proto::StreamResponse {
                payload: Some(Payload::StatusUpdate(proto::TaskStatusUpdateEvent {
                    task_id: tid.to_string(),
                    context_id: context_id.to_string(),
                    status: Some(status),
                    metadata: None,
                })),
            })
        }
        Event::TaskArtifactAdded {
            task_id: tid,
            artifact_id,
        } if tid == task_id => {
            let artifact = task?
                .artifacts
                .iter()
                .find(|a| &a.id == artifact_id)
                .map(core_artifact_to_proto)?;
            Some(proto::StreamResponse {
                payload: Some(Payload::ArtifactUpdate(proto::TaskArtifactUpdateEvent {
                    task_id: tid.to_string(),
                    context_id: context_id.to_string(),
                    artifact: Some(artifact),
                    append: false,
                    last_chunk: false,
                    metadata: None,
                })),
            })
        }
        _ => None,
    }
}

// ── Timestamp helpers ───────────────────────────────────────────────────────

fn datetime_to_timestamp(dt: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

// ── JSON ↔ prost_types helpers ──────────────────────────────────────────────

fn json_to_prost_struct(val: serde_json::Value) -> prost_types::Struct {
    match val {
        serde_json::Value::Object(map) => prost_types::Struct {
            fields: map
                .into_iter()
                .map(|(k, v)| (k, json_to_prost_value(v)))
                .collect(),
        },
        _ => prost_types::Struct::default(),
    }
}

fn json_to_prost_value(val: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match val {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(b),
        serde_json::Value::Number(n) => {
            // serde_json guarantees one of as_f64/as_i64/as_u64 succeeds, but
            // we cascade for integers outside the f64-representable range.
            // The final `unwrap_or` branch should be unreachable; if a JSON
            // Number ever survives all three, log loudly and emit Null so the
            // caller sees a clear marker rather than a silent zero.
            let opt = n
                .as_f64()
                .or_else(|| n.as_i64().map(|i| i as f64))
                .or_else(|| n.as_u64().map(|u| u as f64));
            match opt {
                Some(f) if f.is_finite() => Kind::NumberValue(f),
                Some(f) => {
                    tracing::warn!(
                        value = ?f,
                        "non-finite JSON number converted to Null for proto Struct"
                    );
                    Kind::NullValue(0)
                }
                None => {
                    tracing::warn!(
                        number = ?n,
                        "JSON number is neither f64 nor i64 nor u64; emitting Null"
                    );
                    Kind::NullValue(0)
                }
            }
        }
        serde_json::Value::String(s) => Kind::StringValue(s),
        serde_json::Value::Array(arr) => Kind::ListValue(prost_types::ListValue {
            values: arr.into_iter().map(json_to_prost_value).collect(),
        }),
        serde_json::Value::Object(map) => Kind::StructValue(prost_types::Struct {
            fields: map
                .into_iter()
                .map(|(k, v)| (k, json_to_prost_value(v)))
                .collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

fn prost_struct_to_json(s: &prost_types::Struct) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = s
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
        .collect();
    serde_json::Value::Object(map)
}

fn prost_value_to_json(v: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match &v.kind {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => prost_struct_to_json(s),
        None => serde_json::Value::Null,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::core::task_types::{ContextId, TaskId};

    #[test]
    fn state_round_trip() {
        let states = [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::InputRequired,
            TaskState::Rejected,
            TaskState::AuthRequired,
        ];
        for state in states {
            let proto_val = core_state_to_proto(state);
            let back = proto_state_to_core(proto_val).expect("round-trip must succeed");
            assert_eq!(back, state, "state must survive round-trip");
        }
    }

    #[test]
    fn json_struct_round_trip() {
        let original = serde_json::json!({"key": "value", "num": 42.0, "nested": {"a": true}});
        let prost = json_to_prost_struct(original.clone());
        let back = prost_struct_to_json(&prost);
        assert_eq!(back, original, "JSON struct must survive prost round-trip");
    }

    #[test]
    fn part_text_round_trip() {
        let core_part = Part::Text {
            text: "hello".to_owned(),
        };
        let proto_part = core_part_to_proto(&core_part);
        let back = proto_part_to_core(&proto_part).expect("round-trip must succeed");
        assert_eq!(back, core_part, "text part must survive round-trip");
    }

    #[test]
    fn part_bytes_round_trips_through_proto() {
        let core_part = Part::Bytes {
            bytes: vec![0x00, 0x01, 0xff, b'\n', 0x42],
        };
        let proto_part = core_part_to_proto(&core_part);
        // Confirm the proto carries the Raw variant verbatim.
        assert!(matches!(
            proto_part.content,
            Some(proto::part::Content::Raw(_))
        ));
        let back = proto_part_to_core(&proto_part).expect("round-trip must succeed");
        assert_eq!(back, core_part, "binary part must survive round-trip");
    }

    #[test]
    fn part_bytes_serializes_to_base64_json() {
        let part = Part::Bytes {
            bytes: vec![b'h', b'i'],
        };
        let json = serde_json::to_value(&part).expect("serialize");
        assert_eq!(json["type"], "bytes");
        // "hi" base64 = "aGk=".
        assert_eq!(json["bytes"], "aGk=");
        let back: Part = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, part);
    }

    #[test]
    fn part_bytes_deserialize_rejects_invalid_base64() {
        let json = serde_json::json!({"type":"bytes","bytes":"!@#%^&*"});
        let err =
            serde_json::from_value::<Part>(json).expect_err("invalid base64 must fail to parse");
        assert!(
            err.to_string().to_ascii_lowercase().contains("base64")
                || err.to_string().to_ascii_lowercase().contains("invalid"),
            "error message must indicate base64 decoding failed: {err}"
        );
    }

    #[test]
    fn message_converts_to_proto() {
        let msg = TaskMessage {
            id: MessageId::new(),
            role: MessageRole::User,
            parts: vec![Part::Text {
                text: "test".to_owned(),
            }],
            metadata: None,
        };
        let proto_msg = core_message_to_proto(&msg);
        assert_eq!(proto_msg.message_id, msg.id.to_string());
        assert_eq!(proto_msg.role, proto::Role::User as i32);
        assert_eq!(proto_msg.parts.len(), 1);
    }

    #[test]
    fn task_converts_to_proto() {
        let task = Task {
            id: TaskId::new(),
            context_id: ContextId::new(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: chrono::Utc::now(),
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
            assignee: None,
            creator: None,
            deadline: None,
        };
        let proto_task = core_task_to_proto(&task);
        assert_eq!(proto_task.id, task.id.to_string());
        assert!(proto_task.status.is_some());
    }

    // ── stream-response filtering ───────────────────────────────────────────

    fn make_task() -> Task {
        Task {
            id: TaskId::new(),
            context_id: ContextId::new(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: chrono::Utc::now(),
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
            assignee: None,
            creator: None,
            deadline: None,
        }
    }

    #[test]
    fn stream_filter_emits_task_for_matching_creation() {
        let task = make_task();
        let event = Event::TaskCreated(Box::new(task.clone()));
        let resp = task_event_to_stream_response(&event, &task.id, &task.context_id, None)
            .expect("matching task creation must produce a stream response");
        assert!(matches!(
            resp.payload,
            Some(proto::stream_response::Payload::Task(_))
        ));
    }

    #[test]
    fn stream_filter_drops_unrelated_task_creation() {
        let task = make_task();
        let other = make_task();
        let event = Event::TaskCreated(Box::new(other));
        let resp = task_event_to_stream_response(&event, &task.id, &task.context_id, None);
        assert!(resp.is_none(), "unrelated task creation must not stream");
    }

    #[test]
    fn stream_filter_emits_status_update_for_matching_task() {
        let task = make_task();
        let event = Event::TaskStatusChanged {
            task_id: task.id,
            old_state: TaskState::Submitted,
            new_state: TaskState::Working,
        };
        let resp = task_event_to_stream_response(&event, &task.id, &task.context_id, None)
            .expect("matching status change must produce a stream response");
        let payload = resp
            .payload
            .expect("status-update payload must be populated");
        match payload {
            proto::stream_response::Payload::StatusUpdate(update) => {
                assert_eq!(update.task_id, task.id.to_string());
                assert_eq!(update.context_id, task.context_id.to_string());
                assert_eq!(
                    update.status.expect("status").state,
                    proto::TaskState::Working as i32
                );
            }
            other => panic!("expected StatusUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn stream_filter_ignores_agent_events() {
        use crate::a2a::core::types::{AgentId, AgentInfo, AgentStatus};

        let task = make_task();
        let event = Event::AgentDeregistered(AgentId::new());
        assert!(
            task_event_to_stream_response(&event, &task.id, &task.context_id, None).is_none(),
            "agent events must not appear on a task stream"
        );

        let info = AgentInfo {
            id: AgentId::new(),
            name: "x".to_owned(),
            registered_at: chrono::Utc::now(),
            last_heartbeat_at: chrono::Utc::now(),
            status: AgentStatus::Connected,
            capabilities: None,
        };
        let event = Event::AgentRegistered(info);
        assert!(
            task_event_to_stream_response(&event, &task.id, &task.context_id, None).is_none(),
            "agent events must not appear on a task stream"
        );
    }

    #[test]
    fn stream_filter_emits_artifact_when_task_carries_it() {
        use crate::a2a::core::task_types::ArtifactId;

        let mut task = make_task();
        let artifact_id = ArtifactId::new();
        task.artifacts.push(Artifact {
            id: artifact_id,
            name: Some("doc".to_owned()),
            description: None,
            parts: vec![Part::Text {
                text: "body".to_owned(),
            }],
            metadata: None,
        });

        let event = Event::TaskArtifactAdded {
            task_id: task.id,
            artifact_id,
        };
        let resp = task_event_to_stream_response(&event, &task.id, &task.context_id, Some(&task))
            .expect("artifact event with snapshot must yield response");
        assert!(matches!(
            resp.payload,
            Some(proto::stream_response::Payload::ArtifactUpdate(_))
        ));
    }
}
