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
        if let Some((at, reason)) = self.failure()
            && at.elapsed() < RETRY_AFTER
        {
            return Err(unavailable(&reason));
        }
        match self.store.get_or_try_init(open).await {
            Ok(store) => Ok(Arc::clone(store)),
            Err(reason) => {
                *self.failed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((Instant::now(), reason.clone()));
                Err(unavailable(&reason))
            }
        }
    }

    fn failure(&self) -> Option<(Instant, String)> {
        self.failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
