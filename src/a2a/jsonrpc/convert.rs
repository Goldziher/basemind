//! Core ↔ JSON-RPC DTO conversion for the A2A JSON-RPC binding.
//!
//! The JSON analogue of [`crate::a2a::grpc::convert`]: all mappings between the
//! domain types in [`crate::a2a::core`] and the wire DTOs in [`super::dto`]
//! live here. The forward (`core_*_to_dto`) direction is infallible; the
//! reverse (`dto_*_to_core`) direction validates external input and returns a
//! [`ConvertError`] which [`super::protocol`] maps to a JSON-RPC error.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::a2a::core::push_notifications::{PushNotificationAuth, PushNotificationConfig};
use crate::a2a::core::task_types::{
    Artifact, MessageRole, Part, Task, TaskMessage, TaskState, TaskStatus,
};
use crate::a2a::core::types::MessageId;
use crate::a2a::state::AgentCardInfo;

use super::dto::{
    AgentCapabilitiesDto, AgentCardDto, AgentInterfaceDto, AgentProviderDto, ArtifactDto,
    FileContentDto, MessageDto, PartDto, PushAuthDto, PushNotificationConfigDto, RoleDto, TaskDto,
    TaskPushNotificationConfigDto, TaskStateDto, TaskStatusDto,
};

/// A2A protocol version advertised on the JSON-RPC agent card.
const PROTOCOL_VERSION: &str = "0.3.0";
/// Preferred transport advertised by this (JSON-RPC) binding.
const JSONRPC_TRANSPORT: &str = "JSONRPC";
/// Transport name for the gRPC interface entry on the agent card.
const GRPC_TRANSPORT: &str = "GRPC";

// ── Error ─────────────────────────────────────────────────────────────────

/// Failure converting a wire DTO into a core domain type.
///
/// Raised only on the reverse (deserialize) path, where external input may be
/// malformed. [`super::protocol`] maps this onto a JSON-RPC `InvalidParams`
/// error.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConvertError {
    /// A field carried a value that could not be converted.
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// The offending field name.
        field: String,
        /// Why the value was rejected.
        reason: String,
    },
}

impl ConvertError {
    /// Construct an [`ConvertError::Invalid`] from borrowed parts.
    fn invalid(field: &str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field: field.to_owned(),
            reason: reason.into(),
        }
    }
}

// ── TaskState ─────────────────────────────────────────────────────────────

/// Convert a core [`TaskState`] to its wire DTO (kebab-case on serialize).
pub(crate) fn core_state_to_dto(state: TaskState) -> TaskStateDto {
    match state {
        TaskState::Submitted => TaskStateDto::Submitted,
        TaskState::Working => TaskStateDto::Working,
        TaskState::Completed => TaskStateDto::Completed,
        TaskState::Failed => TaskStateDto::Failed,
        TaskState::Canceled => TaskStateDto::Canceled,
        TaskState::InputRequired => TaskStateDto::InputRequired,
        TaskState::AuthRequired => TaskStateDto::AuthRequired,
        TaskState::Rejected => TaskStateDto::Rejected,
    }
}

/// Convert a wire [`TaskStateDto`] back to a core [`TaskState`].
///
/// `Unknown` has no core analogue; it maps to [`TaskState::Submitted`] as the
/// most conservative non-terminal default.
// B4.5: the inbound direction is exercised by the A2A client (parsing task
// states off remote agents); kept as the complete bidirectional converter.
#[allow(dead_code)]
pub(crate) fn dto_state_to_core(state: TaskStateDto) -> TaskState {
    match state {
        TaskStateDto::Submitted | TaskStateDto::Unknown => TaskState::Submitted,
        TaskStateDto::Working => TaskState::Working,
        TaskStateDto::Completed => TaskState::Completed,
        TaskStateDto::Failed => TaskState::Failed,
        TaskStateDto::Canceled => TaskState::Canceled,
        TaskStateDto::InputRequired => TaskState::InputRequired,
        TaskStateDto::AuthRequired => TaskState::AuthRequired,
        TaskStateDto::Rejected => TaskState::Rejected,
    }
}

// ── Role ──────────────────────────────────────────────────────────────────

fn core_role_to_dto(role: MessageRole) -> RoleDto {
    match role {
        MessageRole::User => RoleDto::User,
        MessageRole::Agent => RoleDto::Agent,
    }
}

fn dto_role_to_core(role: RoleDto) -> MessageRole {
    match role {
        RoleDto::User => MessageRole::User,
        RoleDto::Agent => MessageRole::Agent,
    }
}

// ── Part ──────────────────────────────────────────────────────────────────

/// Convert a core [`Part`] to its wire DTO.
///
/// `Part::Bytes` becomes a `File` part with base64 inline bytes; `Part::Url`
/// becomes a `File` part with a `uri`.
pub(crate) fn core_part_to_dto(part: &Part) -> PartDto {
    match part {
        Part::Text { text } => PartDto::Text {
            text: text.clone(),
            metadata: None,
        },
        Part::Url { url } => PartDto::File {
            file: FileContentDto {
                bytes: None,
                uri: Some(url.clone()),
                mime_type: None,
            },
            metadata: None,
        },
        Part::Data { data } => PartDto::Data {
            data: data.clone(),
            metadata: None,
        },
        Part::Bytes { bytes } => PartDto::File {
            file: FileContentDto {
                bytes: Some(STANDARD.encode(bytes)),
                uri: None,
                mime_type: None,
            },
            metadata: None,
        },
    }
}

/// Convert a wire [`PartDto`] back to a core [`Part`].
///
/// # Errors
///
/// Returns [`ConvertError`] when a `File` part carries neither `bytes` nor
/// `uri`, or when inline `bytes` are not valid base64.
pub(crate) fn dto_part_to_core(part: &PartDto) -> Result<Part, ConvertError> {
    match part {
        PartDto::Text { text, .. } => Ok(Part::Text { text: text.clone() }),
        PartDto::Data { data, .. } => Ok(Part::Data { data: data.clone() }),
        PartDto::File { file, .. } => {
            if let Some(b64) = &file.bytes {
                let bytes = STANDARD
                    .decode(b64)
                    .map_err(|e| ConvertError::invalid("file.bytes", e.to_string()))?;
                Ok(Part::Bytes { bytes })
            } else if let Some(uri) = &file.uri {
                Ok(Part::Url { url: uri.clone() })
            } else {
                Err(ConvertError::invalid(
                    "file",
                    "file part must carry either 'bytes' or 'uri'",
                ))
            }
        }
    }
}

// ── Message ───────────────────────────────────────────────────────────────

/// Convert a core [`TaskMessage`] to its wire DTO.
pub(crate) fn core_message_to_dto(msg: &TaskMessage) -> MessageDto {
    MessageDto {
        message_id: msg.id.to_string(),
        role: core_role_to_dto(msg.role),
        parts: msg.parts.iter().map(core_part_to_dto).collect(),
        context_id: None,
        task_id: None,
        metadata: msg.metadata.clone(),
        kind: "message".to_owned(),
    }
}

/// Convert a wire [`MessageDto`] back to a core [`TaskMessage`].
///
/// An empty `messageId` is treated as "unset" and a fresh [`MessageId`] is
/// minted; a non-empty value must parse as a UUID.
///
/// # Errors
///
/// Returns [`ConvertError`] when `messageId` is non-empty but not a valid UUID,
/// or when any part fails to convert.
pub(crate) fn dto_message_to_core(msg: &MessageDto) -> Result<TaskMessage, ConvertError> {
    let id = if msg.message_id.is_empty() {
        MessageId::new()
    } else {
        msg.message_id
            .parse()
            .map_err(|_| ConvertError::invalid("messageId", "not a valid UUID"))?
    };
    let parts = msg
        .parts
        .iter()
        .map(dto_part_to_core)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TaskMessage {
        id,
        role: dto_role_to_core(msg.role),
        parts,
        metadata: msg.metadata.clone(),
    })
}

// ── TaskStatus ────────────────────────────────────────────────────────────

/// Convert a core [`TaskStatus`] to its wire DTO.
pub(crate) fn core_status_to_dto(status: &TaskStatus) -> TaskStatusDto {
    TaskStatusDto {
        state: core_state_to_dto(status.state),
        message: status.message.as_ref().map(core_message_to_dto),
        timestamp: Some(status.timestamp.to_rfc3339()),
    }
}

// ── Artifact ──────────────────────────────────────────────────────────────

/// Convert a core [`Artifact`] to its wire DTO.
pub(crate) fn core_artifact_to_dto(artifact: &Artifact) -> ArtifactDto {
    ArtifactDto {
        artifact_id: artifact.id.to_string(),
        name: artifact.name.clone(),
        description: artifact.description.clone(),
        parts: artifact.parts.iter().map(core_part_to_dto).collect(),
        metadata: artifact.metadata.clone(),
    }
}

// ── Task ──────────────────────────────────────────────────────────────────

/// Convert a core [`Task`] to its wire DTO.
///
/// The non-spec core fields (`assignee` / `creator` / `deadline`) are dropped;
/// the A2A wire task carries only the spec-defined surface.
pub(crate) fn core_task_to_dto(task: &Task) -> TaskDto {
    TaskDto {
        id: task.id.to_string(),
        context_id: task.context_id.to_string(),
        status: core_status_to_dto(&task.status),
        artifacts: task.artifacts.iter().map(core_artifact_to_dto).collect(),
        history: task.history.iter().map(core_message_to_dto).collect(),
        metadata: task.metadata.clone(),
        kind: "task".to_owned(),
    }
}

// ── Agent card ────────────────────────────────────────────────────────────

/// Build the JSON-RPC agent card DTO from the static [`AgentCardInfo`].
pub(crate) fn core_card_to_dto(card: &AgentCardInfo) -> AgentCardDto {
    // Advertise a bearer scheme only when the server actually enforces auth, so
    // the public discovery card never claims protection it doesn't apply.
    let (security_schemes, security) = if card.requires_auth {
        (
            Some(serde_json::json!({
                "bearer": { "type": "http", "scheme": "bearer" }
            })),
            vec![serde_json::json!({ "bearer": [] })],
        )
    } else {
        (None, Vec::new())
    };

    AgentCardDto {
        name: card.name.clone(),
        description: card.description.clone(),
        version: card.version.clone(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        url: card.http_url.clone(),
        preferred_transport: JSONRPC_TRANSPORT.to_owned(),
        additional_interfaces: vec![
            AgentInterfaceDto {
                url: card.grpc_url.clone(),
                transport: GRPC_TRANSPORT.to_owned(),
            },
            AgentInterfaceDto {
                url: card.http_url.clone(),
                transport: JSONRPC_TRANSPORT.to_owned(),
            },
        ],
        capabilities: AgentCapabilitiesDto {
            streaming: true,
            push_notifications: true,
        },
        default_input_modes: vec!["text/plain".to_owned()],
        default_output_modes: vec!["text/plain".to_owned()],
        skills: vec![],
        security_schemes,
        security,
        provider: Some(AgentProviderDto {
            organization: "basemind".to_owned(),
            url: String::new(),
        }),
    }
}

// ── Push notification config ──────────────────────────────────────────────

/// Convert a core [`PushNotificationConfig`] to the wire result DTO.
pub(crate) fn core_push_config_to_dto(
    config: &PushNotificationConfig,
) -> TaskPushNotificationConfigDto {
    TaskPushNotificationConfigDto {
        task_id: config.task_id.to_string(),
        push_notification_config: PushNotificationConfigDto {
            id: Some(config.id.to_string()),
            url: config.url.clone(),
            token: config.token.clone(),
            authentication: config.authentication.as_ref().map(core_push_auth_to_dto),
        },
    }
}

fn core_push_auth_to_dto(auth: &PushNotificationAuth) -> PushAuthDto {
    PushAuthDto {
        scheme: auth.scheme.clone(),
        credentials: auth.credentials.clone(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::core::task_types::{ArtifactId, ContextId, TaskId};

    fn text_bytes_task() -> Task {
        Task {
            id: TaskId::new(),
            context_id: ContextId::new(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: Some(TaskMessage {
                    id: MessageId::new(),
                    role: MessageRole::Agent,
                    parts: vec![Part::Text {
                        text: "need more input".to_owned(),
                    }],
                    metadata: None,
                }),
                timestamp: chrono::Utc::now(),
            },
            artifacts: vec![Artifact {
                id: ArtifactId::new(),
                name: Some("blob".to_owned()),
                description: None,
                parts: vec![Part::Bytes {
                    bytes: vec![0x00, 0x01, 0xff, b'\n', 0x42],
                }],
                metadata: None,
            }],
            history: vec![TaskMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                parts: vec![Part::Text {
                    text: "hello".to_owned(),
                }],
                metadata: None,
            }],
            metadata: None,
            assignee: None,
            creator: None,
            deadline: None,
        }
    }

    #[test]
    fn state_input_required_survives_as_kebab_on_wire() {
        let dto = core_state_to_dto(TaskState::InputRequired);
        let value = serde_json::to_value(dto).expect("serialize must succeed");
        assert_eq!(value, serde_json::json!("input-required"));
        assert_eq!(dto_state_to_core(dto), TaskState::InputRequired);
    }

    #[test]
    fn state_auth_required_survives_as_kebab_on_wire() {
        let dto = core_state_to_dto(TaskState::AuthRequired);
        let value = serde_json::to_value(dto).expect("serialize must succeed");
        assert_eq!(value, serde_json::json!("auth-required"));
        assert_eq!(dto_state_to_core(dto), TaskState::AuthRequired);
    }

    #[test]
    fn bytes_part_round_trips_through_file_dto() {
        let original = Part::Bytes {
            bytes: vec![0x00, 0x01, 0xff, b'\n', 0x42],
        };
        let dto = core_part_to_dto(&original);
        match &dto {
            PartDto::File { file, .. } => {
                assert!(file.bytes.is_some(), "bytes part must carry inline base64");
                assert!(file.uri.is_none(), "bytes part must not carry a uri");
            }
            other => panic!("expected File part, got {other:?}"),
        }
        let back = dto_part_to_core(&dto).expect("round-trip must succeed");
        assert_eq!(back, original, "binary part must survive round-trip");
    }

    #[test]
    fn url_part_maps_to_file_uri() {
        let original = Part::Url {
            url: "https://example.com/x".to_owned(),
        };
        let dto = core_part_to_dto(&original);
        let back = dto_part_to_core(&dto).expect("round-trip must succeed");
        assert_eq!(back, original, "url part must survive round-trip");
    }

    #[test]
    fn text_part_round_trips() {
        let original = Part::Text {
            text: "hi".to_owned(),
        };
        let dto = core_part_to_dto(&original);
        let back = dto_part_to_core(&dto).expect("round-trip must succeed");
        assert_eq!(back, original);
    }

    #[test]
    fn data_part_round_trips() {
        let original = Part::Data {
            data: serde_json::json!({"k": "v"}),
        };
        let dto = core_part_to_dto(&original);
        let back = dto_part_to_core(&dto).expect("round-trip must succeed");
        assert_eq!(back, original);
    }

    #[test]
    fn file_part_with_neither_bytes_nor_uri_errors() {
        let dto = PartDto::File {
            file: FileContentDto {
                bytes: None,
                uri: None,
                mime_type: None,
            },
            metadata: None,
        };
        let err = dto_part_to_core(&dto).expect_err("empty file part must error");
        assert!(matches!(err, ConvertError::Invalid { ref field, .. } if field == "file"));
    }

    #[test]
    fn file_part_with_bad_base64_errors() {
        let dto = PartDto::File {
            file: FileContentDto {
                bytes: Some("!@#%^&*".to_owned()),
                uri: None,
                mime_type: None,
            },
            metadata: None,
        };
        let err = dto_part_to_core(&dto).expect_err("bad base64 must error");
        assert!(matches!(err, ConvertError::Invalid { ref field, .. } if field == "file.bytes"));
    }

    #[test]
    fn message_with_empty_id_mints_fresh_uuid() {
        let dto = MessageDto {
            message_id: String::new(),
            role: RoleDto::User,
            parts: vec![],
            context_id: None,
            task_id: None,
            metadata: None,
            kind: "message".to_owned(),
        };
        let core = dto_message_to_core(&dto).expect("empty id must mint a fresh uuid");
        assert!(!core.id.to_string().is_empty());
    }

    #[test]
    fn message_with_invalid_id_errors() {
        let dto = MessageDto {
            message_id: "not-a-uuid".to_owned(),
            role: RoleDto::User,
            parts: vec![],
            context_id: None,
            task_id: None,
            metadata: None,
            kind: "message".to_owned(),
        };
        let err = dto_message_to_core(&dto).expect_err("invalid uuid must error");
        assert!(matches!(err, ConvertError::Invalid { ref field, .. } if field == "messageId"));
    }

    #[test]
    fn message_round_trips_through_dto() {
        let original = TaskMessage {
            id: MessageId::new(),
            role: MessageRole::Agent,
            parts: vec![Part::Text {
                text: "x".to_owned(),
            }],
            metadata: Some(serde_json::json!({"a": 1})),
        };
        let dto = core_message_to_dto(&original);
        let back = dto_message_to_core(&dto).expect("round-trip must succeed");
        assert_eq!(back, original, "message must survive round-trip");
    }

    #[test]
    fn task_converts_to_dto_with_spec_fields() {
        let task = text_bytes_task();
        let dto = core_task_to_dto(&task);
        assert_eq!(dto.id, task.id.to_string());
        assert_eq!(dto.context_id, task.context_id.to_string());
        assert_eq!(dto.kind, "task");
        assert_eq!(dto.status.state, TaskStateDto::InputRequired);
        assert_eq!(dto.artifacts.len(), 1);
        assert_eq!(dto.history.len(), 1);
        // Serialized shape carries camelCase + kebab state.
        let value = serde_json::to_value(&dto).expect("serialize must succeed");
        assert_eq!(value["contextId"], task.context_id.to_string());
        assert_eq!(
            value["status"]["state"],
            serde_json::json!("input-required")
        );
    }

    fn sample_card() -> AgentCardInfo {
        AgentCardInfo {
            name: "basemind".to_owned(),
            description: "d".to_owned(),
            version: "1.2.3".to_owned(),
            grpc_url: "http://grpc".to_owned(),
            http_url: "http://http".to_owned(),
            requires_auth: false,
        }
    }

    #[test]
    fn card_uses_jsonrpc_preferred_transport() {
        let dto = core_card_to_dto(&sample_card());
        assert_eq!(dto.protocol_version, "0.3.0");
        assert_eq!(dto.preferred_transport, "JSONRPC");
        assert_eq!(dto.url, "http://http");
        assert_eq!(dto.additional_interfaces.len(), 2);
        assert!(dto.capabilities.streaming);
        assert!(dto.capabilities.push_notifications);
    }

    #[test]
    fn card_omits_security_when_auth_disabled() {
        let dto = core_card_to_dto(&sample_card());
        assert!(dto.security_schemes.is_none());
        assert!(dto.security.is_empty());
    }

    #[test]
    fn card_advertises_bearer_when_auth_enabled() {
        let mut card = sample_card();
        card.requires_auth = true;
        let dto = core_card_to_dto(&card);
        let schemes = dto
            .security_schemes
            .expect("auth card must advertise schemes");
        assert_eq!(schemes["bearer"]["type"], serde_json::json!("http"));
        assert_eq!(schemes["bearer"]["scheme"], serde_json::json!("bearer"));
        assert_eq!(dto.security, vec![serde_json::json!({ "bearer": [] })]);
    }

    #[test]
    fn push_config_converts_to_dto() {
        use crate::a2a::core::push_notifications::PushNotificationId;

        let config = PushNotificationConfig {
            id: PushNotificationId::new(),
            task_id: TaskId::new(),
            url: "https://hook.example/".to_owned(),
            token: "tok".to_owned(),
            authentication: Some(PushNotificationAuth {
                scheme: "Bearer".to_owned(),
                credentials: "secret".to_owned(),
            }),
        };
        let dto = core_push_config_to_dto(&config);
        assert_eq!(dto.task_id, config.task_id.to_string());
        assert_eq!(dto.push_notification_config.url, "https://hook.example/");
        let auth = dto
            .push_notification_config
            .authentication
            .expect("auth must be present");
        assert_eq!(auth.scheme, "Bearer");
        assert_eq!(auth.credentials, "secret");
    }
}
