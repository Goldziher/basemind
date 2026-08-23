//! Discovery, subscription, lifecycle, and GC tests split from `daemon_tests.rs`.

use super::*;

#[tokio::test]
async fn discovery_notification_respects_path_scope_and_membership() {
    let (_d, broker) = temp_broker();
    let (matching_tx, mut matching_rx) = mpsc::channel(8);
    let (outside_tx, mut outside_rx) = mpsc::channel(8);
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");

    let mut member = Session::default();
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent("member"),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: Some(workspace.path().to_path_buf()),
            },
            &mut member,
            &matching_tx,
        )
        .await;
    broker
        .handle(CommsRequest::SubscribeInbox { thread: None }, &mut member, &matching_tx)
        .await;

    let mut outsider = Session::default();
    broker
        .handle(
            CommsRequest::Hello {
                agent: agent("outsider"),
                proto_ver: PROTO_VER,
                remote: None,
                cwd: Some(outside.path().to_path_buf()),
            },
            &mut outsider,
            &outside_tx,
        )
        .await;
    broker
        .handle(
            CommsRequest::SubscribeInbox { thread: None },
            &mut outsider,
            &outside_tx,
        )
        .await;

    let mut alice = hello(&broker, &matching_tx, "alice").await;
    broker
        .handle(
            CommsRequest::ThreadStart {
                subject: Some("scoped".to_string()),
                path: Some(format!("{}/**", workspace.path().display())),
                members: vec![agent("member")],
            },
            &mut alice,
            &matching_tx,
        )
        .await;

    assert!(
        matching_rx.try_recv().is_err(),
        "members already receive message notifications"
    );
    assert!(
        outside_rx.try_recv().is_err(),
        "path scope must not leak thread metadata"
    );
}

/// An inbox subscription with `thread: Some(t)` wakes ONLY for posts to `t`, staying silent for a
/// post to another joined thread.
#[tokio::test]
async fn subscribe_inbox_filter_restricts_to_one_thread() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread1 = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let thread2 = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let sub_resp = broker
        .handle(
            CommsRequest::SubscribeInbox {
                thread: Some(thread1.clone()),
            },
            &mut alice,
            &tx,
        )
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));

    post(&broker, &mut bob, &tx, &thread2, "other thread").await;
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "the filter must not wake on a different joined thread"
    );

    post(&broker, &mut bob, &tx, &thread1, "filtered thread").await;
    match rx.recv().await.expect("notification for the filtered thread") {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.thread, thread1, "the filter wakes on its own thread");
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }
}

/// An inbox subscription never wakes on the subscriber's OWN post, mirroring `on_inbox`'s
/// self-exclusion.
#[tokio::test]
async fn subscribe_inbox_skips_self_authored() {
    let (_d, broker) = temp_broker();
    let (tx, mut rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;
    let mut bob = hello(&broker, &tx, "bob").await;

    let sub_resp = broker
        .handle(CommsRequest::SubscribeInbox { thread: None }, &mut alice, &tx)
        .await;
    assert!(matches!(sub_resp, CommsResponse::Subscribed { .. }));

    post(&broker, &mut alice, &tx, &thread, "alice's own post").await;
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a self-authored post must not wake the author's own inbox sink"
    );

    post(&broker, &mut bob, &tx, &thread, "bob's post").await;
    match rx.recv().await.expect("notification for bob's post") {
        CommsOut::Notification(CommsNotification::Message(meta)) => {
            assert_eq!(meta.subject, "bob's post");
        }
        other => panic!("expected a Message notification, got {other:?}"),
    }
}

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

/// The destructive global blob GC must not sweep while a rescan is in flight: a rescan writes new
/// content-addressed blobs before its `index.msgpack` (which the GC reference-counts) is rewritten,
/// so a mid-rescan sweep would see those blobs as orphans and reap them. `on_rescan` holds the
/// blob-GC READ lock for the whole scan; `run_blob_gc` takes the WRITE lock — so the sweep blocks
/// until the rescan releases.
#[tokio::test]
async fn blob_gc_waits_for_an_in_flight_rescan() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    let rescan_guard = broker.blob_gc_lock.read().await;

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
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    broker.run_blob_gc().await.expect("seed a completed sweep");
    let completed = crate::store_gc::read_gc_state().expect("completed sweep state");

    let _rescan_guard = broker.blob_gc_lock.read().await;

    let result = broker.run_blob_gc_with_lock_timeout(Duration::from_millis(50)).await;
    assert!(
        matches!(result, Err(crate::store_gc::GcError::Starved(_))),
        "a sweep that cannot win the lock within its bound reports Starved, got {result:?}"
    );
    let state = crate::store_gc::read_gc_state().expect("a starved attempt must be persisted");
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
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();

    broker.run_blob_gc().await.expect("sweep");

    let state = crate::store_gc::read_gc_state().expect("gc-state.json must exist after a completed sweep");
    assert!(state.at_epoch_secs > 0, "the sweep timestamp is recorded");
    assert_eq!(state.last_attempt_epoch_secs, state.at_epoch_secs);
    assert_eq!(state.status, crate::store_gc::GcStatus::Completed);
}

/// End-to-end correctness (not just lock timing): racing a real full rescan against the destructive
/// global blob sweep, repeatedly, must never leave the index pointing at a reaped blob. A rescan
/// writes fresh content-addressed blobs but only rewrites `index.msgpack` (which the sweep
/// reference-counts) at completion, so its just-written blobs are unreferenced for the whole scan;
/// without the `blob_gc_lock` serialization a sweep landing mid-scan would reap them. The invariant
/// checked here is the outcome, which holds under any interleaving: every blob the final index
/// references still exists on disk.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_rescan_and_blob_gc_never_reaps_a_referenced_blob() {
    crate::store::init_isolated_cache();
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);

    let ws = tempfile::tempdir().expect("workspace");
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
    for stem in &referenced {
        let prefix = format!("{stem}.");
        let present = std::fs::read_dir(&blobs_dir)
            .expect("read blobs dir")
            .flatten()
            .any(|entry| entry.file_name().to_str().is_some_and(|name| name.starts_with(&prefix)));
        assert!(present, "referenced blob {stem} was reaped by a concurrent GC sweep");
    }
}

/// The system auto-archive sweep flips an idle active thread; a fresh one stays active.
#[tokio::test]
async fn archive_idle_threads_flips_stale_active_threads() {
    let (_d, broker) = temp_broker();
    let (tx, _rx) = mpsc::channel(8);
    let mut alice = hello(&broker, &tx, "alice").await;
    let thread = start_thread(&broker, &mut alice, &tx, &["bob"]).await;

    let mut record = broker.store.get_thread(&thread).unwrap().unwrap();
    record.last_activity = now_micros() - 30 * 24 * 60 * 60 * 1_000_000;
    broker.store.put_thread(&record).unwrap();

    let archived = broker.archive_idle_threads(THREAD_IDLE_TTL).expect("archive");
    assert_eq!(archived, 1);
    assert!(!broker.store.get_thread(&thread).unwrap().unwrap().active);
}
