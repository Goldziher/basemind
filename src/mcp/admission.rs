//! Bounded admission for allocation-heavy MCP calls.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock, ErrorCode, ErrorData};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const DEFAULT_HEAVY_CONCURRENCY: usize = 2;
const DEFAULT_QUEUE_WAIT: Duration = Duration::from_secs(2);
const DEFAULT_RETRY_AFTER_MS: u64 = 500;
const SERVER_BUSY_CODE: i32 = -32003;
const RETRY_AFTER_MS_KEY: &str = "retry_after_ms";

/// Request class used at the MCP dispatch boundary. Control calls deliberately never acquire a
/// heavy permit, so health, task status, and comms remain responsive while scans saturate the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkClass {
    Control,
    Heavy,
}

/// Admission result. Holding the heavy permit for the operation's lifetime bounds concurrency.
pub(crate) enum Admission {
    Control,
    Heavy { _permit: OwnedSemaphorePermit },
}

tokio::task_local! {
    /// The admission of the tool call running in this task, set by [`holding`]. A call's body runs
    /// on the blocking pool ([`super::adapter`]), which a dropped or aborted handler future does not
    /// stop, so the blocking closure takes a clone and the permit is released only when both the
    /// handler and the work it started have ended.
    static HELD: Arc<Admission>;
}

/// Run `work` as the call `admission` admitted: blocking work it starts keeps the permit alive
/// (see [`HELD`]).
pub(crate) async fn holding<F: std::future::Future>(admission: Admission, work: F) -> F::Output {
    HELD.scope(Arc::new(admission), work).await
}

/// The admission of the call running in this task, for blocking work that must hold its permit.
pub(crate) fn held() -> Option<Arc<Admission>> {
    HELD.try_with(Arc::clone).ok()
}

/// Process-scoped controller injected into every daemon-hosted MCP server connection.
pub(crate) struct HeavyAdmission {
    permits: Arc<Semaphore>,
    queue_wait: Duration,
}

impl Default for HeavyAdmission {
    fn default() -> Self {
        Self::new(DEFAULT_HEAVY_CONCURRENCY, DEFAULT_QUEUE_WAIT)
    }
}

impl HeavyAdmission {
    pub(crate) fn new(concurrency: usize, queue_wait: Duration) -> Self {
        assert!(concurrency > 0, "heavy-operation concurrency must be non-zero");
        Self {
            permits: Arc::new(Semaphore::new(concurrency)),
            queue_wait,
        }
    }

    pub(crate) async fn admit(&self, class: WorkClass) -> Result<Admission, ServerBusy> {
        if class == WorkClass::Control {
            return Ok(Admission::Control);
        }
        let permit = tokio::time::timeout(self.queue_wait, Arc::clone(&self.permits).acquire_owned())
            .await
            .map_err(|_| ServerBusy)?
            .map_err(|_| ServerBusy)?;
        Ok(Admission::Heavy { _permit: permit })
    }
}

/// Stable overload signal surfaced to MCP clients instead of letting their transport time out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerBusy;

/// The protocol-level form, for requests that have no tool result to carry it (a resource read).
impl From<ServerBusy> for ErrorData {
    fn from(_: ServerBusy) -> Self {
        ErrorData::new(
            ErrorCode(SERVER_BUSY_CODE),
            super::error_kind::ErrorKind::Busy.code(),
            Some(serde_json::json!({
                "retryable": true,
                RETRY_AFTER_MS_KEY: DEFAULT_RETRY_AFTER_MS
            })),
        )
    }
}

impl ServerBusy {
    /// The rejection as an `isError` tool result. Overload is something the calling agent fixes by
    /// retrying, so it is answered like every other fixable failure; a JSON-RPC error would reach
    /// the host, not the model.
    pub(crate) fn into_tool_result(self) -> CallToolResult {
        let mut result = CallToolResult::error(vec![ContentBlock::text(format!(
            "busy: basemind is running as many heavy calls as it allows and this one was not started. \
             Nothing was changed. Retry the same call in about {DEFAULT_RETRY_AFTER_MS} ms."
        ))]);
        let mut structured = super::error_kind::ErrorKind::Busy.structured();
        structured[RETRY_AFTER_MS_KEY] = serde_json::json!(DEFAULT_RETRY_AFTER_MS);
        result.structured_content = Some(structured);
        result
    }

    /// The rejection for a CLI session ([`BasemindServer::for_cli`](super::BasemindServer::for_cli)):
    /// a JSON-RPC error marked [`BUSY_DATA`](super::BUSY_DATA), which the CLI exits `3` for.
    pub(crate) fn into_cli_error(self) -> ErrorData {
        ErrorData::invalid_request(
            format!(
                "busy: basemind is running as many heavy calls as it allows and this one was not \
                 started. Nothing was changed. Retry in about {DEFAULT_RETRY_AFTER_MS} ms."
            ),
            Some(serde_json::json!({ super::BUSY_DATA: true })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heavy_work_is_bounded_and_excess_requests_fail_busy() {
        let admission = HeavyAdmission::new(2, Duration::from_millis(20));
        let first = admission.admit(WorkClass::Heavy).await.expect("first heavy permit");
        let second = admission.admit(WorkClass::Heavy).await.expect("second heavy permit");

        let result = match admission.admit(WorkClass::Heavy).await {
            Ok(_) => panic!("third heavy request must not be admitted"),
            Err(busy) => busy.into_tool_result(),
        };

        assert_eq!(
            result.is_error,
            Some(true),
            "a tool result the agent reads, not a protocol error"
        );
        let text = match result.content.first() {
            Some(ContentBlock::Text(text)) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert_eq!(
            text,
            format!(
                "busy: basemind is running as many heavy calls as it allows and this one was not started. \
                 Nothing was changed. Retry the same call in about {DEFAULT_RETRY_AFTER_MS} ms."
            )
        );
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({
                "error": "busy",
                "retryable": true,
                "retry_after_ms": DEFAULT_RETRY_AFTER_MS
            }))
        );
        drop((first, second));
    }

    #[test]
    fn every_form_of_the_overload_refusal_says_busy() {
        let protocol = ErrorData::from(ServerBusy);
        assert_eq!(protocol.code, ErrorCode(SERVER_BUSY_CODE));
        assert_eq!(protocol.message, "busy");
        assert_eq!(
            protocol.data,
            Some(serde_json::json!({ "retryable": true, "retry_after_ms": DEFAULT_RETRY_AFTER_MS }))
        );

        let cli = ServerBusy.into_cli_error();
        assert_eq!(cli.code, ErrorCode::INVALID_REQUEST);
        assert_eq!(
            cli.message,
            format!(
                "busy: basemind is running as many heavy calls as it allows and this one was not started. \
                 Nothing was changed. Retry in about {DEFAULT_RETRY_AFTER_MS} ms."
            )
        );
        assert_eq!(cli.data, Some(serde_json::json!({ "busy": true })));
    }

    #[tokio::test]
    async fn control_work_bypasses_saturated_heavy_lane() {
        let admission = HeavyAdmission::new(1, Duration::from_millis(20));
        let heavy = admission.admit(WorkClass::Heavy).await.expect("heavy permit");

        let control = admission.admit(WorkClass::Control).await.expect("control admission");

        assert!(matches!(control, Admission::Control));
        drop(heavy);
    }

    #[tokio::test]
    async fn released_heavy_permit_admits_the_next_request() {
        let admission = HeavyAdmission::new(1, Duration::from_secs(1));
        let first = admission.admit(WorkClass::Heavy).await.expect("first heavy permit");
        drop(first);

        let next = admission.admit(WorkClass::Heavy).await.expect("permit after release");

        assert!(matches!(next, Admission::Heavy { .. }));
    }
}
