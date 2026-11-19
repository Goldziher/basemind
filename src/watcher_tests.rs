use super::*;
use std::sync::mpsc;
use std::time::Duration;

/// Block until the watcher is demonstrably live, then drain what arming produced.
///
/// Registering a filesystem watch is asynchronous, so a test that just sleeps before its
/// trigger races the registration whenever the machine is busy: an unarmed watcher reports
/// nothing, so a negative assertion passes without having tested anything. This writes
/// `probe.rs` under `root` until the callback answers (proving the watch is live) and then drains
/// pending batches, so a following assertion measures the filter rather than the startup window.
fn arm_watcher(root: &Path, path_rx: &mpsc::Receiver<Vec<PathBuf>>) {
    let probe = root.join("probe.rs");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(std::time::Instant::now() < deadline, "watcher never armed within 30s");
        std::fs::write(&probe, b"fn probe() {}\n").expect("write probe file");
        match path_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(_) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("watcher thread died"),
        }
    }
    std::fs::remove_file(&probe).expect("remove probe file");
    // Drain the arming batches (and the probe's own removal). A late one can still arrive after the
    // drain, so the assertions under test match the paths they rule out, not any emission.
    while path_rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
}

/// The event handlers of [`Scripted`] watchers, by the first path each was asked to watch.
static SCRIPTED: Mutex<Vec<(PathBuf, Box<dyn notify::EventHandler>)>> = Mutex::new(Vec::new());
static SCRIPTED_WATCHED: std::sync::Condvar = std::sync::Condvar::new();

/// A notify backend whose events the test injects through a [`Feed`]. It stands in for FSEvents
/// and inotify where a test asserts what the watcher makes of an event: their delivery goes through
/// the OS (on macOS through `fseventsd`, one daemon for every process), which a loaded machine keeps
/// behind by tens of seconds, so a deadline on it fails without a defect.
struct Scripted(Option<Box<dyn notify::EventHandler>>);

impl notify::Watcher for Scripted {
    fn new<F: notify::EventHandler>(event_handler: F, _config: notify::Config) -> notify::Result<Self> {
        Ok(Self(Some(Box::new(event_handler))))
    }

    fn watch(&mut self, path: &Path, _recursive_mode: RecursiveMode) -> notify::Result<()> {
        if let Some(handler) = self.0.take() {
            SCRIPTED.lock().expect("scripted").push((path.to_path_buf(), handler));
            SCRIPTED_WATCHED.notify_all();
        }
        Ok(())
    }

    fn unwatch(&mut self, _path: &Path) -> notify::Result<()> {
        Ok(())
    }

    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::NullWatcher
    }
}

/// The raw events of a [`Scripted`] watcher: what the backend would hand the debouncer.
struct Feed(Box<dyn notify::EventHandler>);

impl Feed {
    /// The feed of the scripted watcher on `root`, once the watcher has started watching it.
    fn of(root: &Path) -> Self {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut scripted = SCRIPTED.lock().expect("scripted");
        loop {
            if let Some(at) = scripted.iter().position(|(path, _)| path == root) {
                return Self(scripted.swap_remove(at).1);
            }
            let left = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("the watcher never watched its root");
            scripted = SCRIPTED_WATCHED.wait_timeout(scripted, left).expect("scripted").0;
        }
    }

    fn send(&mut self, event: notify::Event) {
        self.0.handle_event(Ok(event));
    }

    fn event(&mut self, kind: EventKind, path: &Path) {
        self.send(notify::Event::new(kind).add_path(path.to_path_buf()));
    }

    /// A rename of `from` to `to` (already done on disk) as each backend reports it: FSEvents as an
    /// unpaired rename on each path, inotify as a `From` / `To` pair sharing a cookie.
    fn rename(&mut self, from: &Path, to: &Path, inotify: bool) {
        use notify::event::{ModifyKind::Name, RenameMode};
        if inotify {
            let named = |mode, path: &Path| {
                notify::Event::new(EventKind::Modify(Name(mode)))
                    .add_path(path.to_path_buf())
                    .set_tracker(1)
            };
            self.send(named(RenameMode::From, from));
            self.send(named(RenameMode::To, to));
            self.send(named(RenameMode::Both, from).add_path(to.to_path_buf()));
        } else {
            self.event(EventKind::Modify(Name(RenameMode::Any)), from);
            self.event(EventKind::Modify(Name(RenameMode::Any)), to);
        }
    }
}

/// [`watch_paths`] over a [`Scripted`] backend on its own thread.
struct ScriptedWatch {
    feed: Feed,
    /// Each batch the watcher emitted.
    paths: mpsc::Receiver<Vec<PathBuf>>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    thread: std::thread::JoinHandle<Result<(), WatchError>>,
}

impl ScriptedWatch {
    fn start(root: &Path, debounce_ms: u64) -> Self {
        let mut config = crate::config::default_for_root(root);
        config.watch.debounce_ms = debounce_ms;
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (path_tx, paths) = mpsc::channel::<Vec<PathBuf>>();
        let root_for_thread = root.to_path_buf();
        let thread = std::thread::spawn(move || {
            watch_with::<Scripted>(
                &root_for_thread,
                &config,
                || None,
                shutdown_rx,
                |paths, kind| {
                    assert!(matches!(kind, BatchKind::Incremental { .. }));
                    let _ = path_tx.send(paths);
                },
            )
        });
        Self {
            feed: Feed::of(root),
            paths,
            shutdown,
            thread,
        }
    }

    fn stop(self) {
        let _ = self.shutdown.send(());
        self.thread.join().expect("join watcher").expect("watcher succeeds");
    }
}

/// Every path emitted up to and including the batch that carries `sentinel`. The debouncer emits a
/// path's event no later than any event fed after it, so this holds whatever the events fed before
/// `sentinel` produced.
fn emitted_through(path_rx: &mpsc::Receiver<Vec<PathBuf>>, sentinel: &Path) -> Vec<PathBuf> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut emitted = Vec::new();
    while !emitted.iter().any(|path| path == sentinel) {
        let left = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("the sentinel never surfaced");
        emitted.extend(path_rx.recv_timeout(left).expect("the sentinel never surfaced"));
    }
    emitted
}

/// `watch_paths` should hand the callback the repo-relative path of a file
/// that changes under the watched root, within a bounded window. This is the
/// primitive the MCP serve watcher funnels into `Workspace::rescan`.
#[test]
fn should_emit_changed_path_when_file_is_modified() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    let mut config = crate::config::default_for_root(&root);
    config.watch.debounce_ms = 50;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();

    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        watch_paths(&root_for_thread, &config, shutdown_rx, |paths, kind| {
            assert!(matches!(kind, BatchKind::Incremental { .. }));
            let _ = path_tx.send(paths);
        })
    });

    let target = root.join("hello.rs");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let received = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "watcher never reported hello.rs within 30s"
        );
        std::fs::write(&target, b"fn main() {}\n").expect("write file");
        match path_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(paths) => break paths,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("watcher thread died"),
        }
    };
    assert!(
        received.iter().any(|p| p.ends_with("hello.rs")),
        "expected hello.rs in {received:?}"
    );

    let _ = shutdown_tx.send(());
    let _ = handle.join();
}

/// A config reload must rebuild the watcher's filter: a path the startup config excluded wakes a
/// rescan once the reloaded config stops excluding it.
#[test]
fn reloaded_config_rebuilds_the_watcher_filter() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    let mut config = crate::config::default_for_root(&root);
    config.watch.debounce_ms = 50;
    config.scan.exclude = vec!["blocked.rs".to_string()];
    let mut reloaded = config.clone();
    reloaded.scan.exclude = Vec::new();
    let reloaded = Arc::new(reloaded);

    let swap = Arc::new(AtomicBool::new(false));
    let swap_for_thread = Arc::clone(&swap);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();
    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        watch_paths_reloading(
            &root_for_thread,
            &config,
            move || {
                swap_for_thread
                    .swap(false, Ordering::SeqCst)
                    .then(|| Arc::clone(&reloaded))
            },
            shutdown_rx,
            |paths, _| {
                let _ = path_tx.send(paths);
            },
        )
    });
    arm_watcher(&root, &path_rx);

    let blocked = root.join("blocked.rs");
    std::fs::write(&blocked, b"fn a() {}\n").expect("write blocked");
    while let Ok(paths) = path_rx.recv_timeout(Duration::from_millis(800)) {
        assert!(
            !paths.iter().any(|p| p.ends_with("blocked.rs")),
            "the startup config excludes blocked.rs: {paths:?}"
        );
    }

    swap.store(true, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut seen = false;
    while !seen {
        assert!(
            std::time::Instant::now() < deadline,
            "blocked.rs never surfaced after the reload"
        );
        std::fs::write(&blocked, b"fn b() {}\n").expect("rewrite blocked");
        if let Ok(paths) = path_rx.recv_timeout(Duration::from_millis(500)) {
            seen = paths.iter().any(|p| p.ends_with("blocked.rs"));
        }
    }

    let _ = shutdown_tx.send(());
    let _ = handle.join();
}

#[test]
fn config_reloader_yields_a_config_only_after_the_file_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    std::fs::write(
        root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[watch]\ndebounce_ms = 111\n",
    )
    .unwrap();
    let mut reload = config_reloader(&root);
    assert!(reload().is_none(), "unchanged file: nothing to reload");

    std::fs::write(
        root.join("basemind.toml"),
        "\"$schema\" = \"v1\"\n[watch]\ndebounce_ms = 222\n",
    )
    .unwrap();
    let fresh = reload().expect("a rewritten file is reloaded");
    assert_eq!(fresh.watch.debounce_ms, 222);
    assert!(reload().is_none(), "the change is reported once");

    std::fs::write(root.join("basemind.toml"), "not toml = = =").unwrap();
    assert!(reload().is_none(), "a broken file keeps the previous config");
}

#[test]
fn shutdown_interrupts_sustained_event_batches() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    let pulse = root.join("pulse.rs");
    std::fs::write(&pulse, b"fn pulse() {}\n").expect("write pulse");
    let mut config = crate::config::default_for_root(&root);
    config.watch.debounce_ms = 1;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (batch_tx, batch_rx) = mpsc::channel::<usize>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let feed: Arc<Mutex<Option<Feed>>> = Arc::default();
    let feed_for_thread = Arc::clone(&feed);
    let pulse_for_thread = pulse.clone();
    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        let mut generation = 0usize;
        let result = watch_with::<Scripted>(
            &root_for_thread,
            &config,
            || None,
            shutdown_rx,
            |_paths, _kind| {
                generation += 1;
                let _ = batch_tx.send(generation);
                // Every batch produces the next: the event stream never goes quiet.
                if let Some(feed) = feed_for_thread.lock().expect("feed").as_mut() {
                    feed.event(MODIFY, &pulse_for_thread);
                }
            },
        );
        let _ = done_tx.send(());
        result
    });
    let mut first = Feed::of(&root);
    first.event(MODIFY, &pulse);
    *feed.lock().expect("feed") = Some(first);

    for _ in 0..4 {
        batch_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("sustained batch arrives");
    }

    shutdown_tx.send(()).expect("signal shutdown under load");
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("watcher must observe shutdown without waiting for a quiet receive timeout");
    handle.join().expect("join watcher").expect("watcher succeeds");
}

/// A rename must still surface the new path. Under `NoCache` the debouncer no longer stitches
/// FileId-based rename events, so a rename degrades to remove-old + create-new — we assert the
/// create half reaches the callback so the renamed file gets (re)indexed. Guards the cache swap
/// in `watch_paths` (issue #43). Both backends' shapes of a rename go through the debouncer.
#[test]
fn should_emit_new_path_when_file_is_renamed() {
    for inotify in [false, true] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        let original = root.join("before.rs");
        std::fs::write(&original, b"fn main() {}\n").expect("seed file");
        let mut watch = ScriptedWatch::start(&root, 50);

        let renamed = root.join("after.rs");
        std::fs::rename(&original, &renamed).expect("rename file");
        watch.feed.rename(&original, &renamed, inotify);

        let emitted = emitted_through(&watch.paths, &renamed);
        assert!(emitted.contains(&renamed), "inotify {inotify}: {emitted:?}");

        watch.stop();
    }
}

/// Changes inside `.basemind/` must never surface — the watcher would
/// otherwise feed its own index writes back into a rescan loop.
#[test]
fn should_ignore_changes_under_basemind_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    std::fs::create_dir_all(root.join(crate::config::BASEMIND_DIR)).expect("mkdir .basemind");
    let mut watch = ScriptedWatch::start(&root, 50);

    let noise = root.join(crate::config::BASEMIND_DIR).join("noise.txt");
    std::fs::write(&noise, b"ignored\n").expect("write basemind file");
    watch
        .feed
        .event(EventKind::Create(notify::event::CreateKind::File), &noise);
    let sentinel = root.join("sentinel.rs");
    std::fs::write(&sentinel, b"fn s() {}\n").expect("write sentinel");
    watch
        .feed
        .event(EventKind::Create(notify::event::CreateKind::File), &sentinel);

    let emitted = emitted_through(&watch.paths, &sentinel);
    assert!(
        !emitted
            .iter()
            .any(|path| path.starts_with(root.join(crate::config::BASEMIND_DIR))),
        "a path under .basemind/ was emitted: {emitted:?}"
    );

    watch.stop();
}

/// Writes under a *nested* child-repo `.basemind/` and under a gitignored path must not wake a
/// rescan — this is the core of issue #33 (an umbrella repo's watcher must ignore a nested
/// serve's index flushes, and gitignored churn generally).
#[test]
fn should_ignore_nested_basemind_and_gitignored_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
    std::fs::create_dir_all(root.join("child").join(crate::config::BASEMIND_DIR)).expect("mkdir child/.basemind");
    std::fs::write(root.join(".gitignore"), b"build/\n").expect("write .gitignore");
    std::fs::create_dir_all(root.join("build")).expect("mkdir build");
    let mut watch = ScriptedWatch::start(&root, 50);

    let created = EventKind::Create(notify::event::CreateKind::File);
    let nested = root
        .join("child")
        .join(crate::config::BASEMIND_DIR)
        .join("index.msgpack");
    std::fs::write(&nested, b"\x00").expect("write nested basemind file");
    watch.feed.event(created, &nested);
    let ignored = root.join("build").join("out.o");
    std::fs::write(&ignored, b"\x00").expect("write gitignored file");
    watch.feed.event(created, &ignored);
    let sentinel = root.join("sentinel.rs");
    std::fs::write(&sentinel, b"fn s() {}\n").expect("write sentinel");
    watch.feed.event(created, &sentinel);

    let emitted = emitted_through(&watch.paths, &sentinel);
    assert!(
        !emitted
            .iter()
            .any(|path| path.starts_with(root.join("child")) || path.starts_with(root.join("build"))),
        "nested-.basemind / gitignored churn was emitted: {emitted:?}"
    );

    watch.stop();
}

/// A rescan can outlast the event stream that triggered it, and everything that happened during
/// it must still arrive — queued or coalesced. The callback here blocks well past the debounce
/// window, so `late.rs` is written while the "rescan" owns the consumer thread.
#[test]
fn should_deliver_events_produced_while_the_consumer_is_busy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    let mut config = crate::config::default_for_root(&root);
    config.watch.debounce_ms = 50;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (path_tx, path_rx) = mpsc::channel::<Vec<PathBuf>>();
    let (busy_tx, busy_rx) = mpsc::channel::<()>();

    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        watch_with::<Scripted>(
            &root_for_thread,
            &config,
            || None,
            shutdown_rx,
            |paths, _kind| {
                let _ = busy_tx.send(());
                // Stands in for the synchronous rescan the real consumers run inside this callback.
                std::thread::sleep(Duration::from_millis(600));
                let _ = path_tx.send(paths);
            },
        )
    });
    let mut feed = Feed::of(&root);
    let created = EventKind::Create(notify::event::CreateKind::File);

    let arm = root.join("arm.rs");
    std::fs::write(&arm, b"fn arm() {}\n").expect("write arm file");
    feed.event(created, &arm);
    busy_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the consumer never took the first batch");

    // The consumer is inside the callback right now; this event has nowhere to go but the queue
    // or the pending set.
    let late = root.join("late.rs");
    std::fs::write(&late, b"fn late() {}\n").expect("write late file");
    feed.event(created, &late);

    let emitted = emitted_through(&path_rx, &late);
    assert!(emitted.contains(&late), "{emitted:?}");

    let _ = shutdown_tx.send(());
    handle.join().expect("join watcher").expect("watcher succeeds");
}

const MODIFY: EventKind = EventKind::Modify(notify::event::ModifyKind::Any);

/// One debounced batch, in the shape `notify-debouncer-full` hands the callback.
fn batch(kind: EventKind, paths: &[&str]) -> DebounceEventResult {
    Ok(paths
        .iter()
        .map(|p| {
            DebouncedEvent::new(
                notify::Event::new(kind).add_path(PathBuf::from(p)),
                std::time::Instant::now(),
            )
        })
        .collect())
}

fn drain(rx: &mpsc::Receiver<WatchMsg>) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok()).count()
}

/// The bound must be invisible under normal load: with room in the queue a batch goes straight to
/// the consumer and nothing is set aside.
#[test]
fn should_deliver_batch_to_the_queue_when_it_has_room() {
    let (tx, rx) = mpsc::sync_channel::<WatchMsg>(DEBOUNCE_QUEUE_CAPACITY);
    let pending = PendingPaths::default();

    dispatch_debounced(&tx, &pending, batch(MODIFY, &["/repo/a.rs", "/repo/b.rs"]));

    let Ok(WatchMsg::Batch(Ok(events))) = rx.try_recv() else {
        panic!("the batch must reach the consumer");
    };
    let paths: Vec<PathBuf> = events.iter().flat_map(|e| e.event.paths.iter().cloned()).collect();
    assert_eq!(paths, vec![PathBuf::from("/repo/a.rs"), PathBuf::from("/repo/b.rs")]);
    assert!(
        pending.take().is_empty(),
        "nothing should be coalesced while the queue has room"
    );
}

/// A full queue must neither block the debouncer nor drop a path: the overflow is folded into the
/// pending set, deduped, and filtered by the same relevance test the consumer applies.
#[test]
fn should_coalesce_into_the_pending_set_when_the_queue_is_full() {
    let (tx, rx) = mpsc::sync_channel::<WatchMsg>(DEBOUNCE_QUEUE_CAPACITY);
    let pending = PendingPaths::default();

    for i in 0..DEBOUNCE_QUEUE_CAPACITY {
        dispatch_debounced(&tx, &pending, batch(MODIFY, &[format!("/repo/queued_{i}.rs").as_str()]));
    }
    assert!(pending.take().is_empty(), "the queue had room for all of those");

    // Now full. `busy_b.rs` repeats across two batches, and a read is not a change at all.
    dispatch_debounced(&tx, &pending, batch(MODIFY, &["/repo/busy_a.rs"]));
    dispatch_debounced(&tx, &pending, batch(MODIFY, &["/repo/busy_b.rs", "/repo/busy_c.rs"]));
    dispatch_debounced(&tx, &pending, batch(MODIFY, &["/repo/busy_b.rs"]));
    dispatch_debounced(
        &tx,
        &pending,
        batch(EventKind::Access(notify::event::AccessKind::Any), &["/repo/read.rs"]),
    );

    assert_eq!(
        pending.take(),
        vec![
            PathBuf::from("/repo/busy_a.rs"),
            PathBuf::from("/repo/busy_b.rs"),
            PathBuf::from("/repo/busy_c.rs"),
        ],
        "every overflowing change must survive, deduped, with reads filtered out"
    );
    assert!(
        pending.take().is_empty(),
        "take must empty the set so a path is never replayed"
    );
    assert_eq!(
        drain(&rx),
        DEBOUNCE_QUEUE_CAPACITY,
        "the already-queued batches must be untouched by the overflow"
    );
}

/// The queue must not grow with the burst — this is the leak. 10k batches over four paths leave
/// at most `DEBOUNCE_QUEUE_CAPACITY` batches queued plus a four-path union, so the memory tracks
/// how many distinct paths exist rather than how many batches the debouncer emitted.
#[test]
fn should_not_grow_the_queue_beyond_capacity_under_a_burst() {
    let (tx, rx) = mpsc::sync_channel::<WatchMsg>(DEBOUNCE_QUEUE_CAPACITY);
    let pending = PendingPaths::default();
    let paths = ["/repo/a.rs", "/repo/b.rs", "/repo/c.rs", "/repo/d.rs"];

    for _ in 0..10_000 {
        dispatch_debounced(&tx, &pending, batch(MODIFY, &paths));
    }

    assert_eq!(
        drain(&rx),
        DEBOUNCE_QUEUE_CAPACITY,
        "the queue is capped at its capacity"
    );
    assert_eq!(
        pending.take().len(),
        paths.len(),
        "the overflowing batches collapse into one union of their paths"
    );
}

/// A permission-denied directory must not break the watcher's startup. This is the entire
/// point of the PR: with non-fatal watch registration, an unreadable tree is skipped (with a
/// warning) instead of aborting `watch_paths` with a `notify error: Permission denied`.
///
/// The reproduction only bites when the test runs as a non-root user (as in CI); as root the
/// `chmod 000` directory is still readable, so the test passes trivially locally but exercises
/// the EACCES path in CI.
#[cfg(target_os = "linux")]
#[test]
fn should_not_fail_startup_on_unreadable_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize");
    let locked = root.join("locked");
    std::fs::create_dir_all(&locked).expect("mkdir locked");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    }
    let mut config = crate::config::default_for_root(&root);
    config.watch.debounce_ms = 50;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), WatchError>>();
    let root_for_thread = root.clone();
    let handle = std::thread::spawn(move || {
        let result = watch_paths(&root_for_thread, &config, shutdown_rx, |_paths, _kind| {});
        let _ = done_tx.send(result);
    });

    // Give the watcher a moment to arm (registering watches), then shut it down.
    std::thread::sleep(Duration::from_millis(500));
    let _ = shutdown_tx.send(());
    let result = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("watcher thread died");
    let _ = handle.join();
    // Restore read/exec so the TempDir drop can remove the tree.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700));
    }
    assert!(
        result.is_ok(),
        "watch_paths must not fail on an unreadable directory: {result:?}"
    );
}

/// A directory rename is reported as the directory's own path, not one event per child. Both the
/// old and the new directory must reach the callback so the scan can prune one and index the other.
#[test]
fn should_emit_both_directories_when_a_directory_is_renamed() {
    for inotify in [false, true] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize tempdir");
        std::fs::create_dir_all(root.join("old/deep")).expect("mkdir");
        std::fs::write(root.join("old/x.rs"), b"fn x() {}\n").expect("seed");
        std::fs::write(root.join("old/deep/y.rs"), b"fn y() {}\n").expect("seed");
        let mut watch = ScriptedWatch::start(&root, 50);

        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::rename(&old, &new).expect("rename dir");
        watch.feed.rename(&old, &new, inotify);
        let sentinel = root.join("sentinel.rs");
        std::fs::write(&sentinel, b"fn s() {}\n").expect("write sentinel");
        watch
            .feed
            .event(EventKind::Create(notify::event::CreateKind::File), &sentinel);

        let emitted = emitted_through(&watch.paths, &sentinel);
        assert!(
            emitted.contains(&old) && emitted.contains(&new),
            "inotify {inotify}: {emitted:?}"
        );

        watch.stop();
    }
}

/// Records what [`DirWatches`] arms and releases, standing in for the inotify debouncer.
#[derive(Default)]
struct Recorder {
    armed: Vec<PathBuf>,
    released: Vec<PathBuf>,
}

impl DirRegistry for Recorder {
    fn watch_dir(&mut self, dir: &Path) -> Result<(), notify::Error> {
        self.armed.push(dir.to_path_buf());
        Ok(())
    }

    fn unwatch_dir(&mut self, dir: &Path) {
        self.released.push(dir.to_path_buf());
    }
}

/// A directory renamed within the tree releases the watches of its old subtree (the kernel watch
/// follows the inode and would keep reporting the old name) and arms its new subtree, including
/// subdirectories no event named. A directory already armed is not walked again.
#[test]
fn dir_watches_follow_a_renamed_directory_subtree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonicalize tempdir");
    std::fs::create_dir_all(root.join("old/deep")).expect("mkdir");
    std::fs::create_dir_all(root.join("oldish")).expect("mkdir");
    std::fs::create_dir_all(root.join("target/debug")).expect("mkdir");
    let config = crate::config::default_for_root(&root);
    let filter =
        crate::scanner_filter::IndexFilter::new(&root, &config, crate::retrieval_wiring::backend()).expect("filter");
    let mut watches = DirWatches::default();
    let mut registry = Recorder::default();
    watches.sync(&mut registry, &filter, &[root.join("old"), root.join("oldish")]);
    assert_eq!(
        registry.armed,
        [root.join("old"), root.join("old/deep"), root.join("oldish")]
    );

    std::fs::rename(root.join("old"), root.join("new")).expect("rename dir");
    filter.clear_cache(); // as the watcher loop does before every batch ~keep
    let mut registry = Recorder::default();
    watches.sync(
        &mut registry,
        &filter,
        &[
            root.join("new"),
            root.join("old"),
            root.join("oldish"),
            root.join("target"),
        ],
    );
    assert_eq!(registry.released, [root.join("old"), root.join("old/deep")]);
    assert_eq!(registry.armed, [root.join("new"), root.join("new/deep")]);
    assert_eq!(
        watches.watched.iter().collect::<Vec<_>>(),
        [&root.join("new"), &root.join("new/deep"), &root.join("oldish")]
    );
}
