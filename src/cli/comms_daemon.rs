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

use crate::comms::agents_hub::AgentHub;
use crate::comms::daemon::{Broker, EXIT_TAIL_TIMEOUT, HOLDER_DRAIN_WAIT, RUNTIME_SHUTDOWN_TIMEOUT};
use crate::comms::singleton;

/// How often the registry prune and the hot-workspace eviction run.
const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How often the agent store's sweep and flush run (ADR-0021). Each sweep removes a bounded batch,
/// so it runs often enough to keep up with a busy store.
const AGENT_SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// How often the hosted-read-stack sweep runs. Two orders tighter than [`PRUNE_EVERY`] because it
/// sheds something two orders larger: an hourly clock would leave an O(corpus) read stack resident
/// for up to an hour past its last use, which is exactly the residency the sweep exists to bound.
const READ_STACK_SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the global blob GC + budget sweep runs, on its OWN task — deliberately not part of
/// the prune loop. The GC can block up to its lock-timeout behind a long rescan (it takes the
/// blob-GC write lock; every rescan holds the read side), and when it shared a loop with the
/// cheap prunes one starved GC silently stopped ALL maintenance — the 116 GB incident.
const GC_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Thread-pool caps applied to the daemon process: rayon's global pool (and the scanner pool when
/// `scan_threads` is unset) and lance's CPU runtime.
const DAEMON_POOL_ENV: [(&str, &str); 2] = [("RAYON_NUM_THREADS", "6"), ("LANCE_CPU_THREADS", "2")];

/// Async worker threads for the daemon runtime.
const DAEMON_ASYNC_WORKERS: usize = 8;

/// Ceiling on the daemon runtime's `spawn_blocking` pool.
const DAEMON_MAX_BLOCKING_THREADS: usize = 16;

/// How long the displaced-artifacts cleanup waits after the pass that raised it, so a burst of
/// migrating passes shares one sweep.
const CLEANUP_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Delay before the first blob GC after daemon start.
const STARTUP_GC_DELAY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// How often the Unix socket-ownership watchdog verifies we still own our bound socket. Short, so
/// an orphaned daemon (its socket reclaimed by another) self-terminates within seconds.
#[cfg(unix)]
const OWNERSHIP_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Test hook: delay the agent store open by this many milliseconds, so tests can prove the daemon
/// answers `Ping` while the store is still opening. Compiled only under the dev-only `test-support`
/// feature, so a release build neither reads the variable nor carries the branch.
#[cfg(feature = "test-support")]
const TEST_STORE_OPEN_DELAY_ENV: &str = "BASEMIND_TEST_COMMS_STORE_OPEN_DELAY_MS";

#[cfg(feature = "test-support")]
fn store_open_delay() -> std::time::Duration {
    std::env::var(TEST_STORE_OPEN_DELAY_ENV)
        .ok()
        .and_then(|millis| millis.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or_default()
}

#[cfg(not(feature = "test-support"))]
fn store_open_delay() -> std::time::Duration {
    std::time::Duration::ZERO
}

/// How often a waiting daemon tries the lock again.
const HOLDER_DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(250);

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

    // Bound the third-party thread pools the daemon inherits before any of them is first used. Each
    // defaults to one thread per core and they are created lazily, so an unset env means an 18-core
    // machine ends up with dozens of mostly idle threads per pool. An operator's own setting wins.
    for (key, value) in DAEMON_POOL_ENV {
        if std::env::var_os(key).is_none() {
            // SAFETY: runs first in `run()`, before the runtime or any other thread exists.
            unsafe { std::env::set_var(key, value) };
        }
    }

    #[cfg(feature = "intelligence")]
    crate::embeddings::bound_ort_memory(crate::embeddings::ort_intra_threads(
        crate::embeddings::EmbedProcess::Daemon,
        0,
        0,
    ));

    crate::comms::fd_budget::raise_to_budget();

    // The daemon is mostly idle I/O relay, so a handful of async workers is plenty; the blocking pool
    // (GC, read-stack builds, git-history syncs) is capped well under tokio's default of 512 and its
    // idle threads are reaped quickly.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(DAEMON_ASYNC_WORKERS)
        .max_blocking_threads(DAEMON_MAX_BLOCKING_THREADS)
        .thread_keep_alive(std::time::Duration::from_secs(5))
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    // Single-owner lock FIRST, before bind: a redundant daemon converges (exit 0) without ever
    // touching the live daemon's socket. This — plus the machine-registry entry it writes — is the
    // authoritative "one daemon per comms dir" gate, uniform across Unix and Windows. It is held
    // until after the runtime's teardown below, so it is released last: a successor that took it
    // while this daemon still held its stores found the comms store locked and exited.
    use crate::comms::daemon_lock::{DaemonKind, DaemonLock, DaemonLockOutcome};
    let socket_path = paths.socket_path.clone();
    let daemon_lock = match DaemonLock::acquire_waiting(
        DaemonKind::Comms,
        &paths.comms_dir,
        env!("CARGO_PKG_VERSION"),
        &crate::daemon_lock::daemons_dir(),
        (HOLDER_DRAIN_WAIT, HOLDER_DRAIN_POLL),
        || singleton::probe_alive(&socket_path),
    ) {
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

    let result = runtime.block_on(async move {
        // Held until this block returns: a daemon that is killed leaves the marker for the next.
        let _hosting = crate::scan_evidence::HostingMarker::begin();

        let listener = match singleton::bind_listener(&paths.socket_path, singleton::probe_alive) {
            Ok(listener) => listener,
            Err(singleton::SingletonError::AlreadyRunning(p)) => {
                tracing::info!(socket = %p.display(), "comms daemon already running; exiting");
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("bind comms socket: {e}")),
        };

        // Answer `Ping` (and park everything else) while the broker is built: opening the machine
        // registry can stall on a contended disk, and a liveness probe must not read that as a dead
        // daemon. The agent store opens on a thread of its own and parks only agent requests. Unix
        // only; the Windows pipe front-end starts once the broker exists.
        #[cfg(unix)]
        let early = crate::comms::frontend_uds::EarlyAccept::start(listener, paths.socket_path.clone());

        // `build_id` hashes the whole executable on first use. Do it here, off the request path and
        // alongside the store open, so the first `Status` never stalls an async worker on it.
        tokio::task::spawn_blocking(|| {
            crate::version::build_id();
        });

        let agents = AgentHub::open_after(&paths.comms_dir, store_open_delay());
        log_retention_policy();
        let machine_registry = match crate::registry::Registry::from_data_home(&crate::data_root::DataRoot::current()) {
            Ok(registry) => registry,
            Err(error) => {
                tracing::warn!(%error, "comms: machine registry open failed; coordination tools degrade to empty");
                crate::registry::Registry::open(&paths.comms_dir.join("registry-fallback"))
                    .context("open fallback machine registry")?
            }
        };
        let broker = Arc::new(Broker::with_registry(agents, machine_registry));
        // The first sweep runs as soon as the store has opened, not on the first maintenance tick.
        let broker_for_first_sweep = broker.clone();
        tokio::spawn(async move {
            broker_for_first_sweep.agents_opened().await;
            broker_for_first_sweep.maintain_agents().await;
        });

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
            if !broker_for_bootstrap.ever_served() && !broker_for_bootstrap.serves_http() {
                tracing::info!(
                    bootstrap_secs = bootstrap.as_secs(),
                    "comms: no client connected within the bootstrap window; self-terminating"
                );
                broker_for_bootstrap.begin_drain().await;
            }
        });

        // The detached daemon's stdout/stderr land in `daemon.log` (see `singleton_spawn`); keep it
        // bounded for a daemon that stays up for weeks.
        let comms_dir_for_log = paths.comms_dir.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tick.tick().await;
                crate::comms::singleton::trim_daemon_log(&comms_dir_for_log);
            }
        });

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
                let evicted = broker_for_prune.evict_idle_workspaces(WORKSPACE_HOT_TTL);
                if evicted > 0 {
                    tracing::info!(evicted, "daemon: shed idle hot workspaces from RAM");
                }
                prune_missing_registry_rows(&broker_for_prune).await;
            }
        });

        let broker_for_agents = broker.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(AGENT_SWEEP_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                broker_for_agents.maintain_agents().await;
            }
        });

        // The read-stack sweep runs on its own task, not folded into the prune loop above: the two ~keep
        // bound different things on clocks an order apart, and the prune loop's hourly tick would ~keep
        // make a five-minute read-stack TTL unobservable. ~keep
        let broker_for_stacks = broker.clone();
        tokio::spawn(async move {
            use crate::comms::workspace_pool::READ_STACK_IDLE_TTL;
            let mut tick = tokio::time::interval(READ_STACK_SWEEP_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                let dropped = broker_for_stacks.evict_idle_read_stacks(READ_STACK_IDLE_TTL);
                if dropped > 0 {
                    tracing::info!(dropped, "daemon: shed idle hosted read stacks from RAM");
                }
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
            // The first sweep waits out STARTUP_GC_DELAY: it deserializes every workspace's full index,
            // and running it in the first seconds stacks that peak on top of the reconnect storm's
            // read-stack builds. A daemon that lives this long still reclaims.
            tokio::time::sleep(STARTUP_GC_DELAY).await;
            run_gc_cycle(&broker_for_gc).await;
            let mut tick = tokio::time::interval(GC_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                run_gc_cycle(&broker_for_gc).await;
            }
        });

        // Prompt cleanup after a pass that moved paths across tiers or re-extracted pre-epoch blobs:
        // the orphans it left are known to exist, so waiting for the hourly tick (or the 10 minute
        // startup delay) would only keep them on disk. Debounced so a burst of passes (many
        // workspaces rescanning on the first start of a new version) shares one sweep; the sweep
        // itself is the same lock-bounded, grace-protected one the maintenance tick runs.
        let broker_for_cleanup = broker.clone();
        let cleanup_signal = broker.cleanup_signal();
        tokio::spawn(async move {
            loop {
                cleanup_signal.notified().await;
                tokio::time::sleep(CLEANUP_DEBOUNCE).await;
                run_gc_cycle(&broker_for_cleanup).await;
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
            early
                .finish(broker.clone())
                .await
                .context("hand the socket to the front-end")?,
        ));
        #[cfg(windows)]
        let frontend: Box<dyn CommsFrontendObj> = Box::new(NamedPipeFrontendBox(
            crate::comms::frontend_named_pipe::NamedPipeFrontend::from_first_instance(
                listener,
                paths.socket_path.clone().into_os_string(),
            ),
        ));
        let broker_for_exit = broker.clone();
        let served = frontend
            .serve_obj(broker, shutdown_rx)
            .await
            .context("comms front-end serve loop");
        let tail = async {
            broker_for_exit.shed_hosted_stacks().await;
            if served.is_ok() {
                broker_for_exit.maintain_agents().await;
            }
        };
        if tokio::time::timeout(EXIT_TAIL_TIMEOUT, tail).await.is_err() {
            tracing::warn!(
                timeout_secs = EXIT_TAIL_TIMEOUT.as_secs(),
                "comms: the exit tail overran; the runtime teardown bounds the rest"
            );
        }
        served?;
        Ok(())
    });
    // Bounded teardown instead of the implicit unbounded drop — see [`RUNTIME_SHUTDOWN_TIMEOUT`]. ~keep
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    drop(daemon_lock);
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

fn log_retention_policy() {
    use crate::comms::v2::api::{
        MAX_ACTIVE_THREADS_PER_REPO, MAX_MESSAGES_PER_THREAD, MESSAGE_TTL, REPLAY_TTL, SEEN_TTL,
    };
    tracing::info!(
        message_ttl_secs = MESSAGE_TTL.as_secs(),
        max_messages_per_thread = MAX_MESSAGES_PER_THREAD,
        max_active_threads_per_repo = MAX_ACTIVE_THREADS_PER_REPO,
        seen_ttl_secs = SEEN_TTL.as_secs(),
        replay_ttl_secs = REPLAY_TTL.as_secs(),
        "comms: agent store retention"
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
