//! A test seam that holds a scan mid-pass, so a test can act while the pass is certainly running
//! (interrupt it, or overlap a second scan with it) instead of racing a sleep against a fast
//! machine.
//!
//! Compiled only under the dev-only `test-support` feature, so production builds carry neither the
//! variable nor the wait. With `TEST_HOLD_SCAN_ENV` naming a gate file that exists, every file a
//! scan claims waits before its extraction until the gate is removed or the scan is cancelled. The
//! first file to wait creates `held_marker` next to the gate: the test's readiness signal.

#[cfg(feature = "test-support")]
use std::path::{Path, PathBuf};

use crate::scanner::ScanCancel;

/// Names the gate file that holds every scan in this process while it exists.
#[cfg(feature = "test-support")]
pub const TEST_HOLD_SCAN_ENV: &str = "BASEMIND_TEST_HOLD_SCAN";

/// How often a held file looks at the gate and the cancel token again.
#[cfg(feature = "test-support")]
const POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// The marker a held scan creates beside `gate` once it is waiting on it.
#[cfg(feature = "test-support")]
pub fn held_marker(gate: &Path) -> PathBuf {
    let mut name = gate.as_os_str().to_owned();
    name.push(".held");
    PathBuf::from(name)
}

/// Wait while the [`TEST_HOLD_SCAN_ENV`] gate exists and `cancel` is not tripped.
#[cfg(feature = "test-support")]
pub(crate) fn hold_while_gated(cancel: &ScanCancel) {
    static GATE: std::sync::LazyLock<Option<PathBuf>> =
        std::sync::LazyLock::new(|| std::env::var_os(TEST_HOLD_SCAN_ENV).map(PathBuf::from));
    let Some(gate) = GATE.as_deref() else {
        return;
    };
    if !gate.exists() || cancel.is_cancelled() {
        return;
    }
    std::fs::write(held_marker(gate), b"")
        .unwrap_or_else(|error| panic!("write the held-scan marker beside {}: {error}", gate.display()));
    while gate.exists() && !cancel.is_cancelled() {
        std::thread::sleep(POLL);
    }
}

/// Production builds never hold a scan.
#[cfg(not(feature = "test-support"))]
pub(crate) fn hold_while_gated(_cancel: &ScanCancel) {}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    #[test]
    fn the_held_marker_sits_beside_the_gate() {
        assert_eq!(
            held_marker(Path::new("/tmp/hold/gate")),
            PathBuf::from("/tmp/hold/gate.held")
        );
    }
}
