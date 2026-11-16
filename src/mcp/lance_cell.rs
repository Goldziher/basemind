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
    pub(crate) async fn get_or_open<F, Fut>(&self, open: F) -> Result<Arc<LanceStore>, McpError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<LanceStore>, String>>,
    {
        if let Some(store) = self.store.get() {
            return Ok(Arc::clone(store));
        }
        if let Some(reason) = self.recent_failure() {
            return Err(unavailable(&reason));
        }
        // Initialisers run one at a time, and a call queued behind one that fails runs its own
        // next: it looks again here, so calls that arrived during a failing open fail with it
        // instead of each waiting out the same holder in turn. Only an open that ran records a
        // failure, so a refusal does not extend the window.
        let opened = self
            .store
            .get_or_try_init(|| async {
                if let Some(reason) = self.recent_failure() {
                    return Err(reason);
                }
                open().await.inspect_err(|reason| {
                    *self.failed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((Instant::now(), reason.clone()));
                })
            })
            .await;
        opened.map(Arc::clone).map_err(|reason| unavailable(&reason))
    }

    /// Why the last open failed, when it failed less than [`RETRY_AFTER`] ago.
    fn recent_failure(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < RETRY_AFTER)
            .map(|(_, reason)| reason.clone())
    }
}

/// The error of a call that needed the store: it names the store unavailable, not the call broken.
fn unavailable(reason: &str) -> McpError {
    McpError::internal_error(
        format!(
            "the vector store is unavailable ({reason}); it is opened again {} s after an open fails",
            RETRY_AFTER.as_secs()
        ),
        None,
    )
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
}
