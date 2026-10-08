//! Server→client progress notifications (`notifications/progress`).
//!
//! Best-effort and fire-and-forget — a delivery error is logged via `tracing` and swallowed so a
//! chatty client connection can never fail a tool call. MCP logging (SEP-2577, deprecated) is
//! intentionally not advertised or emitted.

use rmcp::Peer;
use rmcp::RoleServer;
use rmcp::model::ProgressNotificationParam;

/// Emit a progress notification for `token`. Best-effort; only called when the client supplied a
/// progress token on the request.
pub(super) async fn emit_progress(
    peer: &Peer<RoleServer>,
    token: rmcp::model::ProgressToken,
    progress: f64,
    total: Option<f64>,
    message: impl Into<String>,
) {
    let message: String = message.into();
    // Inside a task-offloaded call, mirror the message onto the task's `statusMessage` so a client
    // polling `tasks/get` sees what a progress-token subscriber sees.
    let _ = super::tasks::CURRENT_TASK.try_with(|task| task.set_status_message(message.clone()));
    // `ProgressNotificationParam` is #[non_exhaustive] in rmcp 3.x; build it via the constructor.
    let mut param = ProgressNotificationParam::new(token, progress).with_message(message);
    if let Some(total) = total {
        param = param.with_total(total);
    }
    if let Err(error) = peer.notify_progress(param).await {
        tracing::debug!(?error, "progress notification dropped");
    }
}
