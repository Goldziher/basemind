//! The Linux watcher under the inotify watch limit: a refused registration is counted and
//! reported, not dropped with a log line per directory.

use super::*;

/// Grants `capacity` watches, then refuses every further one as inotify does once
/// `fs.inotify.max_user_watches` is reached.
struct Limited {
    capacity: usize,
    armed: Vec<PathBuf>,
}

impl Limited {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            armed: Vec::new(),
        }
    }
}

impl DirRegistry for Limited {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), notify::Error> {
        if self.armed.len() == self.capacity {
            return Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch).add_path(dir.to_path_buf()));
        }
        self.armed.push(dir.to_path_buf());
        Ok(())
    }

    fn unwatch_dir(&mut self, dir: &Path) {
        self.armed.retain(|armed| armed != dir);
    }
}

fn tree(dirs: &[&str]) -> (tempfile::TempDir, PathBuf, crate::scanner_filter::IndexFilter) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    for dir in dirs {
        std::fs::create_dir_all(root.join(dir)).expect("mkdir");
    }
    let config = crate::config::default_for_root(&root);
    let filter =
        crate::scanner_filter::IndexFilter::new(&root, &config, crate::retrieval_wiring::backend()).expect("filter");
    (tmp, root, filter)
}

/// Directories past the watch limit are reported as unwatched, and are watched once a resync finds
/// room for them.
#[test]
fn a_watcher_that_hits_the_watch_limit_reports_the_unwatched_directories() {
    let (_tmp, root, filter) = tree(&["a/b", "c", "d"]);
    let mut watches = DirWatches::default();
    watches.watched.insert(root.clone());
    let mut registry = Limited::new(2);
    let health = WatchHealth::default();

    watches.sync(
        &mut registry,
        &filter,
        &[root.join("a"), root.join("c"), root.join("d")],
    );
    watches.publish(&health);
    assert_eq!(registry.armed, [root.join("a"), root.join("a/b")]);
    assert_eq!(
        watches.refused.iter().collect::<Vec<_>>(),
        [&root.join("c"), &root.join("d")]
    );
    assert_eq!((health.held(), health.unwatched()), (3, 2));

    registry.capacity = 10;
    filter.clear_cache();
    watches.resync(&mut registry, &filter, &root);
    watches.publish(&health);
    // The walk yields siblings in directory order. ~keep
    registry.armed[2..].sort();
    assert_eq!(
        registry.armed,
        [root.join("a"), root.join("a/b"), root.join("c"), root.join("d")]
    );
    assert!(watches.refused.is_empty(), "{:?}", watches.refused);
    assert_eq!((health.held(), health.unwatched()), (5, 0));
}

/// A refused directory that goes away is no longer reported as unwatched.
#[test]
fn a_refused_directory_that_is_removed_is_no_longer_unwatched() {
    let (_tmp, root, filter) = tree(&["a", "gone/deep"]);
    let mut watches = DirWatches::default();
    let mut registry = Limited::new(1);
    let health = WatchHealth::default();
    watches.sync(&mut registry, &filter, &[root.join("a"), root.join("gone")]);
    watches.publish(&health);
    assert_eq!((health.held(), health.unwatched()), (1, 2));

    std::fs::remove_dir_all(root.join("gone")).expect("rm gone");
    filter.clear_cache();
    watches.sync(&mut registry, &filter, &[root.join("gone")]);
    watches.publish(&health);
    assert!(watches.refused.is_empty(), "{:?}", watches.refused);
    assert_eq!((health.held(), health.unwatched()), (1, 0));
}

/// An error other than the watch limit (the directory vanished between the walk and the watch) is
/// not counted as unwatched.
#[test]
fn a_registration_error_that_is_not_the_limit_is_not_counted() {
    struct Vanished;
    impl DirRegistry for Vanished {
        fn watch_dir(&mut self, _dir: &Path) -> Result<(), notify::Error> {
            Err(notify::Error::path_not_found())
        }

        fn unwatch_dir(&mut self, _dir: &Path) {}
    }
    let (_tmp, root, filter) = tree(&["a"]);
    let mut watches = DirWatches::default();
    let health = WatchHealth::default();
    watches.sync(&mut Vanished, &filter, &[root.join("a")]);
    watches.publish(&health);
    assert_eq!((health.held(), health.unwatched()), (0, 0));
}

/// One directory per unwatched subtree is enough to rescan it: nested refusals collapse into their
/// topmost directory, and a sibling whose name merely starts the same stays separate.
#[test]
fn the_unwatched_roots_are_the_topmost_refused_directories() {
    let (_tmp, root, filter) = tree(&["a/b/c", "a-b", "d"]);
    let mut watches = DirWatches::default();
    let mut registry = Limited::new(0);
    watches.sync(
        &mut registry,
        &filter,
        &[root.join("a"), root.join("a-b"), root.join("d")],
    );
    assert!(watches.has_unwatched());
    assert_eq!(watches.refused.len(), 5, "{:?}", watches.refused);
    assert_eq!(
        watches.unwatched_roots(),
        [root.join("a"), root.join("a-b"), root.join("d")]
    );
}

/// With no refused directory there is nothing to poll.
#[test]
fn a_fully_watched_tree_has_no_unwatched_roots() {
    let (_tmp, root, filter) = tree(&["a"]);
    let mut watches = DirWatches::default();
    watches.sync(&mut Limited::new(10), &filter, &[root.join("a")]);
    assert!(!watches.has_unwatched());
    assert!(watches.unwatched_roots().is_empty());
}

/// No event reaches a directory the watch limit refused, so an edit under one is found only by the
/// periodic pass: it forwards the unwatched directory, and the watcher's own filter keeps it.
#[test]
fn an_edit_under_an_unwatched_directory_is_forwarded_by_the_poll() {
    let (_tmp, root, filter) = tree(&["watched", "unwatched/deep"]);
    let mut watches = DirWatches::default();
    let mut registry = Limited::new(1);
    watches.sync(&mut registry, &filter, &[root.join("watched"), root.join("unwatched")]);
    assert_eq!(registry.armed, [root.join("watched")]);

    std::fs::write(root.join("unwatched/deep/edited.rs"), b"fn edited() {}\n").expect("write edit");
    filter.clear_cache();

    let candidates = watches.unwatched_roots();
    assert_eq!(candidates, [root.join("unwatched")]);
    assert!(
        candidates.iter().all(|dir| keep_event_path(&filter, &root, dir, true)),
        "the filter must keep the unwatched directory so the scan expands it"
    );
}

/// The poll retries the directories the limit refused: once room exists the same pass arms them.
#[test]
fn the_poll_rearms_unwatched_directories_once_the_limit_has_room() {
    let (_tmp, root, filter) = tree(&["a", "b"]);
    let mut watches = DirWatches::default();
    let mut registry = Limited::new(0);
    watches.sync(&mut registry, &filter, &[root.join("a"), root.join("b")]);
    assert!(watches.has_unwatched());

    registry.capacity = 10;
    let polled = watches.unwatched_roots();
    watches.sync(&mut registry, &filter, &polled);
    assert!(!watches.has_unwatched(), "{:?}", watches.refused);
    assert_eq!(registry.armed, [root.join("a"), root.join("b")]);
}

/// The consumer sleeps without a timer unless directories are unwatched, then wakes after the poll
/// interval.
#[test]
fn waiting_for_a_message_honours_the_poll_interval() {
    let (tx, rx) = std::sync::mpsc::sync_channel::<WatchMsg>(1);
    assert!(matches!(
        wait_for_message(&rx, Some(Duration::from_millis(10))),
        Wake::Idle
    ));
    tx.send(WatchMsg::Shutdown).expect("send");
    assert!(matches!(wait_for_message(&rx, None), Wake::Message(WatchMsg::Shutdown)));
    tx.send(WatchMsg::Shutdown).expect("send");
    assert!(matches!(
        wait_for_message(&rx, Some(Duration::from_secs(5))),
        Wake::Message(WatchMsg::Shutdown)
    ));
    drop(tx);
    assert!(matches!(wait_for_message(&rx, None), Wake::Closed));
    assert!(matches!(
        wait_for_message(&rx, Some(Duration::from_secs(5))),
        Wake::Closed
    ));
}
