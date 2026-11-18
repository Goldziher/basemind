//! Lifecycle and GC tests split from `daemon_tests.rs`.

use super::*;

#[tokio::test]
async fn idle_reaper_tracks_links_and_activity() {
    let (_d, broker) = temp_broker();

    assert!(broker.is_idle_for(Duration::ZERO).await);
    broker.link_connected();
    assert!(!broker.is_idle_for(Duration::ZERO).await);
    broker.link_disconnected();
    assert!(broker.is_idle_for(Duration::ZERO).await);
    assert!(!broker.is_idle_for(Duration::from_secs(3600)).await);
    broker.begin_drain().await;
    assert!(!broker.is_idle_for(Duration::ZERO).await);
}

/// A link whose task PANICS must still give its refcount back — otherwise `link_count` never returns
/// to zero, `is_idle_for` is false forever, and that daemon can never reap again for the rest of its
/// life. This is the immortal-daemon bug: one panicking request handler permanently pins the process.
///
/// It is exactly why the refcount is an RAII [`LinkGuard`] and not a `link_disconnected()` call after
/// the serve loop — a plain statement after the loop is skipped by an unwind, a `Drop` is not.
#[tokio::test]
async fn a_panicking_link_task_gives_its_refcount_back_and_the_daemon_can_still_reap() {
    use crate::comms::transport::{CommsLink, PeerCred, serve_link};

    /// A link that blows up the moment the serve loop polls it.
    struct PanickingLink;

    impl CommsLink for PanickingLink {
        async fn recv(&mut self) -> std::io::Result<Option<CommsRequest>> {
            panic!("handler blew up mid-request");
        }
        async fn send(&mut self, _out: CommsOut) -> std::io::Result<()> {
            Ok(())
        }
        fn peer_cred(&self) -> PeerCred {
            PeerCred::default()
        }
    }

    let (_d, broker) = temp_broker();
    let guard = broker.register_link();
    assert!(
        !broker.is_idle_for(Duration::ZERO).await,
        "a registered link means the daemon is not idle"
    );

    let joined = tokio::spawn(serve_link(broker.clone(), PanickingLink, guard)).await;
    assert!(joined.is_err(), "the link task must actually have panicked");

    assert!(
        broker.is_idle_for(Duration::ZERO).await,
        "the panicking link must have released its refcount on unwind — if it leaks, link_count \
         never returns to zero and this daemon is immortal"
    );
    assert!(
        broker.try_begin_idle_drain(Duration::ZERO).await,
        "and the reaper must still be able to drain it"
    );
}

/// Daemon-internal work with NO client attached still blocks the reap. This is the clause that keeps
/// the idle reaper from tearing down the process mid-blob-GC: that sweep holds no link, so without
/// the work refcount a daemon running it would look perfectly idle.
#[tokio::test]
async fn work_in_flight_blocks_the_idle_reap_even_with_no_links() {
    let (_d, broker) = temp_broker();

    assert!(broker.is_idle_for(Duration::ZERO).await, "no links, no work: idle");
    assert_eq!(broker.work_inflight(), 0);

    {
        let _working = broker.begin_work();
        assert_eq!(broker.work_inflight(), 1);
        assert!(
            !broker.is_idle_for(Duration::ZERO).await,
            "work in flight must defeat idleness even though zero links are connected"
        );
        assert!(
            !broker.try_begin_idle_drain(Duration::ZERO).await,
            "the reaper must refuse to start a drain while work is in flight"
        );
    }

    assert_eq!(broker.work_inflight(), 0, "the guard releases the count on drop");
    assert!(
        broker.is_idle_for(Duration::ZERO).await,
        "once the work finishes the daemon is idle again"
    );
    assert!(
        broker.try_begin_idle_drain(Duration::ZERO).await,
        "and now the reaper may claim the drain"
    );
    assert!(
        !broker.try_begin_idle_drain(Duration::ZERO).await,
        "only one caller ever owns the drain — a second attempt is a no-op"
    );
}

/// Every blob sweep records its outcome in the one GC state file of the process-wide test cache,
/// so the tests that run a sweep and then assert on that file take turns. ~keep
static GC_STATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Hold the blob-GC gate the way a running scan does, on a thread of its own, until the returned
/// sender is dropped.
fn hold_gate_as_a_scan(broker: &Broker) -> std::sync::mpsc::Sender<()> {
    let gate = broker.workspaces.blob_gc_gate();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (holding, held) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _scanning = gate.scanning();
        let _ = holding.send(());
        let _ = released.recv();
    });
    held.recv().expect("the gate is held");
    release
}

/// The destructive global blob GC must not sweep while a rescan is in flight: a rescan writes new
/// content-addressed blobs before its `index.msgpack` (which the GC reference-counts) is rewritten,
/// so a mid-rescan sweep would see those blobs as orphans and reap them. A scan holds the blob-GC
/// gate shared for its whole duration; `run_blob_gc` takes it exclusively — so the sweep waits
/// until the scan releases.
#[tokio::test]
async fn blob_gc_waits_for_an_in_flight_rescan() {
    let _one_sweep = GC_STATE.lock().await;
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    let rescan_guard = hold_gate_as_a_scan(&broker);

    let mut gc = std::pin::pin!(broker.run_blob_gc());
    tokio::select! {
        biased;
        _ = &mut gc => panic!("blob GC swept while a rescan held the blob-GC read lock"),
        _ = tokio::time::sleep(Duration::from_millis(150)) => {}
    }

    drop(rescan_guard);
    gc.await.expect("blob GC runs once no rescan holds the read lock");
}

/// The wait for the blob-GC write lock is BOUNDED: behind a rescan that never ends (the runaway
/// case), the sweep must come back `Starved` so the GC task skips the cycle and retries later —
/// an unbounded wait here is how the maintenance loop silently parked forever while the cache
/// grew to 116 GB.
#[tokio::test]
async fn blob_gc_returns_starved_instead_of_hanging_behind_an_endless_rescan() {
    let _one_sweep = GC_STATE.lock().await;
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    broker.run_blob_gc().await.expect("seed a completed sweep");
    let completed =
        crate::store_gc::read_gc_state(&crate::data_root::DataRoot::current()).expect("completed sweep state");

    let _rescan_guard = hold_gate_as_a_scan(&broker);

    let result = broker.run_blob_gc_with_lock_timeout(Duration::from_millis(50)).await;
    assert!(
        matches!(result, Err(crate::store_gc::GcError::Starved(_))),
        "a sweep that cannot win the lock within its bound reports Starved, got {result:?}"
    );
    let state = crate::store_gc::read_gc_state(&crate::data_root::DataRoot::current())
        .expect("a starved attempt must be persisted");
    assert_eq!(state.status, crate::store_gc::GcStatus::Starved);
    assert_eq!(
        state.at_epoch_secs, completed.at_epoch_secs,
        "a failed attempt preserves the last completed sweep timestamp"
    );
    assert!(state.last_attempt_epoch_secs > 0, "the failed attempt is timestamped");
    assert_eq!(state.consecutive_degraded_cycles, 1);
    assert!(
        state
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("rescan held the store lock")),
        "the diagnosis names the contending operation: {:?}",
        state.detail
    );
}

/// Every completed destructive sweep records its outcome to `gc-state.json`, so `cache_stats`
/// can show WHEN GC last actually ran — the observability whose absence let the starved-GC
/// incident go unnoticed.
#[tokio::test]
async fn a_completed_sweep_persists_gc_state() {
    let _one_sweep = GC_STATE.lock().await;
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    broker.run_blob_gc().await.expect("sweep");

    let state = crate::store_gc::read_gc_state(&crate::data_root::DataRoot::current())
        .expect("gc-state.json must exist after a completed sweep");
    assert!(state.at_epoch_secs > 0, "the sweep timestamp is recorded");
    assert_eq!(state.last_attempt_epoch_secs, state.at_epoch_secs);
    assert_eq!(state.status, crate::store_gc::GcStatus::Completed);
}

/// End-to-end correctness (not just lock timing): racing a real full rescan against the destructive
/// global blob sweep, repeatedly, must never leave the index pointing at a reaped blob. A rescan
/// writes fresh content-addressed blobs but only rewrites `index.msgpack` (which the sweep
/// reference-counts) at completion, so its just-written blobs are unreferenced for the whole scan;
/// without the blob-GC gate a sweep landing mid-scan would reap them. The invariant
/// checked here is the outcome, which holds under any interleaving: every blob the final index
/// references still exists on disk.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_rescan_and_blob_gc_never_reaps_a_referenced_blob() {
    let _one_sweep = GC_STATE.lock().await;
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let tx = CancellationToken::new();

    // The workspace-root allow-list (issue #62) only opens a project root; `.git/` is invisible to
    // the scanner, so the blob accounting this test asserts is unchanged by the init.
    let ws = tempfile::tempdir().expect("workspace");
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(ws.path())
        .status()
        .expect("run git init");
    assert!(status.success(), "git init succeeds");
    for i in 0..12 {
        std::fs::write(
            ws.path().join(format!("m{i}.rs")),
            format!("pub fn f{i}() -> u32 {{ {i} }}\npub struct S{i};\n"),
        )
        .expect("write source");
    }
    let root = ws.path().to_path_buf();

    for _ in 0..6 {
        let mut session = Session::default();
        let rescan = broker.handle(
            CommsRequest::Rescan {
                root: root.clone(),
                paths: None,
                full: true,
                embed: false,
                verify: false,
            },
            &mut session,
            &tx,
        );
        let (rescan_resp, gc_res) = tokio::join!(rescan, broker.run_blob_gc());
        assert!(
            matches!(rescan_resp, CommsResponse::Rescanned { .. }),
            "each raced rescan must succeed, got {rescan_resp:?}"
        );
        gc_res.expect("blob GC must succeed under a concurrent rescan");
    }

    let basemind_dir = crate::store::workspace_cache_dir(&root);
    let referenced = crate::store_gc::collect_referenced_hashes(&basemind_dir).expect("collect referenced hashes");
    assert!(
        !referenced.is_empty(),
        "the scanned workspace must reference at least one blob"
    );
    let blobs_dir = crate::store::global_blobs_dir();
    for stem in referenced.stems() {
        let prefix = format!("{stem}.");
        let present = std::fs::read_dir(&blobs_dir)
            .expect("read blobs dir")
            .flatten()
            .any(|entry| entry.file_name().to_str().is_some_and(|name| name.starts_with(&prefix)));
        assert!(present, "referenced blob {stem} was reaped by a concurrent GC sweep");
    }
}
