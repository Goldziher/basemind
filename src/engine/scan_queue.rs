//! The watcher batches of a workspace waiting for a scan slot, merged into one pass.
//!
//! A daemon hosts the read stacks of many workspaces, and their watchers share a few scan slots.
//! A batch that arrives while its workspace waits for a slot joins the pass already waiting rather
//! than queueing a second one, so a branch switch delivered as many batches costs one pass once a
//! slot frees, however long the wait.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use crate::engine::types::index::QueuedScan;

/// One pass's worth of queued work.
#[cfg(any(test, all(feature = "comms", any(unix, windows))))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QueuedPass {
    /// Scan these paths, sorted and distinct.
    Paths(Vec<PathBuf>),
    /// Scan the whole tree: the OS dropped events, so no path set stands for what changed.
    WholeTree,
}

#[derive(Default)]
struct Queued {
    paths: BTreeSet<PathBuf>,
    whole_tree: bool,
    batches: usize,
    /// Whether a drain owns the queue: set by the submit that found none, cleared by the take that
    /// finds the queue empty, so exactly one drain runs while work is queued.
    #[cfg(any(test, all(feature = "comms", any(unix, windows))))]
    draining: bool,
}

/// Batches queued for a workspace's next scan pass.
#[derive(Default)]
pub(crate) struct ScanQueue(Mutex<Queued>);

impl ScanQueue {
    /// Queue one batch; `None` asks for a whole-tree pass. Returns `true` when no drain owns the
    /// queue, so the caller must start one.
    #[cfg(any(test, all(feature = "comms", any(unix, windows))))]
    pub(crate) fn submit(&self, batch: Option<Vec<PathBuf>>) -> bool {
        let mut queued = self.lock();
        queued.batches += 1;
        match batch {
            Some(paths) => queued.paths.extend(paths),
            None => queued.whole_tree = true,
        }
        !std::mem::replace(&mut queued.draining, true)
    }

    /// Everything queued since the last take, as one pass. `None` when nothing is queued, which
    /// ends the drain: the next submit starts another.
    #[cfg(any(test, all(feature = "comms", any(unix, windows))))]
    pub(crate) fn take(&self) -> Option<QueuedPass> {
        let mut queued = self.lock();
        if queued.batches == 0 {
            queued.draining = false;
            return None;
        }
        let Queued { paths, whole_tree, .. } = std::mem::take(&mut *queued);
        queued.draining = true;
        Some(if whole_tree {
            QueuedPass::WholeTree
        } else {
            QueuedPass::Paths(paths.into_iter().collect())
        })
    }

    /// The pass waiting for a slot, or `None` when nothing waits.
    pub(crate) fn queued(&self) -> Option<QueuedScan> {
        let queued = self.lock();
        (queued.batches > 0).then(|| QueuedScan {
            batches: queued.batches,
            paths: if queued.whole_tree { 0 } else { queued.paths.len() },
            whole_tree: queued.whole_tree,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queued> {
        // Every state the lock guards is consistent between statements, so a panic elsewhere leaves
        // nothing half-written.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// The first submit starts the drain, later ones join it, and the batches queued meanwhile are
    /// one pass of their distinct paths.
    #[test]
    fn batches_queued_before_a_take_are_one_pass() {
        let queue = ScanQueue::default();
        assert!(queue.submit(Some(paths(&["b.rs", "a.rs"]))));
        assert!(!queue.submit(Some(paths(&["a.rs", "c.rs"]))));
        assert_eq!(
            queue.queued(),
            Some(QueuedScan {
                batches: 2,
                paths: 3,
                whole_tree: false,
            })
        );
        assert_eq!(queue.take(), Some(QueuedPass::Paths(paths(&["a.rs", "b.rs", "c.rs"]))));
        assert_eq!(queue.queued(), None);

        assert!(!queue.submit(Some(paths(&["d.rs"]))), "the drain still owns the queue");
        assert_eq!(queue.take(), Some(QueuedPass::Paths(paths(&["d.rs"]))));
        assert_eq!(queue.take(), None, "an empty queue ends the drain");
        assert!(queue.submit(Some(paths(&["e.rs"]))), "the next batch starts another");
    }

    /// A whole-tree batch covers every path queued with it.
    #[test]
    fn a_whole_tree_batch_covers_the_paths_queued_with_it() {
        let queue = ScanQueue::default();
        assert!(queue.submit(Some(paths(&["a.rs"]))));
        assert!(!queue.submit(None));
        assert_eq!(
            queue.queued(),
            Some(QueuedScan {
                batches: 2,
                paths: 0,
                whole_tree: true,
            })
        );
        assert_eq!(queue.take(), Some(QueuedPass::WholeTree));
        assert_eq!(queue.take(), None);
    }
}
