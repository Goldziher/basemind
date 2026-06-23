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

/// Run the broker loop. Binds the singleton endpoint (the bind IS the lock), opens the store,
/// serves the platform front-end, and blocks until SIGTERM / Ctrl-C / a `Stop` RPC.
pub fn run() -> Result<()> {
    let paths = singleton::resolve_paths().context("resolve comms paths")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    runtime.block_on(async move {
        // Bind, open the store, and build the broker INSIDE the runtime: `bind_listener`
        // converts a std listener via `tokio::net::UnixListener::from_std` (or creates the first
        // named-pipe instance on Windows), which requires a live reactor — calling it before
        // entering the runtime panics ("no reactor running"). The bind IS the singleton lock;
        // probe before reclaiming a stale endpoint.
        let listener = match singleton::bind_listener(&paths.socket_path, singleton::probe_alive) {
            Ok(listener) => listener,
            Err(singleton::SingletonError::AlreadyRunning(p)) => {
                tracing::info!(socket = %p.display(), "comms daemon already running; exiting");
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("bind comms socket: {e}")),
        };

        let store = Arc::new(CommsStore::open(&paths.comms_dir).context("open comms store")?);
        let broker = Arc::new(Broker::new(store));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Signal handling: SIGTERM / Ctrl-C begins the drain.
        let broker_for_signal = broker.clone();
        let shutdown_for_signal = shutdown_tx.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("comms: shutdown signal received; draining");
            broker_for_signal.begin_drain().await;
            let _ = shutdown_for_signal.send(true);
        });

        // Idle reaper: self-terminate once the daemon has no connected links and no activity
        // for `IDLE_REAP_AFTER`, so a daemon orphaned by a dead session does not linger. Drives
        // the same clean drain path as a `Stop` RPC / SIGTERM.
        let broker_for_reaper = broker.clone();
        let shutdown_for_reaper = shutdown_tx.clone();
        tokio::spawn(async move {
            use crate::comms::daemon::{IDLE_REAP_AFTER, IDLE_REAP_CHECK_EVERY};
            let mut tick = tokio::time::interval(IDLE_REAP_CHECK_EVERY);
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if broker_for_reaper.is_idle_for(IDLE_REAP_AFTER).await {
                    tracing::info!(
                        "comms: idle with no clients past the reap window; self-terminating"
                    );
                    broker_for_reaper.begin_drain().await;
                    let _ = shutdown_for_reaper.send(true);
                    break;
                }
            }
        });

        // The bound endpoint is platform-specific: a `UnixListener` on Unix, a first
        // `NamedPipeServer` instance on Windows. Each gets a tiny object-safe shim so the daemon
        // dispatches through the same `CommsFrontendObj` trait object.
        #[cfg(unix)]
        let frontend: Box<dyn CommsFrontendObj> = Box::new(UdsFrontendBox(
            crate::comms::frontend_uds::UdsFrontend::from_listener(
                listener,
                paths.socket_path.clone(),
            ),
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
    })?;
    Ok(())
}

// `CommsFrontend::serve` uses RPITIT and is not object-safe, so wrap it behind a tiny object-safe
// shim for the single dynamic dispatch in the daemon entry point.
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
