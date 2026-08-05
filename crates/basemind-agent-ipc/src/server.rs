//! The daemon half: bridge one accepted connection to an in-process engine.
//!
//! [`serve_connection`] is generic over any `impl AgentClient`. In practice the daemon builds an
//! in-process channel ([`in_proc_channel`](basemind_agent::in_proc_channel)), spawns the
//! [`Session`](basemind_agent::Session) on the [`EngineEndpoint`](basemind_agent::EngineEndpoint)
//! half, and hands the [`InProcAgentClient`](basemind_agent::InProcAgentClient) half here — so this
//! bridge is the socket-facing mirror of a UI: commands decoded from the socket go *into* the engine,
//! events from the engine go *out* to the socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use basemind_agent::{AgentClient, AgentCommand};
use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::error::IpcError;
use crate::frame::{codec, decode, encode};

/// Grace period for an auto-spawned daemon that no client ever reaches.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
/// Idle window after a previously used daemon has no live attaches.
const IDLE_REAP_AFTER: Duration = Duration::from_secs(10 * 60);
/// Cadence for checking bootstrap and idle lifecycle conditions.
const IDLE_REAP_CHECK_EVERY: Duration = Duration::from_secs(60);
/// Maximum time graceful shutdown waits for existing connections to detach.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

const BOOTSTRAP_TIMEOUT_ENV: &str = "BASEMIND_AGENT_BOOTSTRAP_SECS";
const IDLE_REAP_AFTER_ENV: &str = "BASEMIND_AGENT_IDLE_REAP_SECS";
const IDLE_REAP_CHECK_EVERY_ENV: &str = "BASEMIND_AGENT_IDLE_CHECK_SECS";

#[derive(Clone, Copy, Debug)]
struct ServeConfig {
    bootstrap_timeout: Duration,
    idle_reap_after: Duration,
    idle_check_every: Duration,
    drain_grace: Duration,
}

impl ServeConfig {
    fn from_env() -> Self {
        Self {
            bootstrap_timeout: duration_from_env(BOOTSTRAP_TIMEOUT_ENV, BOOTSTRAP_TIMEOUT),
            idle_reap_after: duration_from_env(IDLE_REAP_AFTER_ENV, IDLE_REAP_AFTER),
            idle_check_every: duration_from_env(IDLE_REAP_CHECK_EVERY_ENV, IDLE_REAP_CHECK_EVERY),
            drain_grace: DRAIN_GRACE,
        }
    }
}

fn duration_from_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

struct ConnectionState {
    started: Instant,
    ever_connected: AtomicBool,
    live_connections: AtomicUsize,
    last_activity_ms: AtomicU64,
    zero_connections: Notify,
}

impl ConnectionState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            ever_connected: AtomicBool::new(false),
            live_connections: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
            zero_connections: Notify::new(),
        })
    }

    fn register(self: &Arc<Self>) -> ConnectionGuard {
        self.live_connections.fetch_add(1, Ordering::SeqCst);
        self.touch();
        ConnectionGuard {
            state: Arc::clone(self),
        }
    }

    fn mark_served(&self) {
        self.ever_connected.store(true, Ordering::SeqCst);
        self.touch();
    }

    fn should_reap(&self, config: ServeConfig) -> bool {
        if !self.ever_connected.load(Ordering::SeqCst) {
            return self.started.elapsed() >= config.bootstrap_timeout;
        }
        if self.live_connections.load(Ordering::SeqCst) != 0 {
            return false;
        }
        self.elapsed_since_activity() >= config.idle_reap_after
    }

    async fn wait_for_zero(&self) {
        loop {
            let notified = self.zero_connections.notified();
            if self.live_connections.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn touch(&self) {
        self.last_activity_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::SeqCst);
    }

    fn elapsed_since_activity(&self) -> Duration {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let last_ms = self.last_activity_ms.load(Ordering::SeqCst);
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }
}

struct ConnectionGuard {
    state: Arc<ConnectionState>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let previous = self.state.live_connections.fetch_sub(1, Ordering::SeqCst);
        self.state.touch();
        if previous == 1 {
            self.state.zero_connections.notify_waiters();
        }
    }
}

/// Run the daemon accept loop: for every accepted connection, mint a fresh engine-facing client via
/// `make_client` and bridge it to the socket with [`serve_connection`] on its own task.
///
/// `make_client` is called once per connection — the daemon passes
/// `|| template.new_client()`([`InProcAgentClient::new_client`](basemind_agent::InProcAgentClient::new_client)),
/// so every attach shares one long-lived engine and the session outlives any single connection.
/// Runs until `shutdown` changes, the bootstrap grace expires without a connection, or a previously
/// used daemon remains at zero connections for its idle window. Transient accept and connection
/// errors are logged without tearing down the shared session.
pub async fn serve<C, F>(
    listener: UnixListener,
    make_client: F,
    shutdown: watch::Receiver<bool>,
) -> Result<(), IpcError>
where
    C: AgentClient,
    F: FnMut() -> C,
{
    serve_with_config(listener, make_client, shutdown, ServeConfig::from_env()).await
}

async fn serve_with_config<C, F>(
    listener: UnixListener,
    mut make_client: F,
    mut shutdown: watch::Receiver<bool>,
    config: ServeConfig,
) -> Result<(), IpcError>
where
    C: AgentClient,
    F: FnMut() -> C,
{
    let state = ConnectionState::new();
    let mut connections = JoinSet::new();
    let mut reap_tick = tokio::time::interval(config.idle_check_every);
    reap_tick.tick().await;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = reap_tick.tick() => {
                if state.should_reap(config) {
                    tracing::info!("agent ipc: lifecycle grace elapsed; self-terminating");
                    break;
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let guard = state.register();
                    let connection_state = Arc::clone(&state);
                    let client = make_client();
                    connections.spawn(async move {
                        let _guard = guard;
                        if let Err(error) = serve_connection_inner(stream, client, Some(&connection_state)).await {
                            tracing::warn!(%error, "agent ipc: connection ended with an error");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "agent ipc: accept failed; continuing");
                }
            },
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined {
                    tracing::warn!(%error, "agent ipc: connection task failed");
                }
            }
        }
    }

    drop(listener);
    drain_connections(&mut connections, &state, config.drain_grace).await;
    Ok(())
}

async fn drain_connections(connections: &mut JoinSet<()>, state: &ConnectionState, grace: Duration) {
    if tokio::time::timeout(grace, state.wait_for_zero()).await.is_err() {
        tracing::warn!(
            grace_secs = grace.as_secs(),
            "agent ipc: drain grace elapsed; aborting connections"
        );
        connections.abort_all();
    }
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "agent ipc: connection task failed while draining");
        }
    }
}

/// Bridge a single connected `stream` to `client` (an engine-facing [`AgentClient`]): forward the
/// engine's events out as msgpack frames, and feed decoded inbound command frames into the engine.
/// Returns when either side closes (the engine shuts down, or the peer disconnects).
///
/// The two directions share `client` in one `select!`: when the inbound branch fires, the pending
/// `next_event()` future is dropped before `send_command` runs, so the `&mut self`/`&self` borrows
/// never overlap — the same cancellation shape the in-process UI loop relies on. Dropping a pending
/// `next_event()` does not lose an event (the engine's broadcast keeps it queued).
pub async fn serve_connection<C: AgentClient>(stream: UnixStream, client: C) -> Result<(), IpcError> {
    serve_connection_inner(stream, client, None).await
}

async fn serve_connection_inner<C: AgentClient>(
    stream: UnixStream,
    mut client: C,
    connection_state: Option<&ConnectionState>,
) -> Result<(), IpcError> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, codec());
    let mut writer = FramedWrite::new(write_half, codec());

    loop {
        tokio::select! {
            event = client.next_event() => match event {
                Some(event) => writer.send(encode(&event)?).await?,
                // The engine shut down; close the socket. ~keep
                None => break,
            },
            frame = reader.next() => match frame {
                Some(Ok(frame)) => {
                    let command: AgentCommand = decode(&frame)?;
                    if let Some(state) = connection_state {
                        state.mark_served();
                    }
                    // A `Shutdown` from a front-end means "this UI is detaching", not "kill the
                    // shared engine": the daemon session must outlive any single connection, so close
                    // this connection without forwarding it. (In-process mode never reaches here — it
                    // drives the engine directly, where `Shutdown` correctly ends the session.) ~keep
                    if matches!(command, AgentCommand::Shutdown) {
                        break;
                    }
                    // The engine only errors here if it is already gone, in which case the next
                    // `next_event()` returns `None` and ends the loop; nothing to do on error. ~keep
                    let _ = client.send_command(command).await;
                }
                Some(Err(error)) => return Err(error.into()),
                // The peer disconnected; drop this connection's client. In the daemon case the shared
                // session lives on (the template still holds the command sink); in the one-shot case
                // this was the sole sink, so dropping it ends the engine. ~keep
                None => break,
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use basemind_agent::in_proc_channel;
    use tokio::sync::watch;

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const SHORT_WINDOW: Duration = Duration::from_millis(40);
    const CHECK_INTERVAL: Duration = Duration::from_millis(5);

    fn test_config() -> ServeConfig {
        ServeConfig {
            bootstrap_timeout: SHORT_WINDOW,
            idle_reap_after: SHORT_WINDOW,
            idle_check_every: CHECK_INTERVAL,
            drain_grace: SHORT_WINDOW,
        }
    }

    #[tokio::test]
    async fn serve_returns_when_shutdown_watch_changes() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let listener = UnixListener::bind(dir.path().join("agent.sock")).expect("bind listener");
        let (_endpoint, template) = in_proc_channel(1, 1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let task = tokio::spawn(serve_with_config(
            listener,
            move || template.new_client(),
            shutdown_rx,
            test_config(),
        ));
        shutdown_tx.send(true).expect("request shutdown");

        tokio::time::timeout(TEST_TIMEOUT, task)
            .await
            .expect("serve exits after shutdown")
            .expect("serve task joins")
            .expect("serve returns successfully");
    }

    #[tokio::test]
    async fn daemon_does_not_reap_while_served_connection_is_live() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let socket = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket).expect("bind listener");
        let (_endpoint, template) = in_proc_channel(1, 1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let mut task = tokio::spawn(serve_with_config(
            listener,
            move || template.new_client(),
            shutdown_rx,
            test_config(),
        ));
        let client = crate::client::UdsAgentClient::connect(&socket)
            .await
            .expect("connect client");
        client
            .send_command(AgentCommand::Cancel)
            .await
            .expect("mark a real attach");
        tokio::time::sleep(SHORT_WINDOW * 2).await;

        assert!(!task.is_finished(), "a live connection pins the daemon");
        drop(client);
        tokio::time::timeout(TEST_TIMEOUT, &mut task)
            .await
            .expect("serve exits after the post-disconnect idle window")
            .expect("serve task joins")
            .expect("serve returns successfully");
    }

    #[tokio::test]
    async fn commandless_connection_does_not_pin_past_bootstrap_grace() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let socket = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket).expect("bind listener");
        let (_endpoint, template) = in_proc_channel(1, 1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = ServeConfig {
            drain_grace: CHECK_INTERVAL,
            ..test_config()
        };

        let task = tokio::spawn(serve_with_config(
            listener,
            move || template.new_client(),
            shutdown_rx,
            config,
        ));
        let _stuck_probe = UnixStream::connect(&socket).await.expect("connect stuck probe");

        tokio::time::timeout(TEST_TIMEOUT, task)
            .await
            .expect("commandless connection cannot defeat bootstrap reap")
            .expect("serve task joins")
            .expect("serve returns successfully");
    }

    #[tokio::test]
    async fn daemon_waits_for_idle_window_after_disconnect() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let socket = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket).expect("bind listener");
        let (_endpoint, template) = in_proc_channel(1, 1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let config = ServeConfig {
            idle_reap_after: SHORT_WINDOW * 3,
            ..test_config()
        };
        let mut task = tokio::spawn(serve_with_config(
            listener,
            move || template.new_client(),
            shutdown_rx,
            config,
        ));
        let client = crate::client::UdsAgentClient::connect(&socket)
            .await
            .expect("connect client");
        client
            .send_command(AgentCommand::Shutdown)
            .await
            .expect("mark a real attach");
        drop(client);
        tokio::time::sleep(SHORT_WINDOW * 2).await;

        assert!(
            !task.is_finished(),
            "a used daemon waits for its idle window, not bootstrap grace"
        );
        tokio::time::timeout(TEST_TIMEOUT, &mut task)
            .await
            .expect("serve exits after the idle window")
            .expect("serve task joins")
            .expect("serve returns successfully");
    }

    #[tokio::test]
    async fn daemon_reaps_after_bootstrap_when_never_connected() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let listener = UnixListener::bind(dir.path().join("agent.sock")).expect("bind listener");
        let (_endpoint, template) = in_proc_channel(1, 1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let task = tokio::spawn(serve_with_config(
            listener,
            move || template.new_client(),
            shutdown_rx,
            test_config(),
        ));

        tokio::time::timeout(TEST_TIMEOUT, task)
            .await
            .expect("unused daemon exits after bootstrap grace")
            .expect("serve task joins")
            .expect("serve returns successfully");
    }

    #[tokio::test]
    async fn empty_probe_connection_does_not_disable_bootstrap_reap() {
        let dir = tempfile::tempdir().expect("socket tempdir");
        let socket = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&socket).expect("bind listener");
        let (_endpoint, template) = in_proc_channel(1, 1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = ServeConfig {
            idle_reap_after: TEST_TIMEOUT * 2,
            ..test_config()
        };

        let task = tokio::spawn(serve_with_config(
            listener,
            move || template.new_client(),
            shutdown_rx,
            config,
        ));
        let probe = UnixStream::connect(&socket).await.expect("connect liveness probe");
        drop(probe);

        tokio::time::timeout(TEST_TIMEOUT, task)
            .await
            .expect("empty probe still permits bootstrap reap")
            .expect("serve task joins")
            .expect("serve returns successfully");
    }
}
