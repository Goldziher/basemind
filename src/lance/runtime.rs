//! The retrieval stack's tokio runtimes (ADR-0023 rule 3, ADR-0050): the only module below the root
//! layer that builds one.
//!
//! LanceDB and xberg are async; the scanner (rayon) and the engine are synchronous, so retrieval
//! `block_on`s these from its own threads. Document extraction and the vector stores never share a
//! blocking pool: an extraction past its deadline is abandoned while its blocking work keeps its
//! thread, and a pool shared with lance's file reads would let hung parses queue every search.

use std::sync::OnceLock;
use std::time::Duration;

use tokio::runtime::Runtime;

use crate::config::MAX_ABANDONED_EXTRACTIONS;

/// How long an idle blocking thread lingers before it exits; threads are created on demand, so a
/// pool's ceiling costs nothing until it is used.
const KEEP_ALIVE: Duration = Duration::from_secs(5);

/// Extractions that run at once: bounded by the scan's rayon pool (one per core) and by
/// `[resources] max_concurrent_documents`. 64 covers the largest hosts basemind targets.
const MAX_ACTIVE_EXTRACTIONS: usize = 64;

/// The extraction runtime's blocking-pool ceiling. Every extraction, running or abandoned past its
/// deadline (at most [`MAX_ABANDONED_EXTRACTIONS`], the largest `[documents]
/// max_abandoned_extractions` accepted), holds at most two blocking threads: xberg's blocking
/// sections can `Handle::block_on` a future that itself waits on a second blocking task. Two per
/// extraction means that wait always finds a free thread, so the pool cannot deadlock on itself.
pub const EXTRACTION_BLOCKING_THREADS: usize = 2 * (MAX_ABANDONED_EXTRACTIONS + MAX_ACTIVE_EXTRACTIONS);

/// A vector store's blocking-pool ceiling. Lance runs each local file read or write as one
/// blocking task that waits on nothing else, at most `DEFAULT_LOCAL_IO_PARALLELISM` (8) per scan;
/// 64 serves eight concurrent queries on one store at full IO parallelism.
pub const STORE_BLOCKING_THREADS: usize = 64;

/// How long dropping a vector store waits for its blocking file IO to finish, outside an async
/// context. A single lance read or write; the bound only guards a wedged filesystem.
pub const STORE_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Worker threads: the futures themselves run on the thread that blocks on them, so the workers
/// only carry spawned tasks and the IO and timer drivers.
fn worker_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4))
}

/// The document-extraction runtime, or why it could not be built (thread exhaustion). The failure is
/// kept, so every later caller fails fast the same way instead of retrying the build.
pub fn extraction() -> Result<&'static Runtime, String> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            // Capped: the default is one worker per core plus a 512-thread blocking pool, on top of
            // the scanner's own per-core rayon pool that drives it.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads())
                .max_blocking_threads(EXTRACTION_BLOCKING_THREADS)
                .thread_keep_alive(KEEP_ALIVE)
                .thread_name("basemind-extract")
                .enable_all()
                .build()
                .map_err(|e| format!("build the extraction runtime: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// A runtime for one vector store, owned by it and shut down when it is dropped: the tasks lance
/// and lancedb spawn for the store (IO scheduler loops, background cache refreshes) hold its
/// datasets and file handles, and must end before the store's directory can be removed. A
/// current-thread runtime runs those tasks only inside the store's own `block_on`s, so shutting it
/// down drops every one of them on the dropping thread.
pub fn store() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(STORE_BLOCKING_THREADS)
        .thread_keep_alive(KEEP_ALIVE)
        .thread_name("basemind-lance")
        .enable_all()
        .build()
}

/// Shut a store's runtime down: its tasks are dropped here, and its blocking file IO is awaited
/// for up to [`STORE_SHUTDOWN_GRACE`] when this thread may block. Inside an async context it may
/// not (tokio panics), so the in-flight IO is left to finish on its own.
pub fn shut_down_store(runtime: Runtime) {
    if tokio::runtime::Handle::try_current().is_ok() {
        runtime.shutdown_background();
    } else {
        runtime.shutdown_timeout(STORE_SHUTDOWN_GRACE);
    }
}
