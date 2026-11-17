//! The status platform every working-tree status starts from, with gix's thread fan-out bounded.
//!
//! gix compares the index to the worktree on scoped threads it creates and joins inside each call,
//! one per core unless told otherwise: about eighteen threads created and destroyed per `git_diff`
//! status on a 14-core machine. In the long-lived daemon that was most of the tens of thousands of
//! threads a soak created, and every one of them took and abandoned allocator state. The comparison
//! is one `lstat` per index entry, so a few thousand entries finish faster on one thread than the
//! fan-out takes to start.

use super::GitError;

/// Index entries one comparison thread is given before another one is worth starting.
const ENTRIES_PER_THREAD: usize = 16_384;

/// The most comparison threads one status call may start, however large the index.
const MAX_THREADS: usize = 4;

/// Threads the index-to-worktree comparison of an index of `entries` entries may use.
pub(super) fn thread_limit(entries: usize) -> usize {
    (entries / ENTRIES_PER_THREAD).clamp(1, MAX_THREADS)
}

/// `local`'s status platform over its current index, comparing on at most
/// [`thread_limit`] threads. `what` names the operation in an error.
pub(super) fn platform<'repo>(
    local: &'repo gix::Repository,
    what: &str,
) -> Result<gix::status::Platform<'repo, gix::progress::Discard>, GitError> {
    let read_error = |msg: String| GitError::Read {
        what: what.to_string(),
        msg,
    };
    let index = local.index_or_empty().map_err(|error| read_error(error.to_string()))?;
    let threads = thread_limit(index.entries().len());
    Ok(local
        .status(gix::progress::Discard)
        .map_err(|error| read_error(error.to_string()))?
        .index(gix::worktree::IndexPersistedOrInMemory::Persisted(index))
        .index_worktree_options_mut(|options| options.thread_limit = Some(threads)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small index compares on one thread; a large one gets a bounded few, never one per core.
    #[test]
    fn the_comparison_fan_out_is_bounded_by_the_index_size() {
        assert_eq!(thread_limit(0), 1);
        assert_eq!(thread_limit(ENTRIES_PER_THREAD - 1), 1);
        assert_eq!(thread_limit(2 * ENTRIES_PER_THREAD), 2);
        assert_eq!(thread_limit(usize::MAX), MAX_THREADS);
    }
}
