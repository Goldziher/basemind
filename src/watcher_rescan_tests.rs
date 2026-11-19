//! A rescan event (FSEvents `MustScanSubDirs`, inotify `IN_Q_OVERFLOW`) says the OS dropped events
//! under a path: the watcher rescans that subtree, or the whole tree, instead of discarding it.

use super::*;

/// What one batch of the store-owning watcher did.
#[derive(Debug, PartialEq, Eq)]
struct Pass {
    full: bool,
    scanned: usize,
    updated: usize,
    removed: usize,
}

/// [`watch_store_with`] over a [`Scripted`] backend, past its initial scan.
struct StoreWatch {
    feed: Feed,
    passes: mpsc::Receiver<Pass>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    thread: std::thread::JoinHandle<Result<(), WatchError>>,
}

impl StoreWatch {
    fn start(root: &Path) -> Self {
        crate::store::init_isolated_cache();
        let store = Arc::new(Mutex::new(
            Store::open(root, crate::store::VIEW_WORKING).expect("open store"),
        ));
        let mut config = crate::config::default_for_root(root);
        config.watch.debounce_ms = 50;
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (pass_tx, passes) = mpsc::channel::<Pass>();
        let root_for_thread = root.to_path_buf();
        let thread = std::thread::spawn(move || {
            let on_batch: BatchCallback = Box::new(move |batch: WatchBatch<'_>| {
                let stats = &batch.report.stats;
                let _ = pass_tx.send(Pass {
                    full: !matches!(batch.kind, BatchKind::Incremental { .. }),
                    scanned: stats.scanned,
                    updated: stats.updated,
                    removed: stats.removed,
                });
            });
            watch_store_with::<Scripted>(&root_for_thread, store, Arc::new(config), shutdown_rx, on_batch)
        });
        let watch = Self {
            feed: Feed::of(root),
            passes,
            shutdown,
            thread,
        };
        let initial = watch.next();
        assert!(initial.full && initial.updated > 0, "initial scan: {initial:?}");
        watch
    }

    fn rescan(&mut self, path: Option<&Path>) {
        let event = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        self.feed.send(match path {
            Some(path) => event.add_path(path.to_path_buf()),
            None => event,
        });
    }

    fn next(&self) -> Pass {
        self.passes.recv_timeout(Duration::from_secs(30)).expect("a pass")
    }

    /// Stop the watcher and return every pass it ran after the ones already taken.
    fn stop(self) -> Vec<Pass> {
        let _ = self.shutdown.send(());
        self.thread.join().expect("join watcher").expect("watcher succeeds");
        self.passes.try_iter().collect()
    }
}

/// `a/x.rs`, `a/y.rs`, `b/z.rs`.
fn three_file_tree() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    std::fs::create_dir_all(root.join("a")).expect("mkdir a");
    std::fs::create_dir_all(root.join("b")).expect("mkdir b");
    std::fs::write(root.join("a/x.rs"), b"fn x() {}\n").expect("seed");
    std::fs::write(root.join("a/y.rs"), b"fn y() {}\n").expect("seed");
    std::fs::write(root.join("b/z.rs"), b"fn z() {}\n").expect("seed");
    (tmp, root)
}

/// A rescan event naming a subdirectory rescans that subtree only: the edit the OS dropped under
/// it is indexed, one under another directory is not, and an unchanged subtree re-extracts nothing.
#[test]
fn a_rescan_event_naming_a_subtree_rescans_that_subtree() {
    let (_tmp, root) = three_file_tree();
    let mut watch = StoreWatch::start(&root);
    // Edits whose events the OS dropped: none reaches the watcher.
    std::fs::write(root.join("a/x.rs"), b"fn x_changed() {}\n").expect("edit");
    std::fs::write(root.join("b/z.rs"), b"fn z_changed() {}\n").expect("edit");

    watch.rescan(Some(&root.join("a")));
    let pass = watch.next();
    assert_eq!(
        pass,
        Pass {
            full: false,
            scanned: 2,
            updated: 1,
            removed: 0
        }
    );

    watch.rescan(Some(&root.join("a")));
    let pass = watch.next();
    assert_eq!(
        pass,
        Pass {
            full: false,
            scanned: 2,
            updated: 0,
            removed: 0
        },
        "an unchanged subtree re-extracts nothing"
    );
    assert_eq!(watch.stop(), []);
}

/// A rescan event naming the root (FSEvents' dropped events) or none (inotify's queue overflow)
/// runs one whole-tree pass for the burst, which picks up every dropped edit and deletion; over an
/// unchanged tree it re-extracts nothing.
#[test]
fn a_rescan_burst_naming_the_root_runs_one_whole_tree_pass() {
    let (_tmp, root) = three_file_tree();
    let mut watch = StoreWatch::start(&root);
    std::fs::write(root.join("b/z.rs"), b"fn z_changed() {}\n").expect("edit");
    std::fs::remove_file(root.join("a/y.rs")).expect("delete");

    watch.rescan(Some(&root.join("a")));
    watch.rescan(Some(&root));
    watch.rescan(None);
    let pass = watch.next();
    assert_eq!(
        pass,
        Pass {
            full: true,
            scanned: 2,
            updated: 1,
            removed: 1
        }
    );

    watch.rescan(None);
    let pass = watch.next();
    assert_eq!(
        pass,
        Pass {
            full: true,
            scanned: 2,
            updated: 0,
            removed: 0
        },
        "an unchanged tree re-extracts nothing"
    );
    assert_eq!(watch.stop(), [], "a burst is one pass");
}

/// The debouncer keeps one pending rescan event and overwrites it with the next, so a burst naming
/// two subtrees must not reach the scan as only the last of them.
#[test]
fn a_rescan_burst_naming_two_subtrees_rescans_both() {
    let (_tmp, root) = three_file_tree();
    let mut watch = StoreWatch::start(&root);
    std::fs::write(root.join("a/x.rs"), b"fn x_changed() {}\n").expect("edit");
    std::fs::write(root.join("b/z.rs"), b"fn z_changed() {}\n").expect("edit");

    watch.rescan(Some(&root.join("a")));
    watch.rescan(Some(&root.join("b")));
    let pass = watch.next();
    assert_eq!(
        pass,
        Pass {
            full: false,
            scanned: 3,
            updated: 2,
            removed: 0
        }
    );
    assert_eq!(watch.stop(), [], "a burst is one pass");
}

/// A whole-tree rescan re-derives the Linux watch set: a directory created while events were
/// dropped is armed, one removed meanwhile is released.
#[test]
fn dir_watches_resync_after_dropped_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    std::fs::create_dir_all(root.join("kept")).expect("mkdir");
    std::fs::create_dir_all(root.join("gone/deep")).expect("mkdir");
    let config = crate::config::default_for_root(&root);
    let filter =
        crate::scanner_filter::IndexFilter::new(&root, &config, crate::retrieval_wiring::backend()).expect("filter");
    let mut watches = DirWatches::default();
    watches.watched.insert(root.clone());
    let mut registry = Recorder::default();
    watches.sync(&mut registry, &filter, &[root.join("kept"), root.join("gone")]);

    std::fs::remove_dir_all(root.join("gone")).expect("rm gone");
    std::fs::create_dir_all(root.join("fresh/deep")).expect("mkdir fresh");
    std::fs::create_dir_all(root.join("target")).expect("mkdir target");
    filter.clear_cache();
    let mut registry = Recorder::default();
    watches.resync(&mut registry, &filter, &root);
    assert_eq!(registry.released, [root.join("gone"), root.join("gone/deep")]);
    assert_eq!(registry.armed, [root.join("fresh"), root.join("fresh/deep")]);
    assert_eq!(
        watches.watched.iter().collect::<Vec<_>>(),
        [&root, &root.join("fresh"), &root.join("fresh/deep"), &root.join("kept")]
    );
}
