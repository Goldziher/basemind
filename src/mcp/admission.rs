//! Bounded admission for expensive MCP calls: a lane of slots per bounded work class.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock, ErrorCode, ErrorData};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::daemon::AdmissionLimits;
use crate::engine::types::index::{AdmissionStatus, LaneStatus};

const DEFAULT_RETRY_AFTER_MS: u64 = 500;
/// How long a cancel-aware heavy read may run before it is stopped and its lane permit given up.
/// Under the stdio relay's 180 s request deadline, so the daemon answers before the relay gives up.
const DEFAULT_HEAVY_DEADLINE: Duration = Duration::from_secs(150);
const SERVER_BUSY_CODE: i32 = -32003;
const RETRY_AFTER_MS_KEY: &str = "retry_after_ms";

/// Calls that read the whole corpus or the whole import graph, admitted as [`WorkClass::Medium`].
/// Keyed by tool name; the CLI-only analyses are answered under their command names.
const MEDIUM_CALLS: &[&str] = &["code_grep", "code_impact", "code_cycles", "code_deadcode", "overview"];

/// The `code_refs` relations answered from the whole import graph.
const GRAPH_RELATIONS: &[&str] = &["importers", "cycles"];

/// Request class used at the MCP dispatch boundary. Control calls deliberately never acquire a
/// slot, so health, task status, and comms remain responsive while scans saturate the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkClass {
    Control,
    /// A whole-corpus read: bounded on its own lane, so it neither waits behind a rescan nor lets
    /// thirty sessions' greps contend for the daemon's few rayon threads at once.
    Medium,
    Heavy,
}

impl WorkClass {
    /// The class of the call `name` with `arguments`: heavy for a call that can become a task
    /// ([`super::tasks::SLOW_CALLS`]), medium for a whole-corpus read, control otherwise.
    pub(crate) fn of(name: &str, arguments: Option<&serde_json::Map<String, serde_json::Value>>) -> Self {
        if super::tasks::is_slow_tool(name) {
            return Self::Heavy;
        }
        let graph_relation = name == "code_refs"
            && arguments
                .and_then(|args| args.get("relation"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|relation| GRAPH_RELATIONS.contains(&relation));
        if graph_relation || MEDIUM_CALLS.contains(&name) {
            return Self::Medium;
        }
        Self::Control
    }
}

/// Admission result. Holding the permit for the operation's lifetime bounds concurrency.
pub(crate) enum Admission {
    Control,
    Medium(Claim),
    Heavy(Claim),
}

impl Admission {
    /// Give up the lane for work the caller stopped waiting on (deadline passed, request
    /// cancelled). The work may still be running on the blocking pool, so its lane permit is
    /// exchanged for an overflow slot: the lane admits the next call, and abandoned work stays
    /// bounded by the overflow pool. With the pool full the lane permit is kept.
    pub(crate) fn abandon(&self) {
        if let Self::Medium(claim) | Self::Heavy(claim) = self {
            claim.abandon();
        }
    }
}

/// An admitted call's permit: a lane permit until [`Admission::abandon`], then an overflow slot.
pub(crate) struct Claim {
    state: Mutex<ClaimState>,
}

struct ClaimState {
    permit: OwnedSemaphorePermit,
    /// The pool to move to on abandonment; `None` once the claim holds an overflow slot.
    overflow: Option<Arc<Semaphore>>,
}

impl Claim {
    fn abandon(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(overflow) = state.overflow.take() else {
            return;
        };
        match Arc::clone(&overflow).try_acquire_owned() {
            Ok(slot) => drop(std::mem::replace(&mut state.permit, slot)),
            Err(_) => state.overflow = Some(overflow),
        }
    }
}

tokio::task_local! {
    /// The admission of the tool call running in this task, set by [`holding`]. A call's body runs
    /// on the blocking pool ([`super::adapter`]), which a dropped or aborted handler future does not
    /// stop, so the blocking closure takes a clone and the permit is released only when both the
    /// handler and the work it started have ended, or moved to the overflow pool when the call is
    /// abandoned ([`Admission::abandon`]).
    static HELD: Arc<Admission>;
}

/// Run `work` as the call `admission` admitted: blocking work it starts keeps the permit alive
/// (see [`HELD`]).
pub(crate) async fn holding<F: std::future::Future>(admission: Arc<Admission>, work: F) -> F::Output {
    HELD.scope(admission, work).await
}

/// The admission of the call running in this task, for blocking work that must hold its permit.
pub(crate) fn held() -> Option<Arc<Admission>> {
    HELD.try_with(Arc::clone).ok()
}

/// The slots of one work class, and how many callers are waiting for one.
struct Lane {
    permits: Arc<Semaphore>,
    /// Slots for abandoned work still running ([`Admission::abandon`]), as many as the lane.
    overflow: Arc<Semaphore>,
    limit: usize,
    queued: AtomicUsize,
}

/// One caller counted in [`Lane::queued`] while it waits, however the wait ends.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn enter(queued: &'a AtomicUsize) -> Self {
        queued.fetch_add(1, Ordering::Relaxed);
        Self(queued)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Lane {
    fn new(limit: usize) -> Self {
        assert!(limit > 0, "admission concurrency must be non-zero");
        let limit = limit.min(Semaphore::MAX_PERMITS);
        Self {
            permits: Arc::new(Semaphore::new(limit)),
            overflow: Arc::new(Semaphore::new(limit)),
            limit,
            queued: AtomicUsize::new(0),
        }
    }

    /// A slot, waiting at most `wait` for one when all are taken.
    async fn acquire(&self, wait: Duration) -> Result<Claim, ServerBusy> {
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let _waiting = Waiting::enter(&self.queued);
                tokio::time::timeout(wait, Arc::clone(&self.permits).acquire_owned())
                    .await
                    .map_err(|_| ServerBusy)?
                    .map_err(|_| ServerBusy)?
            }
        };
        Ok(Claim {
            state: Mutex::new(ClaimState {
                permit,
                overflow: Some(Arc::clone(&self.overflow)),
            }),
        })
    }

    fn status(&self) -> LaneStatus {
        LaneStatus {
            limit: self.limit,
            in_use: self.limit.saturating_sub(self.permits.available_permits()),
            queued: self.queued.load(Ordering::Relaxed),
        }
    }
}

/// Process-scoped controller injected into every daemon-hosted MCP server connection: one lane per
/// bounded work class.
pub(crate) struct HeavyAdmission {
    heavy: Lane,
    medium: Lane,
    queue_wait: Duration,
    deadline: Duration,
}

/// The installed engine configuration's limits ([`AdmissionLimits`]).
impl Default for HeavyAdmission {
    fn default() -> Self {
        Self::from_limits(crate::config::engine::with(|engine| engine.admission))
    }
}

impl HeavyAdmission {
    pub(crate) fn new(heavy: usize, medium: usize, queue_wait: Duration) -> Self {
        Self {
            heavy: Lane::new(heavy),
            medium: Lane::new(medium),
            queue_wait,
            deadline: DEFAULT_HEAVY_DEADLINE,
        }
    }

    /// This controller with cancel-aware heavy reads stopped after `deadline`.
    #[cfg(test)]
    pub(crate) fn with_deadline(self, deadline: Duration) -> Self {
        Self { deadline, ..self }
    }

    /// How long a cancel-aware heavy read may run before it is stopped and abandoned.
    pub(crate) fn deadline(&self) -> Duration {
        self.deadline
    }

    pub(crate) fn from_limits(limits: AdmissionLimits) -> Self {
        Self::new(limits.heavy_concurrency, limits.medium_concurrency, limits.queue_wait)
    }

    pub(crate) async fn admit(&self, class: WorkClass) -> Result<Admission, ServerBusy> {
        match class {
            WorkClass::Control => Ok(Admission::Control),
            WorkClass::Medium => Ok(Admission::Medium(self.medium.acquire(self.queue_wait).await?)),
            WorkClass::Heavy => Ok(Admission::Heavy(self.heavy.acquire(self.queue_wait).await?)),
        }
    }

    /// The lanes as `index_status` reports them.
    pub(crate) fn status(&self) -> AdmissionStatus {
        AdmissionStatus {
            heavy: self.heavy.status(),
            medium: self.medium.status(),
            queue_wait_ms: u64::try_from(self.queue_wait.as_millis()).unwrap_or(u64::MAX),
        }
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
            crate::engine::error_kind::ErrorKind::Busy.code(),
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
            "busy: basemind is running as many calls of this kind as it allows and this one was not started. \
             Nothing was changed. Retry the same call in about {DEFAULT_RETRY_AFTER_MS} ms."
        ))]);
        let mut structured = super::tool_contract::kind_structured(crate::engine::error_kind::ErrorKind::Busy);
        structured[RETRY_AFTER_MS_KEY] = serde_json::json!(DEFAULT_RETRY_AFTER_MS);
        result.structured_content = Some(structured);
        result
    }

    /// The rejection for a CLI session ([`BasemindServer::for_cli`](super::BasemindServer::for_cli)):
    /// a JSON-RPC error marked [`BUSY_DATA`](super::BUSY_DATA), which the CLI exits `3` for.
    pub(crate) fn into_cli_error(self) -> ErrorData {
        ErrorData::invalid_request(
            format!(
                "busy: basemind is running as many calls of this kind as it allows and this one was not \
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
        let admission = HeavyAdmission::new(2, 2, Duration::from_millis(20));
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
                "busy: basemind is running as many calls of this kind as it allows and this one was not started. \
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
                "busy: basemind is running as many calls of this kind as it allows and this one was not started. \
                 Nothing was changed. Retry in about {DEFAULT_RETRY_AFTER_MS} ms."
            )
        );
        assert_eq!(cli.data, Some(serde_json::json!({ "busy": true })));
    }

    #[tokio::test]
    async fn control_work_bypasses_saturated_heavy_lane() {
        let admission = HeavyAdmission::new(1, 1, Duration::from_millis(20));
        let heavy = admission.admit(WorkClass::Heavy).await.expect("heavy permit");

        let control = admission.admit(WorkClass::Control).await.expect("control admission");

        assert!(matches!(control, Admission::Control));
        drop(heavy);
    }

    #[tokio::test]
    async fn released_heavy_permit_admits_the_next_request() {
        let admission = HeavyAdmission::new(1, 1, Duration::from_secs(1));
        let first = admission.admit(WorkClass::Heavy).await.expect("first heavy permit");
        drop(first);

        let next = admission.admit(WorkClass::Heavy).await.expect("permit after release");

        assert!(matches!(next, Admission::Heavy(_)));
    }

    #[tokio::test]
    async fn a_configured_limit_admits_that_many_and_index_status_reports_them() {
        let admission = HeavyAdmission::from_limits(crate::config::daemon::AdmissionLimits {
            heavy_concurrency: 3,
            queue_wait: Duration::from_millis(20),
            ..crate::config::daemon::AdmissionLimits::from_vars(|_| None)
        });
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(
                admission
                    .admit(WorkClass::Heavy)
                    .await
                    .expect("one of the three configured slots"),
            );
        }
        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "a fourth waits and is refused"
        );
        let status = admission.status();
        assert_eq!(
            status.heavy,
            LaneStatus {
                limit: 3,
                in_use: 3,
                queued: 0
            }
        );
        assert_eq!(status.medium.in_use, 0, "a heavy call takes no medium slot");
        drop(held);
        assert_eq!(admission.status().heavy.in_use, 0);
    }

    /// A command that serves nobody else reports the lanes an idle controller reports.
    #[test]
    fn the_idle_status_is_what_a_fresh_controller_reports() {
        let limits = crate::config::engine::with(|engine| engine.admission);
        assert_eq!(AdmissionStatus::idle(limits), HeavyAdmission::default().status());
    }

    #[tokio::test]
    async fn a_waiting_call_is_reported_queued_until_it_is_admitted() {
        let admission = Arc::new(HeavyAdmission::new(1, 1, Duration::from_secs(5)));
        let first = admission.admit(WorkClass::Medium).await.expect("the one medium slot");
        let waiter = tokio::spawn({
            let admission = Arc::clone(&admission);
            async move { admission.admit(WorkClass::Medium).await.is_ok() }
        });
        while admission.status().medium.queued == 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            admission.status().medium,
            LaneStatus {
                limit: 1,
                in_use: 1,
                queued: 1
            }
        );
        drop(first);
        assert!(waiter.await.expect("waiter task"), "the waiter gets the released slot");
        assert_eq!(admission.status().medium.queued, 0);
        assert_eq!(admission.status().queue_wait_ms, 5_000);
    }

    #[tokio::test]
    async fn the_medium_lane_is_independent_of_a_saturated_heavy_lane() {
        let admission = HeavyAdmission::new(1, 1, Duration::from_millis(20));
        let heavy = admission.admit(WorkClass::Heavy).await.expect("heavy permit");
        let medium = admission.admit(WorkClass::Medium).await.expect("medium permit");
        assert!(matches!(medium, Admission::Medium(_)));
        assert!(
            admission.admit(WorkClass::Medium).await.is_err(),
            "a second medium call waits for the one medium slot, not a heavy one"
        );
        drop((heavy, medium));
    }

    #[test]
    fn whole_corpus_reads_are_medium_and_the_rest_keep_their_class() {
        let args = |json: serde_json::Value| json.as_object().cloned();
        let class = |name: &str, json: serde_json::Value| WorkClass::of(name, args(json).as_ref());
        assert_eq!(
            class("code_grep", serde_json::json!({"pattern": "x"})),
            WorkClass::Medium
        );
        for relation in ["importers", "cycles"] {
            assert_eq!(
                class("code_refs", serde_json::json!({"relation": relation})),
                WorkClass::Medium,
                "{relation}"
            );
        }
        assert_eq!(
            class("code_refs", serde_json::json!({"name": "f", "relation": "callers"})),
            WorkClass::Control
        );
        assert_eq!(class("code_refs", serde_json::json!({"name": "f"})), WorkClass::Control);
        for name in ["code_impact", "code_cycles", "code_deadcode", "overview"] {
            assert_eq!(WorkClass::of(name, None), WorkClass::Medium, "{name}");
        }
        assert_eq!(WorkClass::of("index_rescan", None), WorkClass::Heavy);
        assert_eq!(WorkClass::of("index_status", None), WorkClass::Control);
    }

    /// Abandoned work moves to the overflow pool, which is as large as the lane: past that, an
    /// abandoned claim keeps its lane permit, so hung work cannot pile up without bound.
    #[tokio::test]
    async fn abandoning_frees_the_lane_until_the_overflow_pool_is_full() {
        let admission = HeavyAdmission::new(1, 1, Duration::from_millis(20));
        let first = admission.admit(WorkClass::Heavy).await.expect("first heavy permit");
        first.abandon();
        first.abandon();
        let second = admission
            .admit(WorkClass::Heavy)
            .await
            .expect("the abandoned call no longer holds the lane");
        second.abandon();

        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "the overflow pool is full, so the second call kept its lane permit"
        );
        drop(first);
        assert!(
            admission.admit(WorkClass::Heavy).await.is_err(),
            "a freed overflow slot does not release a lane permit already kept"
        );
        drop(second);
        admission
            .admit(WorkClass::Heavy)
            .await
            .expect("the lane is free once the kept permit is dropped");
    }
}
