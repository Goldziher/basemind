//! Task types for the A2A task system (ADR-012).
//!
//! This module defines the core types for the task state machine, messages,
//! artifacts, and task metadata. It contains no I/O, managers, or transport
//! concerns — those live elsewhere.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::a2a::core::types::{AgentId, MessageId};

// ---------------------------------------------------------------------------
// ID newtypes
// ---------------------------------------------------------------------------

/// Unique identifier for a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(Uuid);

impl TaskId {
    /// Create a new random [`TaskId`].
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for TaskId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// Unique identifier for a task execution context (session / conversation
/// thread from the A2A perspective).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextId(Uuid);

impl ContextId {
    /// Create a new random [`ContextId`].
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ContextId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ContextId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ContextId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// Unique identifier for a task artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(Uuid);

impl ArtifactId {
    /// Create a new random [`ArtifactId`].
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ArtifactId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

// ---------------------------------------------------------------------------
// Task state machine
// ---------------------------------------------------------------------------

/// Lifecycle state of a task.
///
/// Valid transitions are enforced by [`TaskState::can_transition_to`].
///
/// ```text
/// Submitted ──► Working ──► Completed
///           │           └──► Failed
///           │           └──► Canceled
///           │           └──► InputRequired ──► Working
///           │                              └──► Canceled
///           │                              └──► Failed
///           │           └──► AuthRequired  ──► Working
///           │                              └──► Canceled
///           │                              └──► Failed
///           └──► Rejected
///           └──► Canceled
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Task has been submitted and is awaiting agent pickup.
    Submitted,
    /// An agent is actively working on the task.
    Working,
    /// Task finished successfully.
    Completed,
    /// Task ended with an unrecoverable error.
    Failed,
    /// Task was canceled before or during execution.
    Canceled,
    /// Execution is paused; the agent needs more input from the user.
    InputRequired,
    /// Execution is paused; the agent needs the user to complete an auth flow.
    AuthRequired,
    /// Task was rejected before any work started (e.g. validation failure).
    Rejected,
}

impl TaskState {
    /// Returns `true` if the state is a terminal state (no further transitions
    /// are possible).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        )
    }

    /// Returns `true` if transitioning from `self` to `target` is a valid
    /// state-machine step.
    pub fn can_transition_to(self, target: TaskState) -> bool {
        match self {
            TaskState::Submitted => matches!(
                target,
                TaskState::Working | TaskState::Rejected | TaskState::Canceled
            ),
            TaskState::Working => matches!(
                target,
                TaskState::Completed
                    | TaskState::Failed
                    | TaskState::Canceled
                    | TaskState::InputRequired
                    | TaskState::AuthRequired
            ),
            TaskState::InputRequired => matches!(
                target,
                TaskState::Working | TaskState::Canceled | TaskState::Failed
            ),
            TaskState::AuthRequired => matches!(
                target,
                TaskState::Working | TaskState::Canceled | TaskState::Failed
            ),
            // Terminal states have no valid outgoing transitions.
            TaskState::Completed
            | TaskState::Failed
            | TaskState::Canceled
            | TaskState::Rejected => false,
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Submitted => "submitted",
            Self::Working => "working",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::InputRequired => "input_required",
            Self::AuthRequired => "auth_required",
            Self::Rejected => "rejected",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for TaskState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "submitted" => Ok(Self::Submitted),
            "working" => Ok(Self::Working),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            "input_required" => Ok(Self::InputRequired),
            "rejected" => Ok(Self::Rejected),
            "auth_required" => Ok(Self::AuthRequired),
            _ => Err(format!("unknown task state: '{s}'")),
        }
    }
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Who authored a [`TaskMessage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// The message was authored by the human user.
    User,
    /// The message was authored by an AI agent.
    Agent,
}

/// A single content part within a [`TaskMessage`] or [`Artifact`].
///
/// The `type` field is used as the serde tag so the JSON representation is
/// self-describing: `{"type":"text","text":"hello"}` and
/// `{"type":"bytes","bytes":"<base64>"}` for binary payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Plain UTF-8 text content.
    Text { text: String },
    /// A URL reference to an external resource.
    Url { url: String },
    /// Arbitrary structured JSON data.
    Data { data: serde_json::Value },
    /// Raw binary content. Encoded as base64 in JSON so the wire form remains
    /// valid UTF-8; transferred as native bytes over gRPC.
    Bytes {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
}

/// Serde codec that round-trips `Vec<u8>` through standard base64.
///
/// Used by [`Part::Bytes`] so the JSON form is `{"bytes":"<base64>"}` rather
/// than a JSON array of integers (which would balloon the payload size).
mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

/// A single message within a task's conversation history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskMessage {
    /// Stable identity for this message.
    pub id: MessageId,
    /// Who sent this message.
    pub role: MessageRole,
    /// Ordered list of content parts that make up this message.
    pub parts: Vec<Part>,
    /// Optional provider-specific metadata.
    pub metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Task status
// ---------------------------------------------------------------------------

/// A snapshot of a task's current state, optionally accompanied by an
/// explanatory message from the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskStatus {
    /// The task's current lifecycle state.
    pub state: TaskState,
    /// An optional message providing context for this status (e.g. an error
    /// explanation or a clarifying question when `state` is
    /// [`TaskState::InputRequired`]).
    pub message: Option<TaskMessage>,
    /// Wall-clock time at which this status was recorded.
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// A named output produced by an agent as part of completing a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Stable identity for this artifact.
    pub id: ArtifactId,
    /// Optional human-readable name (e.g. `"patch.diff"`).
    pub name: Option<String>,
    /// Optional description of what the artifact contains.
    pub description: Option<String>,
    /// Ordered content parts that compose this artifact.
    pub parts: Vec<Part>,
    /// Optional provider-specific metadata.
    pub metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

/// A unit of work dispatched through the agent nexus.
///
/// A task moves through a well-defined state machine (see [`TaskState`]) and
/// accumulates [`Artifact`]s and a message [`history`](Task::history) as it
/// progresses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// Stable identity for this task.
    pub id: TaskId,
    /// The execution context this task belongs to (analogous to a session).
    pub context_id: ContextId,
    /// Current status of the task.
    pub status: TaskStatus,
    /// Artifacts produced so far (may be empty for in-progress tasks).
    pub artifacts: Vec<Artifact>,
    /// Full conversation history for this task.
    pub history: Vec<TaskMessage>,
    /// Optional caller-supplied metadata.
    pub metadata: Option<serde_json::Value>,
    /// The agent currently responsible for executing this task.
    pub assignee: Option<AgentId>,
    /// The agent (or system) that created this task.
    pub creator: Option<AgentId>,
    /// When set, the task watchdog will fail the task if it is still in a
    /// non-terminal state past this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<chrono::DateTime<chrono::Utc>>,
}

// ---------------------------------------------------------------------------
// TaskFilter
// ---------------------------------------------------------------------------

/// Predicate used when listing or querying tasks.
///
/// All fields are optional; a `Default` filter matches every task.
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    /// Restrict results to tasks in a specific context.
    pub context_id: Option<ContextId>,
    /// Restrict results to tasks in a specific state.
    pub state: Option<TaskState>,
    /// Restrict results to tasks assigned to a specific agent.
    pub assignee: Option<AgentId>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_new_produces_unique_ids() {
        let a = TaskId::new();
        let b = TaskId::new();
        assert_ne!(a, b, "two freshly generated TaskIds must not be equal");
    }

    #[test]
    fn context_id_new_produces_unique_ids() {
        let a = ContextId::new();
        let b = ContextId::new();
        assert_ne!(a, b, "two freshly generated ContextIds must not be equal");
    }

    #[test]
    fn task_state_is_terminal() {
        assert!(
            TaskState::Completed.is_terminal(),
            "Completed must be terminal"
        );
        assert!(TaskState::Failed.is_terminal(), "Failed must be terminal");
        assert!(
            TaskState::Canceled.is_terminal(),
            "Canceled must be terminal"
        );
        assert!(
            TaskState::Rejected.is_terminal(),
            "Rejected must be terminal"
        );

        assert!(
            !TaskState::Submitted.is_terminal(),
            "Submitted must not be terminal"
        );
        assert!(
            !TaskState::Working.is_terminal(),
            "Working must not be terminal"
        );
        assert!(
            !TaskState::InputRequired.is_terminal(),
            "InputRequired must not be terminal"
        );
        assert!(
            !TaskState::AuthRequired.is_terminal(),
            "AuthRequired must not be terminal"
        );
    }

    #[test]
    fn task_state_valid_transitions() {
        assert!(
            TaskState::Submitted.can_transition_to(TaskState::Working),
            "Submitted → Working must be valid"
        );
        assert!(
            TaskState::Submitted.can_transition_to(TaskState::Rejected),
            "Submitted → Rejected must be valid"
        );
        assert!(
            TaskState::Submitted.can_transition_to(TaskState::Canceled),
            "Submitted → Canceled must be valid"
        );

        assert!(
            TaskState::Working.can_transition_to(TaskState::Completed),
            "Working → Completed must be valid"
        );
        assert!(
            TaskState::Working.can_transition_to(TaskState::Failed),
            "Working → Failed must be valid"
        );
        assert!(
            TaskState::Working.can_transition_to(TaskState::Canceled),
            "Working → Canceled must be valid"
        );
        assert!(
            TaskState::Working.can_transition_to(TaskState::InputRequired),
            "Working → InputRequired must be valid"
        );
        assert!(
            TaskState::Working.can_transition_to(TaskState::AuthRequired),
            "Working → AuthRequired must be valid"
        );

        assert!(
            TaskState::InputRequired.can_transition_to(TaskState::Working),
            "InputRequired → Working must be valid"
        );
        assert!(
            TaskState::InputRequired.can_transition_to(TaskState::Canceled),
            "InputRequired → Canceled must be valid"
        );
        assert!(
            TaskState::InputRequired.can_transition_to(TaskState::Failed),
            "InputRequired → Failed must be valid"
        );

        assert!(
            TaskState::AuthRequired.can_transition_to(TaskState::Working),
            "AuthRequired → Working must be valid"
        );
        assert!(
            TaskState::AuthRequired.can_transition_to(TaskState::Canceled),
            "AuthRequired → Canceled must be valid"
        );
        assert!(
            TaskState::AuthRequired.can_transition_to(TaskState::Failed),
            "AuthRequired → Failed must be valid"
        );
    }

    #[test]
    fn task_state_invalid_transitions() {
        assert!(
            !TaskState::Completed.can_transition_to(TaskState::Working),
            "Completed → Working must be invalid"
        );
        assert!(
            !TaskState::Submitted.can_transition_to(TaskState::Completed),
            "Submitted → Completed must be invalid"
        );
        assert!(
            !TaskState::Failed.can_transition_to(TaskState::Working),
            "Failed → Working must be invalid"
        );
        assert!(
            !TaskState::Rejected.can_transition_to(TaskState::Working),
            "Rejected → Working must be invalid"
        );
        assert!(
            !TaskState::Canceled.can_transition_to(TaskState::Completed),
            "Canceled → Completed must be invalid"
        );
        assert!(
            !TaskState::Working.can_transition_to(TaskState::Submitted),
            "Working → Submitted must be invalid"
        );
    }

    #[test]
    fn task_round_trips_through_json() {
        let task = Task {
            id: TaskId::new(),
            context_id: ContextId::new(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(TaskMessage {
                    id: MessageId::new(),
                    role: MessageRole::Agent,
                    parts: vec![Part::Text {
                        text: "processing".to_owned(),
                    }],
                    metadata: None,
                }),
                timestamp: Utc::now(),
            },
            artifacts: vec![Artifact {
                id: ArtifactId::new(),
                name: Some("result.txt".to_owned()),
                description: Some("the output".to_owned()),
                parts: vec![Part::Data {
                    data: serde_json::json!({"key": "value"}),
                }],
                metadata: None,
            }],
            history: vec![TaskMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                parts: vec![Part::Url {
                    url: "https://example.com".to_owned(),
                }],
                metadata: Some(serde_json::json!({"source": "cli"})),
            }],
            metadata: Some(serde_json::json!({"priority": 1})),
            assignee: Some(AgentId::new()),
            creator: Some(AgentId::new()),
            deadline: None,
        };

        let json = serde_json::to_string(&task).expect("serialization must succeed");
        let recovered: Task = serde_json::from_str(&json).expect("deserialization must succeed");

        assert_eq!(task.id, recovered.id, "id must survive round-trip");
        assert_eq!(
            task.context_id, recovered.context_id,
            "context_id must survive round-trip"
        );
        assert_eq!(
            task.status.state, recovered.status.state,
            "status.state must survive round-trip"
        );
        assert_eq!(
            task.artifacts.len(),
            recovered.artifacts.len(),
            "artifacts length must survive round-trip"
        );
        assert_eq!(
            task.artifacts[0].id, recovered.artifacts[0].id,
            "artifact id must survive round-trip"
        );
        assert_eq!(
            task.history.len(),
            recovered.history.len(),
            "history length must survive round-trip"
        );
        assert_eq!(
            task.assignee, recovered.assignee,
            "assignee must survive round-trip"
        );
        assert_eq!(
            task.creator, recovered.creator,
            "creator must survive round-trip"
        );
    }

    #[test]
    fn task_state_serializes_as_snake_case() {
        let cases = [
            (TaskState::Submitted, "\"submitted\""),
            (TaskState::Working, "\"working\""),
            (TaskState::Completed, "\"completed\""),
            (TaskState::Failed, "\"failed\""),
            (TaskState::Canceled, "\"canceled\""),
            (TaskState::InputRequired, "\"input_required\""),
            (TaskState::AuthRequired, "\"auth_required\""),
            (TaskState::Rejected, "\"rejected\""),
        ];

        for (state, expected) in cases {
            let actual = serde_json::to_string(&state).expect("serialization must succeed");
            assert_eq!(
                actual, expected,
                "TaskState::{state:?} must serialize as {expected}"
            );
        }
    }

    #[test]
    fn task_state_from_str_round_trips() {
        let cases = [
            ("submitted", TaskState::Submitted),
            ("working", TaskState::Working),
            ("completed", TaskState::Completed),
            ("failed", TaskState::Failed),
            ("canceled", TaskState::Canceled),
            ("input_required", TaskState::InputRequired),
            ("rejected", TaskState::Rejected),
            ("auth_required", TaskState::AuthRequired),
        ];
        for (s, expected) in cases {
            let parsed: TaskState = s.parse().expect("must parse known variant");
            assert_eq!(parsed, expected, "round-trip failed for '{s}'");
        }
    }

    #[test]
    fn task_state_from_str_invalid_returns_error() {
        assert!(
            "unknown".parse::<TaskState>().is_err(),
            "unknown state string must return Err"
        );
    }
}
