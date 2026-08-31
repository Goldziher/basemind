//! Terminal store failures: classifying them, persisting the first one, and reading it back.
//!
//! Some `fjall` errors are not transient. `Poisoned` means a failed flush/commit already broke the
//! consistency guarantee, and `Unrecoverable` says so outright — both are permanent for the
//! process's lifetime, so every subsequent request fails identically. A daemon that keeps running
//! in that state is worse than a dead one: it still holds the singleton flock and the socket, so
//! `pid_is_live` stays true, nothing else can take over, and the operator sees only total failure
//! with no evidence of the cause.
//!
//! This module supplies the two halves of the answer. [`is_fatal`] is the predicate the request
//! choke point uses to decide the store will never serve again, and [`persist`] writes the first
//! such error to `<comms_dir>/last-fatal.json` **before** the daemon exits, so `comms doctor` can
//! name it afterwards instead of the evidence dying with the process.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::CommsStoreError;

/// Filename of the durable fatal-error record inside the comms dir.
const FATAL_FILE: &str = "last-fatal.json";

/// The first terminal store failure a daemon hit, written out before it releases the singleton.
/// Read back by `comms doctor` so a wedged-then-exited daemon leaves an explanation behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FatalStoreError {
    /// Unix seconds at which the failure was recorded.
    pub epoch_secs: u64,
    /// The pid of the daemon that hit it. Distinguishes this corpse from any daemon live now.
    pub pid: u32,
    /// The daemon build that hit it.
    pub version: String,
    /// The request that tripped it, so the failing operation is named rather than guessed at.
    pub request: String,
    /// The rendered error, including the `fjall` variant.
    pub error: String,
}

/// Path of the fatal-error record for a comms dir.
pub fn fatal_path(comms_dir: &Path) -> PathBuf {
    comms_dir.join(FATAL_FILE)
}

/// Whether this error proves the store will never serve again in this process.
///
/// Deliberately narrow. `Io`, `Encode`/`Decode` and `Limit` are per-request failures a healthy
/// store recovers from, and `Locked` is a startup race the daemon entry point already converges on
/// — tearing the daemon down for any of those would turn a single bad request into an outage. Only
/// the self-declared permanent states qualify: `fjall`'s own `Poisoned` / `Unrecoverable`, and the
/// `Unrecoverable` that the underlying LSM tree raises when required files could not be recovered
/// from disk — which reaches us wrapped in `fjall::Error::Storage` and would otherwise slip past.
///
/// Note what is NOT here: `LsmError::ChecksumMismatch` and the invalid-tag / trailer / header
/// variants are corruption reports scoped to one block. They may well doom the next request too,
/// but they do not prove the whole store is unusable, and this predicate ends a daemon's life.
pub fn is_fatal(error: &CommsStoreError) -> bool {
    matches!(
        error,
        CommsStoreError::Fjall(
            fjall::Error::Poisoned
                | fjall::Error::Unrecoverable
                | fjall::Error::Storage(fjall::LsmError::Unrecoverable)
        )
    )
}

/// Record the first terminal failure for `comms_dir`, best-effort.
///
/// Write-once: an existing record is left alone, because the *first* error is the one that explains
/// the wedge — later ones are just the same poison observed again. Never returns an error; this runs
/// on the way out of a daemon that is already failing, and a write failure here must not preempt the
/// exit that actually frees the singleton.
pub fn persist(comms_dir: &Path, request: &str, error: &CommsStoreError) {
    let path = fatal_path(comms_dir);
    if path.exists() {
        return;
    }
    let record = FatalStoreError {
        epoch_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        request: request.to_string(),
        error: error.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// Read the fatal-error record for `comms_dir`, or `None` when there is none (or it is unreadable —
/// a corrupt record is indistinguishable from no record for an operator report).
pub fn read(comms_dir: &Path) -> Option<FatalStoreError> {
    let bytes = std::fs::read(fatal_path(comms_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the fatal-error record, acknowledging it. Returns whether a record was actually removed.
pub fn clear(comms_dir: &Path) -> bool {
    std::fs::remove_file(fatal_path(comms_dir)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poisoned() -> CommsStoreError {
        CommsStoreError::Fjall(fjall::Error::Poisoned)
    }

    #[test]
    fn only_permanent_fjall_states_are_fatal() {
        assert!(is_fatal(&poisoned()));
        assert!(is_fatal(&CommsStoreError::Fjall(fjall::Error::Unrecoverable)));
        assert!(
            is_fatal(&CommsStoreError::Fjall(fjall::Error::Storage(
                fjall::LsmError::Unrecoverable
            ))),
            "an unrecoverable LSM tree reaches us wrapped in Storage; it must not slip past"
        );
        assert!(
            !is_fatal(&CommsStoreError::Fjall(fjall::Error::Storage(
                fjall::LsmError::InvalidTrailer
            ))),
            "block-scoped corruption does not prove the whole store is unusable"
        );
        assert!(
            !is_fatal(&CommsStoreError::Limit("threads")),
            "a retention ceiling is a per-request refusal, not a broken store"
        );
        assert!(
            !is_fatal(&CommsStoreError::Locked(PathBuf::from("/tmp/x"))),
            "a lost startup race converges; it must not tear the winner down"
        );
        assert!(
            !is_fatal(&CommsStoreError::Io {
                path: PathBuf::from("/tmp/x"),
                source: std::io::Error::other("transient"),
            }),
            "an io error on one path does not prove the store is unusable"
        );
    }

    #[test]
    fn persist_is_write_once_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(dir.path()), None, "no record before anything fails");

        persist(dir.path(), "ThreadPost", &poisoned());
        let first = read(dir.path()).expect("record written");
        assert_eq!(first.request, "ThreadPost");
        assert!(
            first.error.contains("fjall"),
            "the fjall variant is named: {}",
            first.error
        );
        assert_eq!(first.pid, std::process::id());

        persist(
            dir.path(),
            "ThreadHistory",
            &CommsStoreError::Fjall(fjall::Error::Unrecoverable),
        );
        assert_eq!(
            read(dir.path()).expect("record still there"),
            first,
            "the FIRST error explains the wedge; later ones must not overwrite it"
        );
    }

    #[test]
    fn clear_removes_the_record_and_reports_whether_it_did() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!clear(dir.path()), "clearing nothing reports nothing removed");
        persist(dir.path(), "Ping", &poisoned());
        assert!(clear(dir.path()), "an existing record is removed");
        assert_eq!(read(dir.path()), None);
    }
}
