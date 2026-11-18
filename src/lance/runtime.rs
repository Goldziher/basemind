//! The retrieval stack's one tokio runtime (ADR-0023 rule 3).
//!
//! LanceDB and xberg are async; the scanner (rayon) and the engine are synchronous, so retrieval
//! `block_on`s this runtime from its own threads. Every vector store and every document extraction
//! shares it, where each store used to build a current-thread runtime of its own and extraction a
//! second pool. Built on first use and never dropped.

use std::sync::OnceLock;

use tokio::runtime::Runtime;

/// Worker threads: the futures themselves run on the thread that blocks on them, so the workers
/// only carry spawned tasks and the IO and timer drivers.
fn worker_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4))
}

/// The shared runtime, or why it could not be built (thread exhaustion). The failure is kept, so
/// every later caller fails fast the same way instead of retrying the build.
pub fn shared() -> Result<&'static Runtime, String> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            // Capped: the default is one worker per core plus a 512-thread blocking pool, on top of
            // the scanner's own per-core rayon pool that drives it. The blocking pool serves both
            // extraction and the vector stores' file IO.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads())
                .max_blocking_threads(32)
                .thread_keep_alive(std::time::Duration::from_secs(5))
                .thread_name("basemind-retrieval")
                .enable_all()
                .build()
                .map_err(|e| format!("build the retrieval runtime: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}
