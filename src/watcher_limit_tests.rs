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
