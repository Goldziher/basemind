//! Headless agent-shell tool shims for `BasemindServer`.
//!
//! Each shim is a thin wrapper around its `run_*` helper in `helpers_shells.rs`,
//! with telemetry instrumentation matching the rest of the MCP surface. The
//! whole module is gated on `feature = "shells"` — when the feature is off these
//! tools are never registered, so the agent does not see them in the tool list.

#![cfg(feature = "shells")]

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::tool;
use serde_json::Value;

use super::BasemindServer;
use super::helpers::record_call;
use super::types_shells::{ShellCaptureParams, ShellKillParams, ShellSendParams, ShellSpawnParams};

#[rmcp::tool_router(vis = "pub(super)", router = "tool_router_shells")]
impl BasemindServer {
    #[tool(
        description = "Spawn a detached headless shell session backed by the embedded rmux daemon \
        and return a stable `session_id`. `command` runs through the login shell (e.g. \
        `bash -lc '<command>'`). Optional `cwd` (repo-relative), `env` (key/value list), and \
        `title`. The session is headless — no terminal is attached — drive it with `shell_send`, \
        read it with `shell_capture`, and end it with `shell_kill`. The response includes an \
        `attach_command` (`rmux attach -t <name>`) an operator can run to observe it. Needs \
        --features shells.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn shell_spawn(
        &self,
        Parameters(p): Parameters<ShellSpawnParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_shells::run_shell_spawn(&self.state, p).await;
        record_call(
            &self.state,
            "shell_spawn",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Write `text` to a headless shell session's stdin, addressed by `session_id` \
        (from `shell_spawn`). When `enter` is true (default) a trailing newline is appended so the \
        line executes; set `enter=false` to send a raw keystroke fragment. Use `shell_capture` \
        afterward to read the result. Needs --features shells.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn shell_send(
        &self,
        Parameters(p): Parameters<ShellSendParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_shells::run_shell_send(&self.state, p).await;
        record_call(
            &self.state,
            "shell_send",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Capture the currently-visible screen text of a headless shell session, \
        addressed by `session_id`. Returns the rendered pane with trailing blank lines trimmed; \
        pass `lines` to get only the last N non-blank rows (the most recent output). This is a \
        screen snapshot, not a full scrollback log. Needs --features shells.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub(crate) async fn shell_capture(
        &self,
        Parameters(p): Parameters<ShellCaptureParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_shells::run_shell_capture(&self.state, p).await;
        record_call(
            &self.state,
            "shell_capture",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }

    #[tool(
        description = "Kill a headless shell session by `session_id` and forget its mapping. \
        Returns `killed=true` when a live session was terminated, `false` when it was already \
        gone. Needs --features shells.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn shell_kill(
        &self,
        Parameters(p): Parameters<ShellKillParams>,
    ) -> Result<CallToolResult, McpError> {
        let __started = std::time::Instant::now();
        let __params_json = serde_json::to_value(&p).unwrap_or(Value::Null);
        let __result: Result<CallToolResult, McpError> =
            super::helpers_shells::run_shell_kill(&self.state, p).await;
        record_call(
            &self.state,
            "shell_kill",
            &__params_json,
            __started,
            &__result,
        );
        __result
    }
}
