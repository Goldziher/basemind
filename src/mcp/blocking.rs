//! Blocking work inside an async tool body.

/// Run blocking work (a corpus sweep, blob or file IO, a git walk) without stalling the
/// reactor. `block_in_place` needs the multi-thread runtime the CLI builds; on a `current_thread`
/// runtime (tests) it runs inline, which is safe because that runtime has no other task to starve.
pub(super) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    #[cfg(test)]
    BLOCKING_CALLS.with(|calls| calls.set(calls.get() + 1));
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
thread_local! {
    static BLOCKING_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times [`blocking`] was entered on this thread: what a test reads to see that a call
/// site still goes through it.
#[cfg(all(test, feature = "memory"))]
pub(super) fn blocking_calls() -> usize {
    BLOCKING_CALLS.with(std::cell::Cell::get)
}
