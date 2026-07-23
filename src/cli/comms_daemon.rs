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
        let listener = match singleton::bind_listener(&paths.socket_path, singleton::probe_alive) {
            Ok(listener) => listener,
            Err(singleton::SingletonError::AlreadyRunning(p)) => {
                tracing::info!(socket = %p.display(), "comms daemon already running; exiting");
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("bind comms socket: {e}")),
        };

        let store = Arc::new(CommsStore::open(&paths.comms_dir).context("open comms store")?);
        match store.prune_expired(crate::comms::store::MESSAGE_TTL) {
            Ok(n) if n > 0 => {
                tracing::info!(pruned = n, "comms: pruned expired messages on startup")
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "comms: startup message prune failed"),
        }
        let machine_registry = match crate::registry::Registry::from_data_home() {
            Ok(registry) => registry,
            Err(error) => {
                tracing::warn!(%error, "comms: machine registry open failed; coordination tools degrade to empty");
                crate::registry::Registry::open(&paths.comms_dir.join("registry-fallback"))
                    .context("open fallback machine registry")?
            }
        };
        let broker = Arc::new(Broker::with_registry(store.clone(), machine_registry));

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

        let store_for_prune = store.clone();
        let broker_for_prune = broker.clone();
        tokio::spawn(async move {
            use crate::comms::daemon::{THREAD_IDLE_TTL, THREAD_RETENTION_TTL, WORKSPACE_HOT_TTL};
            let mut tick = tokio::time::interval(PRUNE_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                match store_for_prune.prune_expired(crate::comms::store::MESSAGE_TTL) {
                    Ok(n) if n > 0 => tracing::info!(pruned = n, "comms: pruned expired messages"),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "comms: periodic message prune failed"),
                }
                // Thread lifecycle: auto-archive idle active threads, then reclaim the storage of ~keep
                // threads that have been archived well past the retention window. ~keep
                match broker_for_prune.archive_idle_threads(THREAD_IDLE_TTL) {
                    Ok(n) if n > 0 => tracing::info!(archived = n, "comms: archived idle threads"),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "comms: periodic thread archive failed"),
                }
                match broker_for_prune.purge_archived_threads(THREAD_RETENTION_TTL) {
                    Ok(n) if n > 0 => tracing::info!(purged = n, "comms: purged expired archived threads"),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "comms: periodic archived-thread purge failed"),
                }
                let evicted = broker_for_prune.evict_idle_workspaces(WORKSPACE_HOT_TTL);
                if evicted > 0 {
                    tracing::info!(evicted, "daemon: shed idle hot workspaces from RAM");
                }
                // Cross-workspace blob GC over the machine-global store: reference-count against ~keep
                // EVERY workspace and reap blobs no workspace points at. Safe only here — the daemon ~keep
                // is the sole caller that sees all references. Routed through the broker so it takes ~keep
                // the blob-GC write lock and never sweeps while a rescan is writing fresh blobs. ~keep
                match broker_for_prune.run_blob_gc().await {
                    Ok(report) if report.removed > 0 => tracing::info!(
                        removed = report.removed,
                        bytes_freed = report.bytes_freed,
                        "daemon: reclaimed orphaned global blobs"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "daemon: global blob GC failed"),
                }
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
        frontend
            .serve_obj(broker, shutdown_rx)
            .await
            .context("comms front-end serve loop")
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
