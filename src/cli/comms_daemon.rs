//! Broker-daemon entry point (`basemind comms daemon`).
//!
//! Unlike [`comms`](super::comms) — the agent-comms *client* verbs — this module runs the broker
//! *server*: it binds the singleton endpoint (the bind IS the lock), opens the store, and serves
//! the platform front-end (Unix-domain socket on Unix, named pipe on Windows) until SIGTERM /
//! Ctrl-C / a `Stop` RPC drains it. Kept out of `main.rs` so the CLI entry stays under the
//! module-size cap as the cross-platform transports grow.

#![cfg(all(feature = "comms", any(unix, windows)))]

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::comms::daemon::Broker;
use crate::comms::singleton;
use crate::comms::store::CommsStore;

/// How often the message-TTL sweep runs. Hourly is ample: messages already drop out of the
/// default 24h recency reads long before [`MESSAGE_TTL`](crate::comms::store::MESSAGE_TTL).
const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How often the global blob GC + budget sweep runs, on its OWN task — deliberately not part of
/// the prune loop. The GC can block up to its lock-timeout behind a long rescan (it takes the
/// blob-GC write lock; every rescan holds the read side), and when it shared a loop with the
/// cheap prunes one starved GC silently stopped ALL maintenance — the 116 GB incident.
const GC_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How often the Unix socket-ownership watchdog verifies we still own our bound socket. Short, so
/// an orphaned daemon (its socket reclaimed by another) self-terminates within seconds.
#[cfg(unix)]
const OWNERSHIP_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Hard bound on the runtime teardown at the end of [`run`]. Dropping the runtime implicitly waits
/// *forever* for in-flight `spawn_blocking` work — which is exactly how a SIGTERM'd daemon used to
/// hang until SIGKILL while a big rescan finished. The drain already trips the broker's scan-cancel
/// token, so a scan winds down within one file; this timeout is the backstop for the pathological
/// case where that one file is stuck inside non-cooperative work (e.g. an ONNX embed call).
const RUNTIME_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The `(device, inode)` identity of the socket file, or `None` when it is absent / unstattable.
/// The ownership watchdog compares this against the value captured at bind time to detect an
/// unlink-and-rebind reclaim by another daemon.
#[cfg(unix)]
fn socket_inode(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

/// Run the broker loop. Binds the singleton endpoint (the bind IS the lock), opens the store,
/// serves the platform front-end, and blocks until SIGTERM / Ctrl-C / a `Stop` RPC.
pub fn run() -> Result<()> {
    let paths = singleton::resolve_paths().context("resolve comms paths")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let result = runtime.block_on(async move {
        // Single-owner lock FIRST, before bind: a redundant daemon converges (exit 0) without ever
        // touching the live daemon's socket. This — plus the machine-registry entry it writes — is
        // the authoritative "one daemon per comms dir" gate, uniform across Unix and Windows.
        use crate::comms::daemon_lock::{DaemonLock, DaemonLockOutcome};
        let _daemon_lock = match DaemonLock::acquire(&paths.comms_dir, env!("CARGO_PKG_VERSION")) {
            Ok(DaemonLockOutcome::Acquired(lock)) => lock,
            Ok(DaemonLockOutcome::AlreadyHeld(holder)) => {
                tracing::info!(
                    holder_pid = holder.as_ref().map(|record| record.pid),
                    "comms daemon: another daemon already owns this comms dir; converging (exit 0)"
                );
                return Ok(());
            }
            Err(error) => return Err(anyhow::anyhow!("acquire daemon lock: {error}")),
        };

        let listener = match singleton::bind_listener(&paths.socket_path, singleton::probe_alive) {
            Ok(listener) => listener,
            Err(singleton::SingletonError::AlreadyRunning(p)) => {
                tracing::info!(socket = %p.display(), "comms daemon already running; exiting");
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("bind comms socket: {e}")),
        };

        let store = match CommsStore::open(&paths.comms_dir) {
            Ok(store) => Arc::new(store),
            // The store flock is the second per-comms-dir guard. If a peer holds it we lost the race
            // (e.g. a socket false-reclaim): converge quietly rather than exiting non-zero.
            Err(crate::comms::store::CommsStoreError::Locked(path)) => {
                tracing::info!(
                    path = %path.display(),
                    "comms daemon: store already owned by another daemon; converging (exit 0)"
                );
                return Ok(());
            }
            Err(error) => return Err(anyhow::anyhow!("open comms store: {error}")),
        };
        log_retention_policy();
        run_retention_maintenance(&store);
        let machine_registry = match crate::registry::Registry::from_data_home() {
            Ok(registry) => registry,
            Err(error) => {
                tracing::warn!(%error, "comms: machine registry open failed; coordination tools degrade to empty");
                crate::registry::Registry::open(&paths.comms_dir.join("registry-fallback"))
                    .context("open fallback machine registry")?
            }
        };
        let broker = Arc::new(Broker::with_registry(store.clone(), machine_registry));
        broker.install_comms_dir(paths.comms_dir.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        broker.install_shutdown(shutdown_tx);

        let broker_for_signal = broker.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("comms: shutdown signal received; draining");
            broker_for_signal.begin_drain().await;
        });

        let broker_for_reaper = broker.clone();
        tokio::spawn(async move {
            let idle_after = crate::comms::daemon::idle_reap_after();
            let mut tick = tokio::time::interval(crate::comms::daemon::idle_reap_check_every());
            tick.tick().await;
            loop {
                tick.tick().await;
                if broker_for_reaper.try_begin_idle_drain(idle_after).await {
                    tracing::info!(
                        idle_after_secs = idle_after.as_secs(),
                        "comms: idle with no clients past the reap window; self-terminating"
                    );
                    break;
                }
            }
        });

        // Bootstrap reaper: a daemon nobody ever connects to (a spawn-and-abandon from a test or a
        // script that ran `daemon ensure` then exited) self-terminates after one grace window,
        // instead of lingering the full idle window. One-shot: once any client has touched either
        // transport, the idle reaper above governs from then on.
        let broker_for_bootstrap = broker.clone();
        tokio::spawn(async move {
            let bootstrap = crate::comms::daemon::bootstrap_timeout();
            tokio::time::sleep(bootstrap).await;
            if !broker_for_bootstrap.ever_served() {
                tracing::info!(
                    bootstrap_secs = bootstrap.as_secs(),
                    "comms: no client connected within the bootstrap window; self-terminating"
                );
                broker_for_bootstrap.begin_drain().await;
            }
        });

        let store_for_prune = store.clone();
        let broker_for_prune = broker.clone();
        tokio::spawn(async move {
            use crate::comms::daemon::WORKSPACE_HOT_TTL;
            let mut tick = tokio::time::interval(PRUNE_EVERY);
            tick.tick().await;
            // Sweep once at startup for the same reason the blob GC does: a daemon that restarts ~keep
            // more often than PRUNE_EVERY would otherwise never retire a single stale row. ~keep
            prune_missing_registry_rows(&broker_for_prune).await;
            loop {
                tick.tick().await;
                run_retention_maintenance(&store_for_prune);
                let evicted = broker_for_prune.evict_idle_workspaces(WORKSPACE_HOT_TTL);
                if evicted > 0 {
                    tracing::info!(evicted, "daemon: shed idle hot workspaces from RAM");
                }
                prune_missing_registry_rows(&broker_for_prune).await;
            }
        });

        // Cross-workspace blob GC over the machine-global store: reference-count against ~keep
        // EVERY workspace and reap blobs no workspace points at. Safe only here — the daemon ~keep
        // is the sole caller that sees all references. Routed through the broker so it takes ~keep
        // the blob-GC write lock and never sweeps while a rescan is writing fresh blobs. ~keep
        // Runs on its own task — never sharing a loop with the cheap prunes above — and sweeps ~keep
        // once at startup: a daemon that restarts more often than GC_EVERY must still reclaim, ~keep
        // and a GC starved behind a rescan must never stall the rest of the maintenance. ~keep
        let broker_for_gc = broker.clone();
        tokio::spawn(async move {
            run_gc_cycle(&broker_for_gc).await;
            let mut tick = tokio::time::interval(GC_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                run_gc_cycle(&broker_for_gc).await;
            }
        });

        #[cfg(unix)]
        if let Some(bound_inode) = socket_inode(&paths.socket_path) {
            let broker_for_owner = broker.clone();
            let socket = paths.socket_path.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(OWNERSHIP_CHECK_EVERY);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if socket_inode(&socket) != Some(bound_inode) {
                        tracing::warn!(
                            socket = %socket.display(),
                            "comms: socket unlinked or replaced by another daemon; self-terminating"
                        );
                        broker_for_owner.begin_drain().await;
                        break;
                    }
                }
            });
        }

        // Second MCP front-end: the stateless streamable-HTTP transport, hosted alongside the ~keep
        // UDS relay on the SAME daemon. Additive — a bind failure logs loudly and leaves the UDS ~keep
        // relay running; HTTP is never a precondition for comms. Tied to the same shutdown watch. ~keep
        let broker_for_http = broker.clone();
        let http_comms_dir = paths.comms_dir.clone();
        let http_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(error) =
                crate::comms::http_frontend::serve_http(broker_for_http, http_comms_dir, http_shutdown).await
            {
                tracing::error!(%error, "comms: streamable-HTTP MCP front-end failed to start");
            }
        });

        #[cfg(unix)]
        let frontend: Box<dyn CommsFrontendObj> = Box::new(UdsFrontendBox(
            crate::comms::frontend_uds::UdsFrontend::from_listener(listener, paths.socket_path.clone()),
        ));
        #[cfg(windows)]
        let frontend: Box<dyn CommsFrontendObj> = Box::new(NamedPipeFrontendBox(
            crate::comms::frontend_named_pipe::NamedPipeFrontend::from_first_instance(
                listener,
                paths.socket_path.clone().into_os_string(),
            ),
        ));
        let broker_for_exit = broker.clone();
        frontend
            .serve_obj(broker, shutdown_rx)
            .await
            .context("comms front-end serve loop")?;
        // A daemon that gave up because its store became permanently unusable did NOT stop cleanly.
        // Exiting non-zero is what distinguishes it from an idle reap or a `Stop` RPC for whatever
        // supervises the process; the drain that got us here has already released the flock and the
        // socket, so the next `comms start` gets a clean daemon. See `Broker::hit_fatal_store_failure`.
        if broker_for_exit.hit_fatal_store_failure() {
            let detail = crate::comms::store_health::read(&paths.comms_dir)
                .map(|record| record.error)
                .unwrap_or_else(|| "see the daemon log".to_string());
            return Err(anyhow::anyhow!(
                "comms store is permanently unusable ({detail}); released the daemon lock and exited so a clean daemon can take over"
            ));
        }
        Ok(())
    });
    // Bounded teardown instead of the implicit unbounded drop — see [`RUNTIME_SHUTDOWN_TIMEOUT`]. ~keep
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result?;
    Ok(())
}

trait CommsFrontendObj: Send {
    fn serve_obj(
        self: Box<Self>,
        broker: Arc<Broker>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>;
}

#[cfg(unix)]
struct UdsFrontendBox(crate::comms::frontend_uds::UdsFrontend);

#[cfg(unix)]
impl CommsFrontendObj for UdsFrontendBox {
    fn serve_obj(
        self: Box<Self>,
        broker: Arc<Broker>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> {
        use crate::comms::transport::CommsFrontend;
        Box::pin(async move { Box::new(self.0).serve(broker, shutdown).await })
    }
}

#[cfg(windows)]
struct NamedPipeFrontendBox(crate::comms::frontend_named_pipe::NamedPipeFrontend);

#[cfg(windows)]
impl CommsFrontendObj for NamedPipeFrontendBox {
    fn serve_obj(
        self: Box<Self>,
        broker: Arc<Broker>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> {
        use crate::comms::transport::CommsFrontend;
        Box::pin(async move { Box::new(self.0).serve(broker, shutdown).await })
    }
}

/// Retire machine-registry rows whose on-disk path is gone, logging only when something was
/// actually reclaimed so a quiet machine stays quiet in the log.
async fn prune_missing_registry_rows(broker: &Broker) {
    let removed = broker.prune_missing_registry_rows().await;
    if removed > 0 {
        tracing::info!(removed, "daemon: pruned machine-registry rows whose path is gone");
    }
}

/// Apply the effective machine-global comms retention policy. Called at startup and hourly so a
/// frequently restarted daemon still converges, and so policy is visible in structured logs.
fn run_retention_maintenance(store: &CommsStore) {
    use crate::comms::store::{EPHEMERAL_AGENT_TTL, MESSAGE_TTL, THREAD_IDLE_TTL, THREAD_RETENTION_TTL};

    let mut succeeded = true;
    match store.prune_expired(MESSAGE_TTL) {
        Ok(count) if count > 0 => tracing::info!(pruned = count, "comms: pruned expired messages"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: message prune failed");
        }
    }
    match store.archive_idle(THREAD_IDLE_TTL) {
        Ok(count) if count > 0 => tracing::info!(archived = count, "comms: archived idle threads"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: thread archive failed");
        }
    }
    match store.purge_archived(THREAD_RETENTION_TTL) {
        Ok(count) if count > 0 => tracing::info!(purged = count, "comms: purged archived threads"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: archived-thread purge failed");
        }
    }
    match store.prune_ephemeral_agents(EPHEMERAL_AGENT_TTL) {
        Ok(count) if count > 0 => tracing::info!(pruned = count, "comms: pruned stale ephemeral agents"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: ephemeral-agent prune failed");
        }
    }
    match crate::comms::identity::prune_expired_claims(crate::comms::identity::CLAIM_TTL) {
        Ok(count) if count > 0 => tracing::info!(pruned = count, "comms: pruned stale identity claims"),
        Ok(_) => {}
        Err(error) => {
            succeeded = false;
            tracing::warn!(%error, "comms: identity-claim prune failed");
        }
    }
    if succeeded && let Err(error) = store.record_maintenance() {
        tracing::warn!(%error, "comms: record maintenance completion failed");
    }
}

fn log_retention_policy() {
    use crate::comms::store::{
        EPHEMERAL_AGENT_TTL, MAX_ACTIVE_THREADS, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, THREAD_IDLE_TTL,
        THREAD_RETENTION_TTL,
    };
    tracing::info!(
        message_ttl_secs = MESSAGE_TTL.as_secs(),
        max_messages_per_thread = MAX_MESSAGES_PER_THREAD,
        thread_idle_ttl_secs = THREAD_IDLE_TTL.as_secs(),
        thread_retention_ttl_secs = THREAD_RETENTION_TTL.as_secs(),
        ephemeral_agent_ttl_secs = EPHEMERAL_AGENT_TTL.as_secs(),
        claim_ttl_secs = crate::comms::identity::CLAIM_TTL.as_secs(),
        max_active_threads = MAX_ACTIVE_THREADS,
        "comms: effective retention policy"
    );
}

/// One blob-GC + budget sweep, with every outcome logged: reclaim at info, a starved cycle at
/// warn (skipped, retried next tick — never wedged), any other failure at warn.
async fn run_gc_cycle(broker: &Broker) {
    match broker.run_blob_gc().await {
        Ok(report) if report.removed > 0 || report.workspaces_reaped > 0 || report.workspaces_evicted > 0 => {
            tracing::info!(
                removed = report.removed,
                bytes_freed = report.bytes_freed,
                workspaces_reaped = report.workspaces_reaped,
                workspace_bytes_freed = report.workspace_bytes_freed,
                workspaces_evicted = report.workspaces_evicted,
                evicted_bytes_freed = report.evicted_bytes_freed,
                "daemon: reclaimed global cache"
            );
        }
        Ok(_) => {}
        Err(error @ crate::store_gc::GcError::Starved(_)) => {
            tracing::warn!(%error, "daemon: blob GC starved this cycle; retrying on the next tick");
        }
        Err(error) => tracing::warn!(%error, "daemon: global blob GC failed"),
    }
}

/// Block until SIGTERM or Ctrl-C.
#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Block until Ctrl-C (or Ctrl-Break). Windows has no SIGTERM; `ctrl_c` covers the console
/// signals and a `Stop` RPC drives the same drain path independently.
#[cfg(windows)]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::socket_inode;

    #[test]
    fn socket_inode_identifies_a_file_and_reports_replacement_or_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.sock");
        let b = dir.path().join("b.sock");
        std::fs::write(&a, b"").expect("write a");
        std::fs::write(&b, b"").expect("write b");

        let ident_a = socket_inode(&a).expect("a exists");
        assert_eq!(socket_inode(&a), Some(ident_a), "identity is stable across stats");
        assert_ne!(
            socket_inode(&b),
            Some(ident_a),
            "a distinct file must not match our bound identity"
        );
        std::fs::remove_file(&a).expect("unlink a");
        assert_eq!(socket_inode(&a), None, "an unlinked socket reports absence");
    }
}
