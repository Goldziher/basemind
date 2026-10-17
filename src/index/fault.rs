//! A latch for index commit failures that no later commit can recover from.
//!
//! Fjall reports a full disk as an `Io` error with kind `StorageFull`, and every commit after a
//! failed flush as `Poisoned`. Neither is transient: carrying on only burns minutes of extraction
//! work against a store that cannot persist it, then leaves a status that reads complete over
//! half-written `refs_by_*` partitions (issue #79). [`IndexFault`] remembers the first such error on
//! the shared [`IndexDb`](super::IndexDb) handle, so every commit site — the parallel workers, the
//! serial resolve writers — can record it where it happens and the scan can poll one place.

use std::error::Error;
use std::io::ErrorKind;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::IndexError;

/// The first fatal commit error seen through an index handle, rendered for the operator.
#[derive(Default)]
pub struct IndexFault {
    /// Mirrors `reason.is_some()` so the per-file hot path polls an atomic, not the mutex.
    tripped: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl IndexFault {
    /// Latch `error` when it is fatal; a non-fatal error is ignored. The first fatal error wins,
    /// because later ones are the same poison observed again.
    pub fn record(&self, error: &IndexError) {
        let Some(kind) = fatal_kind(error) else {
            return;
        };
        let mut slot = self.slot();
        slot.get_or_insert_with(|| format!("{kind}: {error}"));
        self.tripped.store(true, Ordering::Release);
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Cheap check for a hot loop: whether any fatal failure has been latched.
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    /// The latched reason, or `None` while the handle is healthy.
    pub fn reason(&self) -> Option<String> {
        self.slot().clone()
    }

    /// Forget a latched failure that freeing disk space cures. A poisoned handle stays latched: only
    /// reopening the database clears that.
    pub fn clear_recoverable(&self) {
        let mut slot = self.slot();
        if slot.as_deref().is_some_and(|reason| !reason.starts_with(POISONED)) {
            *slot = None;
            self.tripped.store(false, Ordering::Release);
        }
    }

    /// Whether the latched failure is a poisoned handle, which only reopening the database clears.
    pub fn is_poisoned(&self) -> bool {
        self.reason().is_some_and(|reason| reason.starts_with(POISONED))
    }
}

const POISONED: &str = "index handle poisoned";
const STORAGE_FULL: &str = "disk full";

/// Classify `error` as fatal: the disk is full, or fjall has declared the handle poisoned or
/// unrecoverable. Walks the source chain, because the `StorageFull` io error arrives wrapped
/// (`fjall::Error::Io`, or `lsm_tree::Error::Io` inside `fjall::Error::Storage`).
fn fatal_kind(error: &IndexError) -> Option<&'static str> {
    if matches!(
        error,
        IndexError::Fjall(fjall::Error::Poisoned | fjall::Error::Unrecoverable)
    ) {
        return Some(POISONED);
    }
    let mut cursor: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(current) = cursor {
        if current
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == ErrorKind::StorageFull)
        {
            return Some(STORAGE_FULL);
        }
        cursor = current.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> IndexError {
        IndexError::Fjall(fjall::Error::Io(std::io::Error::from(ErrorKind::StorageFull)))
    }

    #[test]
    fn storage_full_and_poisoned_latch_and_the_first_one_wins() {
        let fault = IndexFault::default();
        assert!(fault.reason().is_none());
        fault.record(&full());
        fault.record(&IndexError::Fjall(fjall::Error::Poisoned));
        let reason = fault.reason().expect("latched");
        assert!(reason.starts_with(STORAGE_FULL), "first error wins: {reason}");
        assert!(!fault.is_poisoned());
    }

    #[test]
    fn clearing_forgets_a_full_disk_but_never_a_poisoned_handle() {
        let fault = IndexFault::default();
        fault.record(&full());
        fault.clear_recoverable();
        assert!(fault.reason().is_none());
        fault.record(&IndexError::Fjall(fjall::Error::Poisoned));
        fault.clear_recoverable();
        assert!(fault.is_poisoned());
    }

    #[test]
    fn poisoned_is_reported_as_poisoned() {
        let fault = IndexFault::default();
        fault.record(&IndexError::Fjall(fjall::Error::Poisoned));
        assert!(fault.is_poisoned());
    }

    #[test]
    fn wrapped_and_plain_io_storage_full_are_both_fatal() {
        let fault = IndexFault::default();
        fault.record(&IndexError::Io {
            path: "x".into(),
            source: std::io::Error::from(ErrorKind::StorageFull),
        });
        assert!(fault.reason().is_some());
    }

    #[test]
    fn transient_errors_do_not_latch() {
        let fault = IndexFault::default();
        fault.record(&IndexError::Io {
            path: "x".into(),
            source: std::io::Error::from(ErrorKind::PermissionDenied),
        });
        fault.record(&IndexError::Fjall(fjall::Error::Io(std::io::Error::from(
            ErrorKind::Interrupted,
        ))));
        assert!(fault.reason().is_none());
    }
}
