//! The fixed codes the audit log and a model-facing reply use instead of free text, so that no
//! path, harness output or request field reaches either.

use crate::error::LaunchError;
use crate::event::{ApprovalKind, ApprovalSource, FinishStatus};

/// A stable code for `error`, without the paths, values or harness text its message carries.
#[must_use]
pub fn launch_error_code(error: &LaunchError) -> &'static str {
    match error {
        LaunchError::InvalidIdentity(_) => "invalid_identity",
        LaunchError::InvalidSubName(_) => "invalid_sub_name",
        LaunchError::InvalidHost(_) => "invalid_host",
        LaunchError::WorkingDirOutsideRoot { .. } => "working_dir_outside_root",
        LaunchError::WorkingDirNotRelative(_) => "working_dir_not_relative",
        LaunchError::NonUtf8Path(_) => "non_utf8_path",
        LaunchError::WorkingDirChanged(_) => "working_dir_changed",
        LaunchError::InvalidExecutable { .. } => "invalid_executable",
        LaunchError::Path { .. } => "path",
        LaunchError::BypassNotAuthorized { .. } => "bypass_not_authorized",
        LaunchError::BypassArgument(_) => "bypass_argument",
        LaunchError::ArgumentNotAllowed { .. } => "argument_not_allowed",
        LaunchError::UnverifiedMode { .. } => "unverified_mode",
        LaunchError::Unsupported { .. } => "unsupported",
        LaunchError::InvalidMcpServerName(_) => "invalid_mcp_server_name",
        LaunchError::InvalidMcpToolName(_) => "invalid_mcp_tool_name",
        LaunchError::EnvNotAllowed(_) => "env_not_allowed",
        LaunchError::ProjectConfig { .. } => "project_config",
        LaunchError::RootNotAllowed { .. } => "root_not_allowed",
        LaunchError::GitConfig { .. } => "git_config",
        LaunchError::InvalidLimits(_) => "invalid_limits",
        LaunchError::UnknownHarness(_) => "unknown_harness",
        LaunchError::Spawn { .. } => "spawn",
        LaunchError::Io(_) => "io",
        LaunchError::Protocol(_) => "protocol",
        LaunchError::PromptTooLarge(_) => "prompt_too_large",
        LaunchError::SlashCommandPrompt => "slash_command_prompt",
        LaunchError::TurnLimit(_) => "turn_limit",
        LaunchError::ControlQueueFull => "control_queue_full",
        LaunchError::SessionClosed => "session_closed",
    }
}

pub(crate) fn approval_kind(kind: ApprovalKind) -> &'static str {
    match kind {
        ApprovalKind::Command => "command",
        ApprovalKind::FileChange => "file_change",
        ApprovalKind::Permissions => "permissions",
        ApprovalKind::McpTool => "mcp_tool",
        ApprovalKind::Fetch => "fetch",
        ApprovalKind::Tool => "tool",
    }
}

pub(crate) fn approval_source(source: ApprovalSource) -> &'static str {
    match source {
        ApprovalSource::DefaultDeny => "default_deny",
        ApprovalSource::Policy => "policy",
        ApprovalSource::Caller => "caller",
        ApprovalSource::Withdrawn => "withdrawn",
        ApprovalSource::TooManyPending => "too_many_pending",
        ApprovalSource::GrantUnknown => "grant_unknown",
    }
}

pub(crate) fn finish_code(status: FinishStatus) -> &'static str {
    match status {
        FinishStatus::Completed => "completed",
        FinishStatus::Stopped => "stopped",
        FinishStatus::WallTimeExceeded => "wall_time_exceeded",
        FinishStatus::TurnLimitExceeded => "turn_limit_exceeded",
        FinishStatus::Failed => "failed",
    }
}
