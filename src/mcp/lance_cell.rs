//! A vector store a tool call opens on first use, and one that would not open, remembered for a
//! while.
//!
//! An open waits up to 30 seconds for another open of the same store to finish, and fails when a
//! holder stays past that (see `crate::lance`). Without the memory here every later call would
//! wait out the same stuck holder again before failing.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::ErrorData as McpError;

use crate::lance::LanceStore;

/// How long a failed open answers for the store: calls within this of the failure are told at
/// once that it is unavailable, and the first call after it opens the store again.
pub(super) const RETRY_AFTER: Duration = Duration::from_secs(60);

/// A lazily opened vector store.
#[derive(Default)]
pub(crate) struct LanceCell {
    store: tokio::sync::OnceCell<Arc<LanceStore>>,
    /// When the last open failed, and why.
    failed: std::sync::Mutex<Option<(Instant, String)>>,
}

impl LanceCell {
    /// The store, when an open has succeeded.
    #[cfg_attr(not(feature = "memory"), allow(dead_code, reason = "read only by the memory store"))]
    pub(crate) fn get(&self) -> Option<&Arc<LanceStore>> {
        self.store.get()
    }

    /// The store, opened by `open` unless it is open already. An open that failed less than
    /// [`RETRY_AFTER`] ago is not tried again: the call fails at once with its reason.
    pub(crate) async fn get_or_open<F, Fut>(&self, open: F) -> Result<Arc<LanceStore>, Unavailable>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<LanceStore>, String>>,
    {
        if let Some(store) = self.store.get() {
            return Ok(Arc::clone(store));
        }
        if let Some(recent) = self.recent_failure() {
            return Err(recent);
        }
        // Initialisers run one at a time, and a call queued behind one that fails runs its own
        // next: it looks again here, so calls that arrived during a failing open fail with it
        // instead of each waiting out the same holder in turn. Only an open that ran records a
        // failure, so a refusal does not extend the window.
        let opened = self
            .store
            .get_or_try_init(|| async {
                if let Some(recent) = self.recent_failure() {
                    return Err(recent);
                }
                open().await.map_err(|reason| {
                    *self.failed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((Instant::now(), reason.clone()));
                    Unavailable {
                        reason,
                        retry_in: RETRY_AFTER,
                    }
                })
            })
            .await;
        opened.map(Arc::clone)
    }

    /// The last open's failure, when it failed less than [`RETRY_AFTER`] ago.
    fn recent_failure(&self) -> Option<Unavailable> {
        let failed = self.failed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (at, reason) = failed.as_ref()?;
        let retry_in = RETRY_AFTER.checked_sub(at.elapsed()).filter(|left| !left.is_zero())?;
        Some(Unavailable {
            reason: reason.clone(),
            retry_in,
        })
    }
}

/// A call went without the store: its open failed, now or less than [`RETRY_AFTER`] ago.
#[derive(Debug)]
pub(crate) struct Unavailable {
    /// Why the open failed.
    reason: String,
    /// How long until a call opens the store again.
    retry_in: Duration,
}

impl Unavailable {
    /// `tool`'s refusal of the call, naming the cause and when to retry, then `meanwhile`: what
    /// the caller can do without the store. A refusal, not an internal error: nothing is broken in
    /// the tool, and the same call succeeds once the store opens.
    pub(crate) fn refusal(&self, tool: &str, meanwhile: Option<&str>) -> McpError {
        let mut message = format!("`{tool}`: {self}.");
        if let Some(meanwhile) = meanwhile {
            message.push(' ');
            message.push_str(meanwhile);
        }
        McpError::invalid_request(message, None)
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "vector store unavailable ({}). An open that fails is retried by the first call after {} s; \
             retry in about {} s",
            self.reason,
            RETRY_AFTER.as_secs(),
            self.retry_in.as_secs().max(1)
        )
    }
}

/// The refusal of a call made on no tool's behalf (a backfill step), named by no tool.
impl From<Unavailable> for McpError {
    fn from(unavailable: Unavailable) -> Self {
        McpError::invalid_request(unavailable.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Calls that arrive while an open is waiting out a stuck holder queue behind it. When it
    /// fails they fail with it: none of them opens again and waits out the holder in turn.
    #[tokio::test(start_paused = true)]
    async fn calls_queued_behind_a_failing_open_fail_with_it() {
        let cell = LanceCell::default();
        let opens = AtomicUsize::new(0);
        let call = || {
            cell.get_or_open(|| async {
                opens.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err("the open lock is held".to_string())
            })
        };
        let started = tokio::time::Instant::now();
        let answers = tokio::join!(call(), call(), call(), call());
        assert!(
            [answers.0, answers.1, answers.2, answers.3].iter().all(Result::is_err),
            "every call fails"
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1, "one open ran for all the calls");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(30),
            "no call waited past the one open"
        );
    }

    /// A call inside the window is refused at once, not opened again: the refusal names the tool,
    /// the store and the first failure's cause, says when to retry, and ends with what the caller
    /// can do meanwhile. It is a refusal, not an internal error.
    #[tokio::test]
    async fn a_call_inside_the_window_is_refused_by_name_with_when_to_retry() {
        let cell = LanceCell::default();
        let first = cell
            .get_or_open(|| async { Err("the open lock is held".to_string()) })
            .await;
        assert!(first.is_err());
        let refused = cell
            .get_or_open(|| async { Err("opened again".to_string()) })
            .await
            .err()
            .expect("refused inside the window");
        let error = refused.refusal("code_search", Some("Search with `mode: \"keyword\"` meanwhile."));
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_REQUEST);
        let message = error.message;
        assert!(
            message.starts_with("`code_search`: vector store unavailable (the open lock is held)"),
            "{message}"
        );
        assert!(message.contains("retry in about "), "{message}");
        assert!(
            message.ends_with("Search with `mode: \"keyword\"` meanwhile."),
            "{message}"
        );
    }
}
