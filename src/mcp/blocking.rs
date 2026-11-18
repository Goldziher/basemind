//! Blocking work inside an async tool body.

/// Run blocking work (a corpus sweep, blob or file IO, a git walk) without stalling the
/// reactor. `block_in_place` needs the multi-thread runtime the CLI builds; on a `current_thread`
/// runtime (tests) it runs inline, which is safe because that runtime has no other task to starve.
pub(super) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    let multi_thread = tokio::runtime::Handle::try_current()
        .map(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multi_thread {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

#[cfg(test)]
mod tests {
    /// `blocking` keeps blocking work off the reactor. With one worker thread, a task spawned beside
    /// the blocked one can only run if the worker hands its runtime over first; run inline, the call
    /// would hold the only worker for as long as it waits for that task.
    #[test]
    fn blocking_work_does_not_hold_the_only_worker_of_a_multi_thread_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let ran_beside = runtime.block_on(async {
            tokio::spawn(async {
                let (ran, saw_it) = std::sync::mpsc::channel();
                tokio::spawn(async move {
                    let _ = ran.send(());
                });
                super::blocking(|| saw_it.recv_timeout(std::time::Duration::from_secs(10)).is_ok())
            })
            .await
            .unwrap()
        });
        assert!(ran_beside, "a task beside the blocking call never ran");
    }
}
