//! The record of a terminal store failure a daemon left behind, and reading it back.
//!
//! Daemons up to this release's predecessor wrote the first permanent failure of their comms store
//! to `<comms_dir>/last-fatal.json` before exiting, so `comms doctor` could name it. The agent
//! store of comms v2 does not take the daemon down when it fails (it is reported by
//! `basemind agent status` instead), so nothing writes the record now; `comms doctor` still shows
//! and clears one that is there.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

    #[test]
    fn a_record_left_behind_is_read_and_cleared() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!clear(dir.path()), "clearing nothing reports nothing removed");
        let record = FatalStoreError {
            epoch_secs: 1,
            pid: 2,
            version: "0.33.0".to_string(),
            request: "thread_post".to_string(),
            error: "poisoned".to_string(),
        };
        std::fs::write(fatal_path(dir.path()), serde_json::to_vec(&record).expect("encode")).expect("write");
        assert_eq!(read(dir.path()), Some(record));
        assert!(clear(dir.path()), "an existing record is removed");
        assert_eq!(read(dir.path()), None);
    }
}
