//! Process-wide rayon pools, each built once on first use.
//!
//! A check-then-build pool lets concurrent first callers each spawn a whole pool and race to publish
//! one: the losers' workers are spawned for nothing, and on a machine near its thread limit a loser's
//! build can fail while the winner's pool is there to use. Here one lock covers the build and its
//! publication, and a failed build publishes nothing, so the next call builds again.

use std::sync::{Mutex, OnceLock, PoisonError};

/// The pool in `cell`, built by `build` when `cell` is still empty. Concurrent first callers wait
/// for one build and share its pool; an `Err` from `build` is returned and leaves `cell` empty.
pub(crate) fn built_once<E>(
    cell: &'static OnceLock<rayon::ThreadPool>,
    build: impl FnOnce() -> Result<rayon::ThreadPool, E>,
) -> Result<&'static rayon::ThreadPool, E> {
    static BUILDING: Mutex<()> = Mutex::new(());
    if let Some(pool) = cell.get() {
        return Ok(pool);
    }
    let _building = BUILDING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(pool) = cell.get() {
        return Ok(pool);
    }
    let pool = build()?;
    Ok(cell.get_or_init(|| pool))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    use super::*;

    fn one_worker() -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
        rayon::ThreadPoolBuilder::new().num_threads(1).build()
    }

    #[test]
    fn concurrent_first_callers_share_one_build() {
        static CELL: OnceLock<rayon::ThreadPool> = OnceLock::new();
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        const CALLERS: usize = 4;
        let start = Arc::new(Barrier::new(CALLERS));
        let callers: Vec<_> = (0..CALLERS)
            .map(|_| {
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    built_once(&CELL, || {
                        BUILDS.fetch_add(1, Ordering::SeqCst);
                        // Holds the build open so an unserialized caller would start its own.
                        std::thread::sleep(Duration::from_millis(50));
                        one_worker()
                    })
                    .expect("the pool builds")
                })
            })
            .collect();
        let pools: Vec<&'static rayon::ThreadPool> = callers
            .into_iter()
            .map(|caller| caller.join().expect("caller"))
            .collect();
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
        assert!(pools.iter().all(|pool| std::ptr::eq(*pool, pools[0])));
    }

    #[test]
    fn a_failed_build_leaves_the_cell_empty_for_a_retry() {
        static CELL: OnceLock<rayon::ThreadPool> = OnceLock::new();
        let failed = built_once(&CELL, || Err::<rayon::ThreadPool, _>("refused"));
        assert_eq!(failed.err(), Some("refused"));
        assert!(CELL.get().is_none());
        let pool = built_once(&CELL, one_worker).expect("the retry builds");
        assert_eq!(pool.current_num_threads(), 1);
    }
}
