//! Bounded admission for allocation-heavy MCP calls.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{ErrorCode, ErrorData};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const DEFAULT_HEAVY_CONCURRENCY: usize = 2;
const DEFAULT_QUEUE_WAIT: Duration = Duration::from_secs(2);
const DEFAULT_RETRY_AFTER_MS: u64 = 500;
const SERVER_BUSY_CODE: i32 = -32003;

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

impl From<ServerBusy> for ErrorData {
    fn from(_: ServerBusy) -> Self {
        ErrorData::new(
            ErrorCode(SERVER_BUSY_CODE),
            "server_busy",
            Some(serde_json::json!({
                "retryable": true,
                "retry_after_ms": DEFAULT_RETRY_AFTER_MS
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heavy_work_is_bounded_and_excess_requests_fail_with_server_busy() {
        let admission = HeavyAdmission::new(2, Duration::from_millis(20));
        let first = admission.admit(WorkClass::Heavy).await.expect("first heavy permit");
        let second = admission.admit(WorkClass::Heavy).await.expect("second heavy permit");

        let error = match admission.admit(WorkClass::Heavy).await {
            Ok(_) => panic!("third heavy request must not be admitted"),
            Err(error) => ErrorData::from(error),
        };

        assert_eq!(error.code, ErrorCode(SERVER_BUSY_CODE));
        assert_eq!(error.message, "server_busy");
        assert_eq!(
            error.data,
            Some(serde_json::json!({
                "retryable": true,
                "retry_after_ms": DEFAULT_RETRY_AFTER_MS
            }))
        );
        drop((first, second));
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
