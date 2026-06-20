//! A2A JSON-RPC wire DTOs (camelCase, `kind`-discriminated, kebab-case states).
//!
//! These structs are the *wire* shape for the A2A JSON-RPC binding — they are
//! deliberately distinct from the core domain types in
//! [`crate::a2a::core`]. The core types serialize as snake_case with a `type`
//! tag on [`Part`](crate::a2a::core::task_types::Part) and emit
//! `input_required` / `auth_required` for the interrupted states; the A2A spec
//! demands camelCase, a `kind` discriminator, and kebab-case task states
//! (`input-required` / `auth-required`). Rather than overload the core serde,
//! this module owns the wire DTOs and [`super::convert`] bridges the two.
//!
//! All DTOs are `pub(crate)`: they exist only to back the JSON-RPC handlers.

use serde::{Deserialize, Serialize};

// ── Parts ─────────────────────────────────────────────────────────────────

/// A single content part on the wire (`{"kind":"text"|"file"|"data", …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum PartDto {
    /// Plain UTF-8 text content.
    Text {
        /// The text payload.
        text: String,
        /// Optional provider-specific metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// File content carried either inline (base64) or by reference (uri).
    File {
        /// The file body or reference.
        file: FileContentDto,
        /// Optional provider-specific metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
    /// Arbitrary structured JSON data.
    Data {
        /// The structured payload.
        data: serde_json::Value,
        /// Optional provider-specific metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

/// File content for a [`PartDto::File`] — inline base64 bytes or a uri.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileContentDto {
    /// Base64-encoded inline file bytes (maps to core `Part::Bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bytes: Option<String>,
    /// URI reference to external file content (maps to core `Part::Url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) uri: Option<String>,
    /// Optional MIME type (`mimeType` on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mime_type: Option<String>,
}

// ── Message ───────────────────────────────────────────────────────────────

/// Authoring role on the wire (`"user"` / `"agent"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RoleDto {
    /// Authored by the human user.
    User,
    /// Authored by an AI agent.
    Agent,
}

/// Default value for the `kind` discriminator on a [`MessageDto`].
fn message_kind() -> String {
    "message".to_owned()
}

/// A single A2A message (`{"kind":"message", …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageDto {
    /// Stable identity for this message (`messageId`).
    pub(crate) message_id: String,
    /// Who authored the message.
    pub(crate) role: RoleDto,
    /// Ordered content parts.
    pub(crate) parts: Vec<PartDto>,
    /// Optional execution-context id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context_id: Option<String>,
    /// Optional task id this message belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<String>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
    /// Literal `"message"` discriminator — always emitted on serialize.
    #[serde(default = "message_kind")]
    pub(crate) kind: String,
}

// ── Task status ───────────────────────────────────────────────────────────

/// Lifecycle state on the wire — kebab-case per the A2A spec.
///
/// Note the kebab forms `input-required` / `auth-required`; the core
/// [`TaskState`](crate::a2a::core::task_types::TaskState) emits snake_case
/// (`input_required`), which is *wrong* for the wire — hence this dedicated DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TaskStateDto {
    /// Submitted, awaiting pickup.
    Submitted,
    /// Actively being worked on.
    Working,
    /// Paused; needs more user input (`"input-required"`).
    InputRequired,
    /// Paused; needs the user to complete an auth flow (`"auth-required"`).
    AuthRequired,
    /// Finished successfully.
    Completed,
    /// Ended with an unrecoverable error.
    Failed,
    /// Canceled before or during execution.
    Canceled,
    /// Rejected before any work started.
    Rejected,
    /// Unknown / unspecified state.
    Unknown,
}

/// A status snapshot on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskStatusDto {
    /// Current lifecycle state.
    pub(crate) state: TaskStateDto,
    /// Optional explanatory message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<MessageDto>,
    /// Optional RFC3339 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timestamp: Option<String>,
}

// ── Artifact ──────────────────────────────────────────────────────────────

/// An agent-produced output on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactDto {
    /// Stable identity for this artifact (`artifactId`).
    pub(crate) artifact_id: String,
    /// Optional human-readable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    /// Ordered content parts.
    pub(crate) parts: Vec<PartDto>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
}

// ── Task ──────────────────────────────────────────────────────────────────

/// Default value for the `kind` discriminator on a [`TaskDto`].
fn task_kind() -> String {
    "task".to_owned()
}

/// A task on the wire (`{"kind":"task", …}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskDto {
    /// Stable identity for this task.
    pub(crate) id: String,
    /// Execution context id (`contextId`).
    pub(crate) context_id: String,
    /// Current status.
    pub(crate) status: TaskStatusDto,
    /// Artifacts produced so far.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifacts: Vec<ArtifactDto>,
    /// Conversation history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) history: Vec<MessageDto>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
    /// Literal `"task"` discriminator — always emitted on serialize.
    #[serde(default = "task_kind")]
    pub(crate) kind: String,
}

// ── Agent card ────────────────────────────────────────────────────────────

/// An agent card on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentCardDto {
    /// Human-readable agent name.
    pub(crate) name: String,
    /// Human-readable agent description.
    pub(crate) description: String,
    /// Agent version string.
    pub(crate) version: String,
    /// A2A protocol version (`protocolVersion`).
    pub(crate) protocol_version: String,
    /// Primary endpoint URL.
    pub(crate) url: String,
    /// Preferred transport name (`preferredTransport`).
    pub(crate) preferred_transport: String,
    /// Additional transport interfaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) additional_interfaces: Vec<AgentInterfaceDto>,
    /// Advertised capabilities.
    pub(crate) capabilities: AgentCapabilitiesDto,
    /// Default accepted input modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) default_input_modes: Vec<String>,
    /// Default produced output modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) default_output_modes: Vec<String>,
    /// Advertised skills (empty for now).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) skills: Vec<serde_json::Value>,
    /// Named security schemes (e.g. a `bearer` HTTP scheme). Absent when the
    /// server runs without auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) security_schemes: Option<serde_json::Value>,
    /// Security requirements: a list of `{ scheme-name: [scopes] }` maps. Empty
    /// when the server runs without auth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) security: Vec<serde_json::Value>,
    /// Optional provider descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<AgentProviderDto>,
}

/// A single transport interface advertised on the agent card.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentInterfaceDto {
    /// Endpoint URL for this transport.
    pub(crate) url: String,
    /// Transport name (e.g. `"GRPC"` / `"JSONRPC"`).
    pub(crate) transport: String,
}

/// Capability flags advertised on the agent card.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentCapabilitiesDto {
    /// Whether streaming is supported.
    pub(crate) streaming: bool,
    /// Whether push notifications are supported (`pushNotifications`).
    pub(crate) push_notifications: bool,
}

/// Provider (organization) descriptor on the agent card.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentProviderDto {
    /// Provider organization name.
    pub(crate) organization: String,
    /// Provider URL.
    pub(crate) url: String,
}

// ── JSON-RPC method params ─────────────────────────────────────────────────

/// Params for `message/send` and `message/stream`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageSendParams {
    /// The message to send.
    pub(crate) message: MessageDto,
    /// Optional send configuration (opaque pass-through for now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) configuration: Option<serde_json::Value>,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
}

/// Params for `tasks/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskQueryParams {
    /// Task id to fetch.
    pub(crate) id: String,
    /// Optional history truncation length (`historyLength`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) history_length: Option<u32>,
}

/// Params for `tasks/cancel` (and other id-only methods).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskIdParams {
    /// Task id.
    pub(crate) id: String,
    /// Optional provider-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
}

/// Params for `tasks/pushNotificationConfig/set`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskPushConfigParams {
    /// Task id (`taskId`).
    pub(crate) task_id: String,
    /// The push-notification configuration (`pushNotificationConfig`).
    pub(crate) push_notification_config: PushNotificationConfigDto,
}

/// Params for `tasks/pushNotificationConfig/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetTaskPushConfigParams {
    /// Task id.
    pub(crate) id: String,
    /// Optional specific config id (`pushNotificationConfigId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) push_notification_config_id: Option<String>,
}

/// Params for `tasks/pushNotificationConfig/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ListTaskPushConfigParams {
    /// Task id.
    pub(crate) id: String,
}

/// Params for `tasks/pushNotificationConfig/delete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteTaskPushConfigParams {
    /// Task id.
    pub(crate) id: String,
    /// Config id to delete (`pushNotificationConfigId`).
    pub(crate) push_notification_config_id: String,
}

/// A push-notification configuration on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PushNotificationConfigDto {
    /// Optional config id (server-assigned on create).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    /// Absolute webhook URL.
    pub(crate) url: String,
    /// Optional correlation token.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) token: String,
    /// Optional `Authorization` credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) authentication: Option<PushAuthDto>,
}

/// Webhook authentication on the wire — mirrors core `PushNotificationAuth`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PushAuthDto {
    /// HTTP authentication scheme name.
    pub(crate) scheme: String,
    /// Credential payload.
    pub(crate) credentials: String,
}

/// Result type for the push-config RPCs (`{taskId, pushNotificationConfig}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskPushNotificationConfigDto {
    /// Task id (`taskId`).
    pub(crate) task_id: String,
    /// The push-notification configuration (`pushNotificationConfig`).
    pub(crate) push_notification_config: PushNotificationConfigDto,
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn part_text_serializes_with_kind_text() {
        let part = PartDto::Text {
            text: "hello".to_owned(),
            metadata: None,
        };
        let value = serde_json::to_value(&part).expect("serialize must succeed");
        assert_eq!(value, json!({"kind": "text", "text": "hello"}));
    }

    #[test]
    fn part_file_with_bytes_serializes_with_kind_file() {
        let part = PartDto::File {
            file: FileContentDto {
                bytes: Some("aGk=".to_owned()),
                uri: None,
                mime_type: None,
            },
            metadata: None,
        };
        let value = serde_json::to_value(&part).expect("serialize must succeed");
        assert_eq!(value, json!({"kind": "file", "file": {"bytes": "aGk="}}));
    }

    #[test]
    fn part_data_serializes_with_kind_data() {
        let part = PartDto::Data {
            data: json!({"a": 1}),
            metadata: None,
        };
        let value = serde_json::to_value(&part).expect("serialize must succeed");
        assert_eq!(value, json!({"kind": "data", "data": {"a": 1}}));
    }

    #[test]
    fn file_content_mime_type_serializes_camel_case() {
        let file = FileContentDto {
            bytes: None,
            uri: Some("https://x/y".to_owned()),
            mime_type: Some("text/plain".to_owned()),
        };
        let value = serde_json::to_value(&file).expect("serialize must succeed");
        assert_eq!(
            value,
            json!({"uri": "https://x/y", "mimeType": "text/plain"})
        );
    }

    #[test]
    fn task_state_input_required_is_kebab_case() {
        let value =
            serde_json::to_value(TaskStateDto::InputRequired).expect("serialize must succeed");
        assert_eq!(value, json!("input-required"));
    }

    #[test]
    fn task_state_auth_required_is_kebab_case() {
        let value =
            serde_json::to_value(TaskStateDto::AuthRequired).expect("serialize must succeed");
        assert_eq!(value, json!("auth-required"));
    }

    #[test]
    fn message_serializes_message_id_and_kind() {
        let msg = MessageDto {
            message_id: "abc".to_owned(),
            role: RoleDto::User,
            parts: vec![],
            context_id: None,
            task_id: None,
            metadata: None,
            kind: message_kind(),
        };
        let value = serde_json::to_value(&msg).expect("serialize must succeed");
        assert_eq!(value["messageId"], json!("abc"));
        assert_eq!(value["kind"], json!("message"));
        assert_eq!(value["role"], json!("user"));
    }

    #[test]
    fn message_deserializes_with_default_kind() {
        let value = json!({"messageId": "abc", "role": "agent", "parts": []});
        let msg: MessageDto = serde_json::from_value(value).expect("deserialize must succeed");
        assert_eq!(msg.kind, "message", "kind must default to 'message'");
        assert_eq!(msg.role, RoleDto::Agent);
    }

    #[test]
    fn task_serializes_context_id_and_kind() {
        let task = TaskDto {
            id: "t1".to_owned(),
            context_id: "c1".to_owned(),
            status: TaskStatusDto {
                state: TaskStateDto::Working,
                message: None,
                timestamp: None,
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
            kind: task_kind(),
        };
        let value = serde_json::to_value(&task).expect("serialize must succeed");
        assert_eq!(value["contextId"], json!("c1"));
        assert_eq!(value["kind"], json!("task"));
        assert_eq!(value["status"]["state"], json!("working"));
    }
}
