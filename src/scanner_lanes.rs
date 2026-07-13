//! Fault containment for the scanner's OPTIONAL post-extraction lanes.
//!
//! [`crate::scanner::scan`] persists the code map (`Store::flush`, i.e. `index.msgpack`) as soon as
//! extraction and the Fjall index writes are done. Everything that runs after that barrier —
//! resolved-reference stitching, the LanceDB document / code batches, the BM25 corpus stats — is an
//! *enrichment* lane: valuable, but never worth the code map.
//!
//! Without containment, a panic in any of those lanes (e.g. the `stack-graphs` partial-path
//! stitcher indexing out of bounds) unwound straight out of `scan`, the final flush never ran, and
//! the workspace was left with gigabytes of committed blobs next to an `index.msgpack` reporting
//! `file_count: 0` — a silently empty code map, plus a full re-scan on every launch. A degraded lane
//! must mean "no resolved refs" or "no embeddings", never "no code map".
//!
//! Two containment primitives, both built on `catch_unwind`:
//!
//! - [`run_optional_lane`] — wraps a whole lane. Rayon's `ThreadPool::install` re-raises a worker
//!   panic on the calling thread, so this also contains panics raised on the scanner pool's workers.
//! - [`contain_panic`] — wraps one unit of work inside a lane, so a single pathological file is
//!   dropped instead of taking the other 47 000 files' resolution down with it.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Intra-file + cross-file reference resolution (`intel::resolve_pass`).
pub const LANE_RESOLVE: &str = "resolve";
/// Document batches → LanceDB (`scanner_docs::flush_document_batches`).
pub const LANE_DOC_BATCHES: &str = "doc_batches";
/// Code-chunk batches → LanceDB (`scanner_code::flush_code_batches`).
pub const LANE_CODE_BATCHES: &str = "code_batches";
/// Purge of `code_chunks` rows for files removed since the last scan.
pub const LANE_CODE_REMOVALS: &str = "code_removals";
/// Purge of `documents` rows for docs removed since the last scan.
#[cfg(feature = "documents")]
pub const LANE_DOC_REMOVALS: &str = "doc_removals";
/// Corpus-global BM25 statistics recompute.
pub const LANE_BM25_STATS: &str = "bm25_stats";

/// Fault-injection seam, compiled only under the dev-only `test-support` feature so production
/// builds carry neither the variable nor the branch.
///
/// Value is `"<lane>"` or `"<lane>:<kind>"`, where kind is:
/// - `panic` (the default) — unwind inside the lane, standing in for the `stack-graphs` panic.
/// - `abort` — kill the process mid-lane, standing in for the SIGKILL an operator sends when an
///   optional lane hangs forever (the ONNX-model download stuck on a blackholed IPv6 route).
#[cfg(feature = "test-support")]
pub const TEST_FAULT_LANE_ENV: &str = "BASEMIND_TEST_FAULT_LANE";

#[cfg(feature = "test-support")]
fn inject_fault(lane: &str) {
    let Ok(spec) = std::env::var(TEST_FAULT_LANE_ENV) else {
        return;
    };
    let (want, kind) = spec.split_once(':').unwrap_or((spec.as_str(), "panic"));
    if want != lane {
        return;
    }
    if kind == "abort" {
        std::process::abort();
    }
    panic!("injected fault in optional scan lane `{lane}`");
}

#[cfg(not(feature = "test-support"))]
fn inject_fault(_lane: &str) {}

/// Run one optional post-extraction lane, containing any panic it raises.
///
/// The code map is already durable on disk by the time a lane runs, so a lane that dies degrades
/// only its own tier. The failure is logged at WARN with the lane name and the panic message.
pub(crate) fn run_optional_lane(lane: &str, body: impl FnOnce()) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        inject_fault(lane);
        body();
    }));
    if let Err(payload) = outcome {
        tracing::warn!(
            lane,
            reason = panic_reason(payload.as_ref()),
            "optional scan lane panicked; this tier is degraded for the scan but the code map is intact"
        );
    }
}

/// Run one unit of work inside a lane, returning `Err(reason)` if it panicked.
pub(crate) fn contain_panic<T>(body: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(body)).map_err(|payload| panic_reason(payload.as_ref()).to_string())
}

/// Best-effort message out of a panic payload. `panic!("literal")` yields a `&'static str`;
/// `panic!("{x}")` and `assert!`/index-out-of-bounds yield a `String`.
fn panic_reason(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    }
}
