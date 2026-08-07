//! The broker: the single owner of all comms state.
//!
//! [`Broker`] wraps the [`CommsStore`] and an in-RAM registry of live notification sinks. It
//! handles each [`CommsRequest`] and fans out [`CommsNotification::Message`] to every link
//! subscribed to the posted thread. The daemon is the sole writer to the store, so request
//! handling needs no cross-process coordination beyond the store's flock.
//!
//! There is NO auto-join: `Hello` records identity and captures the scope chain for path-glob
//! discovery only. Agents explicitly START a thread or JOIN one.
//!
//! ## Lifecycle
//!
//! `Starting → Active ⇄ Idle → Draining → Stopped`. The subscriber refcount drives the
//! Active⇄Idle edge; `Draining` stops accepting, flushes, then releases the flock and unlinks the
//! socket on the way to `Stopped`.
//!
//! Four things enter `Draining`: a `Stop` RPC, SIGTERM, the socket-ownership watchdog (another
//! daemon reclaimed our socket), and the **idle reaper** — the daemon is machine-wide and
//! auto-spawned on demand, so a daemon nobody is using exits after [`IDLE_REAP_AFTER`] rather than
//! lingering, and the next client that needs one respawns it. [`Broker::is_idle_for`] defines
//! exactly what "nobody is using" means and why silence on the socket is not part of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::watch;

use super::git_history_ops::HistoryEntry;
use super::ids::{AgentId, ThreadId};
use super::protocol::{CommsNotification, CommsOut, CommsRequest, CommsResponse};
use super::scope::ScopeChain;
use super::store::{CommsStore, CommsStoreError};
use super::workspace_pool::{self, WorkspacePool};
use crate::registry::Registry as MachineRegistry;

/// Default page size when a client omits `limit`.
pub const DEFAULT_LIMIT: u32 = 100;
/// Hard cap on a page, mirroring the MCP `limit` ceiling.
pub const MAX_LIMIT: u32 = 1000;

/// Idle window after which a daemon with no connected links and no work in flight self-terminates.
///
/// Note what "idle" costs here: nothing holds a link between requests — every caller (`serve`, the
/// CLI verbs, the MCP comms helpers) connects, does its RPC, and drops the client — so
/// `link_count == 0` is the steady state even DURING an active session, and this window is really
/// "N minutes since anyone last called us". Reaping early is cheap (a respawn re-OPENS the on-disk
/// fjall indexes rather than rebuilding them), so this is deliberately short: ten minutes keeps an
/// interactive session's daemon warm across normal pauses while shrinking the standing population a
/// machine running the suite carries. The complementary [`BOOTSTRAP_TIMEOUT`] handles the other leak
/// shape — a daemon nobody ever connected to (a spawn-and-abandon from a test or script).
pub const IDLE_REAP_AFTER: Duration = Duration::from_secs(10 * 60);
/// How often the idle reaper re-checks the broker. Small relative to [`IDLE_REAP_AFTER`], so the
/// worst-case overshoot past the window is one tick.
pub const IDLE_REAP_CHECK_EVERY: Duration = Duration::from_secs(60);

/// Grace window for a daemon that NO client has ever connected to. A daemon is auto-spawned on
/// demand, so one that never sees a single link within this window was spawned and abandoned (a test
/// or script that ran `daemon ensure` then exited, or a spawner that died) — it self-terminates
/// rather than lingering the full [`IDLE_REAP_AFTER`]. Distinct from idle: idle needs a prior
/// connection; this fires precisely when there has never been one. See [`Broker::ever_served`].
pub const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
/// Env override for [`BOOTSTRAP_TIMEOUT`], in whole seconds. Lets tests drive the bootstrap reap
/// without waiting two minutes, and is a field escape hatch. See [`IDLE_REAP_AFTER_ENV`].
pub const BOOTSTRAP_TIMEOUT_ENV: &str = "BASEMIND_COMMS_BOOTSTRAP_SECS";

/// Env var overriding [`IDLE_REAP_AFTER`], in whole seconds. Exists so tests can exercise the reap
/// without sleeping for half an hour; also a field escape hatch for a machine that wants daemons to
/// linger (or vanish) more aggressively.
pub const IDLE_REAP_AFTER_ENV: &str = "BASEMIND_COMMS_IDLE_REAP_SECS";
/// Env var overriding [`IDLE_REAP_CHECK_EVERY`], in whole seconds. See [`IDLE_REAP_AFTER_ENV`].
pub const IDLE_REAP_CHECK_EVERY_ENV: &str = "BASEMIND_COMMS_IDLE_CHECK_SECS";

/// Read a whole-seconds [`Duration`] from `var`, falling back to `default` when it is unset, empty,
/// unparseable, or zero. Zero is rejected on purpose: a zero check interval would spin the reaper.
fn duration_from_env(var: &str, default: Duration) -> Duration {
    match std::env::var(var) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            _ => default,
        },
        Err(_) => default,
    }
}

/// The effective idle window: [`IDLE_REAP_AFTER`] unless [`IDLE_REAP_AFTER_ENV`] overrides it.
pub fn idle_reap_after() -> Duration {
    duration_from_env(IDLE_REAP_AFTER_ENV, IDLE_REAP_AFTER)
}

/// The effective reaper cadence: [`IDLE_REAP_CHECK_EVERY`] unless [`IDLE_REAP_CHECK_EVERY_ENV`]
/// overrides it.
pub fn idle_reap_check_every() -> Duration {
    duration_from_env(IDLE_REAP_CHECK_EVERY_ENV, IDLE_REAP_CHECK_EVERY)
}

/// The effective bootstrap grace: [`BOOTSTRAP_TIMEOUT`] unless [`BOOTSTRAP_TIMEOUT_ENV`] overrides it.
pub fn bootstrap_timeout() -> Duration {
    duration_from_env(BOOTSTRAP_TIMEOUT_ENV, BOOTSTRAP_TIMEOUT)
}

/// How long a drain waits for links accepted before it started to finish their in-flight request
/// before exiting anyway. Bounded so one wedged client cannot pin a draining daemon forever; ample
/// for any request that is actually progressing, and the idle path normally finds zero links.
pub const DRAIN_GRACE: Duration = Duration::from_secs(10);
/// Poll cadence while waiting out [`DRAIN_GRACE`].
const DRAIN_POLL_EVERY: Duration = Duration::from_millis(25);
/// How long a GC cycle waits for the blob-GC write lock before declaring itself starved and
/// skipping the cycle. Every rescan holds the read side for its whole duration, so this must be
/// long enough to outlast a legitimate big-monorepo scan yet bounded — an unbounded wait behind
/// a runaway rescan is how the maintenance loop silently parked forever (116 GB incident).
const GC_LOCK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The RAII refcount guards ([`LinkGuard`], [`WorkGuard`]) live in `daemon_guards.rs` to keep this
/// file under the module-size cap. Re-exported here so their historical path (`daemon::LinkGuard`)
/// stays stable for the front-ends that import them.
pub use super::daemon_guards::{LinkGuard, RelayGuard, WorkGuard};

/// How long an ACTIVE thread may sit idle before the system auto-archives it. Conservative — a
/// thread past two weeks of silence is almost certainly done. The daemon's periodic sweep
/// (`archive_idle`) applies this; the creator or a human can archive sooner.
pub const THREAD_IDLE_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How long an ARCHIVED thread's storage is retained before the daemon permanently reclaims it
/// (row + messages + members + cursors). The retention tail after [`THREAD_IDLE_TTL`]: a thread
/// first drops out of active listings, then, once archived and untouched for this far-longer
/// window, its storage is freed. Conservative so a thread stays recoverable well past archival.
pub const THREAD_RETENTION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a hot workspace may sit unrequested before the daemon sheds it from RAM. Its on-disk
/// cache survives; the next request re-opens it lazily.
///
/// Independent of [`IDLE_REAP_AFTER`], and no longer ordered against it: this bounds the memory of a
/// LIVE, BUSY daemon (one serving workspace A while workspace B goes cold), whereas the reap window
/// disposes of a daemon nobody is using at all. A daemon that is merely idle now exits before this
/// TTL would ever fire — which is strictly better, since exiting releases the same handles plus the
/// process.
pub const WORKSPACE_HOT_TTL: Duration = Duration::from_secs(15 * 60);

/// Lifecycle state of the broker. See the module docs for the transition rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    /// Booting: store opening, front-ends not yet accepting.
    Starting,
    /// Serving with at least one live subscriber.
    Active,
    /// No live subscribers; socket + flock retained, caches may be shed.
    Idle,
    /// Stop requested: refusing new work, flushing.
    Draining,
    /// Fully stopped; flock released, socket unlinked.
    Stopped,
}

/// What a [`SubSink`] wakes on. `Thread` is the original thread-scoped stream opened by
/// [`CommsRequest::Subscribe`] (which also joins the thread); `Inbox` is the passive,
/// membership-routed stream opened by [`CommsRequest::SubscribeInbox`] that backs `inbox_wait`.
pub(super) enum SubScope {
    /// Wake on every post to this one thread (the classic `Subscribe` behavior).
    Thread(ThreadId),
    /// Wake on a post to any thread the sink's agent is a member of, or — when `Some` — to just
    /// that one thread. Self-authored posts never wake this scope (mirrors `on_inbox`).
    Inbox {
        /// Restrict the wake to this thread only. `None` = any joined thread.
        thread: Option<ThreadId>,
    },
}

/// A registered notification sink for one subscription. The link's writer half drains it.
pub(super) struct SubSink {
    /// What this sink wakes on.
    pub(super) scope: SubScope,
    /// The agent owning the subscription. `Thread` sinks retain it only for diagnostics; `Inbox`
    /// sinks use it to route by membership and to exclude the agent's own posts.
    pub(super) agent: AgentId,
    /// Where notifications are pushed.
    pub(super) tx: mpsc::Sender<CommsOut>,
}

/// In-RAM broker state behind a single async mutex.
pub(super) struct Registry {
    /// Live notification sinks keyed by subscription handle.
    pub(super) sinks: AHashMap<u64, SubSink>,
    /// Current lifecycle state.
    pub(super) state: LifecycleState,
}

/// The broker. Cheap to share via `Arc`; every front-end and link holds one.
pub struct Broker {
    pub(super) store: Arc<CommsStore>,
    /// Hot read-write workspace indexes. The daemon is the machine's sole fjall writer; front-ends
    /// forward their scans/rescans here so concurrent read-only sessions never contend for the lock.
    pub(super) workspaces: Arc<WorkspacePool>,
    /// Open git-history indexes, keyed by the repo's SHARED history cache dir (every linked worktree
    /// of a clone maps to one entry — the commit graph is identical). Same rationale as
    /// [`workspaces`](Self::workspaces): fjall's directory lock is exclusive, so the daemon holds
    /// these and front-ends forward their history ops — the BUILD and the reads — over the socket.
    /// See [`git_history_ops`](super::git_history_ops).
    pub(super) git_history: std::sync::Mutex<AHashMap<std::path::PathBuf, Arc<HistoryEntry>>>,
    /// Serializes the COLD open of a git-history index (fjall's directory lock is exclusive, so two
    /// racing opens of the same database leave the loser failing on the lock).
    pub(super) git_history_open_lock: Mutex<()>,
    pub(super) registry: Mutex<Registry>,
    /// The machine-wide repo/worktree/branch/workspace registry (distinct from the `registry` sink
    /// map above). The daemon is its sole writer; coordination tools read/mutate through it.
    machine_registry: Mutex<MachineRegistry>,
    /// Serializes destructive global blob GC against in-flight rescans. Rescans take the READ side
    /// (many workspaces rescan concurrently); the GC sweep takes the WRITE side. A rescan writes new
    /// content-addressed blobs BEFORE its `index.msgpack` (which `collect_referenced_hashes` reads)
    /// is rewritten, so a GC that reference-counts mid-rescan would see those fresh blobs as orphans
    /// and reap them — a first-ever scan (no prior index) could lose ALL its blobs. This lock keeps
    /// the two mutually exclusive without blocking concurrent rescans of different workspaces.
    blob_gc_lock: RwLock<()>,
    /// The accept-loop shutdown signal, installed by the daemon entry point after it builds the
    /// `watch` channel. `begin_drain` fires it so a `Stop` RPC (or any drain) actually breaks the
    /// front-end accept loop — not just notifies connected sinks. Absent in tests that drive the
    /// broker directly, where `begin_drain` still transitions state and notifies sinks.
    shutdown: std::sync::OnceLock<watch::Sender<bool>>,
    /// Cooperative cancellation for in-flight scans. Every drain route ends in `finish_drain`,
    /// which trips this FIRST — so a `Stop` RPC, SIGTERM, the idle reaper, and socket-ownership
    /// loss all interrupt a mid-scan `spawn_blocking` at per-file granularity instead of letting
    /// the runtime teardown block on it (previously only SIGKILL could end a scanning daemon).
    scan_cancel: crate::scanner::ScanCancel,
    pub(super) subscriber_count: AtomicUsize,
    link_count: AtomicUsize,
    pub(super) relay_count: AtomicUsize,
    /// Latches true the first time any link connects. Drives the bootstrap reaper: a daemon that
    /// stays `false` past [`BOOTSTRAP_TIMEOUT`] was spawned and abandoned (nobody ever dialed it) and
    /// self-terminates. Never reset — once a session has used the daemon, the idle window governs.
    ever_linked: AtomicBool,
    /// Daemon-internal work units in flight (see [`Broker::begin_work`]). Distinct from
    /// `link_count`: this covers work NO client is attached to — the blob GC above all — which the
    /// idle reaper would otherwise be free to tear down mid-sweep.
    pub(super) work_inflight: AtomicUsize,
    last_activity_ms: AtomicU64,
    /// In-flight streamable-HTTP requests (see [`Broker::begin_http_request`]). The HTTP front-end
    /// is stateless — nothing holds a link between requests — so `link_count` cannot represent an
    /// active HTTP client. This counter does: a request in flight pins the daemon exactly as a
    /// connected UDS link would, so the idle reaper cannot tear the process down mid-request.
    pub(super) http_inflight: AtomicUsize,
    /// Milliseconds (since [`started`](Self::started)) of the last streamable-HTTP request, stamped
    /// on both the start and the end of every request. Feeds the idle predicate the same way
    /// `last_activity_ms` does for UDS links: a request within the idle window keeps the daemon
    /// alive even though no persistent connection is held between requests. Initial `0` reads as
    /// "epoch" — identical to `last_activity_ms`, so an HTTP-idle daemon reaps on the normal window.
    pub(super) last_http_ms: AtomicU64,
    pub(super) next_sub: AtomicU64,
    pub(super) started: Instant,
    pub(super) version: String,
}

impl Broker {
    /// Construct a broker over an already-opened store, opening the machine registry from the
    /// machine-global cache. A registry-open failure degrades to an empty in-memory registry (rooted
    /// at a throwaway path) rather than failing the daemon — coordination tools then return empty
    /// until a workspace registers. Use [`Broker::with_registry`] to inject a registry (tests).
    pub fn new(store: Arc<CommsStore>) -> Self {
        let registry = MachineRegistry::from_data_home().unwrap_or_else(|error| {
            tracing::warn!(%error, "comms: machine registry open failed; using an empty in-memory registry");
            MachineRegistry::open(
                &std::env::temp_dir().join(format!("basemind-registry-fallback-{}", std::process::id())),
            )
            .expect("open fallback registry in temp dir")
        });
        Self::with_registry(store, registry)
    }

    /// Construct a broker over an already-opened store and an explicit machine registry. The daemon
    /// owns the registry as its sole writer; the coordination tools read/mutate through it. Tests
    /// inject an isolated registry here.
    pub fn with_registry(store: Arc<CommsStore>, machine_registry: MachineRegistry) -> Self {
        let workspaces = Arc::new(WorkspacePool::new(workspace_pool::DEFAULT_HOT_CAP));
        // Adopt the pool's drain token so `begin_drain`'s cancel reaches BOTH dispatch paths: the
        // socket path (which clones `self.scan_cancel` into `pool.rescan`) and the in-process host
        // seam (`WorkspacePool::host_rescan`, which uses the pool's own copy). One flag, every scan.
        let scan_cancel = workspaces.scan_cancel();
        Self {
            store,
            workspaces,
            git_history: std::sync::Mutex::new(AHashMap::new()),
            git_history_open_lock: Mutex::new(()),
            registry: Mutex::new(Registry {
                sinks: AHashMap::new(),
                state: LifecycleState::Starting,
            }),
            machine_registry: Mutex::new(machine_registry),
            blob_gc_lock: RwLock::new(()),
            shutdown: std::sync::OnceLock::new(),
            scan_cancel,
            subscriber_count: AtomicUsize::new(0),
            link_count: AtomicUsize::new(0),
            relay_count: AtomicUsize::new(0),
            ever_linked: AtomicBool::new(false),
            work_inflight: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
            http_inflight: AtomicUsize::new(0),
            last_http_ms: AtomicU64::new(0),
            next_sub: AtomicU64::new(1),
            started: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Install the accept-loop shutdown signal. Called once by the daemon entry point after it
    /// builds the `watch` channel whose receiver drives the front-end accept loop. Idempotent: a
    /// second call is ignored (the first sender wins), so re-installation cannot orphan the loop.
    pub fn install_shutdown(&self, shutdown: watch::Sender<bool>) {
        let _ = self.shutdown.set(shutdown);
    }

    /// Mark the broker Active once front-ends are accepting.
    pub async fn mark_active(&self) {
        let mut reg = self.registry.lock().await;
        if reg.state == LifecycleState::Starting || reg.state == LifecycleState::Idle {
            reg.state = LifecycleState::Active;
        }
    }

    /// Current live subscriber count.
    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Record a newly connected front-end link and stamp activity.
    pub fn link_connected(&self) {
        self.link_count.fetch_add(1, Ordering::Relaxed);
        self.ever_linked.store(true, Ordering::Relaxed);
        self.touch();
    }

    /// Whether any client has ever touched this daemon, over EITHER transport: a UDS/pipe link
    /// (`ever_linked`) or a streamable-HTTP request (`last_http_ms` stamped away from its `0` epoch).
    /// Feeds the bootstrap reaper — a daemon that stays `false` past [`BOOTSTRAP_TIMEOUT`] was
    /// spawned and abandoned. Covering HTTP too keeps it from reaping a daemon serving `/ui` or
    /// `/mcp` clients that never open a persistent link.
    pub fn ever_served(&self) -> bool {
        self.ever_linked.load(Ordering::Relaxed) || self.last_http_ms.load(Ordering::Relaxed) != 0
    }

    /// Record a front-end link closing and stamp activity.
    pub fn link_disconnected(&self) {
        self.link_count.fetch_sub(1, Ordering::Relaxed);
        self.touch();
    }

    /// Stamp "now" as the last-activity time.
    pub fn touch(&self) {
        self.last_activity_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Count a newly accepted link for as long as the returned guard lives. Call this in the accept
    /// loop, BEFORE spawning the link's task — see [`LinkGuard`] for why the ordering matters.
    pub fn register_link(self: &Arc<Self>) -> LinkGuard {
        self.link_connected();
        LinkGuard { broker: self.clone() }
    }

    /// Serve one accepted RELAY connection: run the relay handshake, then — if accepted — host the
    /// full rmcp code-map router over the raw stream, sharing this workspace's one
    /// [`SharedReadStack`](crate::mcp::SharedReadStack) across every connection.
    ///
    /// The accept loop routes a connection here after peeking [`relay::RELAY_MAGIC`](super::relay::RELAY_MAGIC);
    /// this consumes the magic, reads the [`RelayHello`](super::relay::RelayHello), and validates it.
    /// A proto/view mismatch, or a failure to build the workspace's shared stack, is answered with a
    /// non-accepting [`RelayWelcome`](super::relay::RelayWelcome) so the client falls back to an
    /// in-process serve — the daemon never bricks a client. `link` is the accept-loop link guard,
    /// held for the whole session so the idle reaper counts this rmcp client as live.
    pub async fn serve_relay_connection<S>(self: Arc<Self>, mut stream: S, link: LinkGuard)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use super::relay;
        use tokio::io::AsyncReadExt as _;

        self.mark_active().await;
        let mut magic = [0u8; relay::RELAY_MAGIC.len()];
        if let Err(error) = stream.read_exact(&mut magic).await {
            tracing::warn!(%error, "relay: reading preamble failed");
            return;
        }
        if magic != relay::RELAY_MAGIC {
            tracing::warn!("relay: preamble mismatch after peek; dropping");
            return;
        }
        let hello = match relay::read_hello(&mut stream).await {
            Ok(hello) => hello,
            Err(error) => {
                tracing::warn!(%error, "relay: reading hello failed");
                return;
            }
        };

        let daemon_version = env!("CARGO_PKG_VERSION").to_string();
        let decline = |code: &str| relay::RelayWelcome {
            relay_proto_ver: relay::RELAY_PROTO_VER,
            daemon_version: daemon_version.clone(),
            accepted: false,
            code: Some(code.to_string()),
        };

        if hello.relay_proto_ver != relay::RELAY_PROTO_VER {
            let _ = relay::write_welcome(&mut stream, &decline("relay_proto_skew")).await;
            return;
        }
        if hello.view != crate::store::VIEW_WORKING {
            let _ = relay::write_welcome(&mut stream, &decline("view_unsupported")).await;
            return;
        }

        // ~keep Build/fetch the shared stack BEFORE accepting, so a build failure still lets the client
        // ~keep fall back rather than being told "accepted" against a stack we cannot serve.
        let host = Arc::clone(&self.workspaces) as Arc<dyn crate::mcp::HostBackend>;
        let git_history_host = Arc::clone(&self) as Arc<dyn crate::git_history::remote::HistoryHost>;
        let shared = match self
            .workspaces
            .get_or_build_serve_state(&hello.root, host, git_history_host)
            .await
        {
            Ok(shared) => shared,
            Err(error) => {
                tracing::warn!(%error, root = %hello.root.display(), "relay: hosting read stack failed");
                let _ = relay::write_welcome(&mut stream, &decline("host_build_failed")).await;
                return;
            }
        };
        let _conn = match self.workspaces.begin_conn(&hello.root) {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(%error, "relay: connection accounting failed");
                let _ = relay::write_welcome(&mut stream, &decline("host_build_failed")).await;
                return;
            }
        };
        let Some(_relay) = self.try_register_relay().await else {
            let _ = relay::write_welcome(&mut stream, &decline("daemon_draining")).await;
            return;
        };

        let welcome = relay::RelayWelcome {
            relay_proto_ver: relay::RELAY_PROTO_VER,
            daemon_version,
            accepted: true,
            code: None,
        };
        if let Err(error) = relay::write_welcome(&mut stream, &welcome).await {
            tracing::warn!(%error, "relay: sending welcome failed");
            return;
        }

        let agent = hello.agent.to_string();
        tracing::info!(agent = %agent, root = %hello.root.display(), "relay: hosting rmcp session");
        let server = crate::mcp::BasemindServer::from_shared(shared, agent);
        let (read_half, write_half) = tokio::io::split(stream);
        match rmcp::ServiceExt::serve(server, (read_half, write_half)).await {
            Ok(running) => {
                if let Err(error) = running.waiting().await {
                    tracing::info!(%error, "relay: rmcp session ended with error");
                }
            }
            Err(error) => tracing::warn!(%error, "relay: rmcp serve failed to start"),
        }
        drop(_conn);
        drop(link);
    }

    /// Mark a unit of daemon-internal work as running for as long as the returned guard lives, so
    /// the idle reaper cannot mistake it for idleness. See [`WorkGuard`].
    pub fn begin_work(&self) -> WorkGuard<'_> {
        self.work_inflight.fetch_add(1, Ordering::SeqCst);
        WorkGuard { broker: self }
    }

    /// Note a client-less work unit finished: drop the in-flight count and stamp activity. The
    /// counterpart to [`begin_work`](Self::begin_work), called by [`WorkGuard`]'s `Drop`.
    pub(super) fn end_work(&self) {
        self.work_inflight.fetch_sub(1, Ordering::SeqCst);
        self.touch();
    }

    /// Number of daemon-internal work units currently running. Exposed for tests.
    pub fn work_inflight(&self) -> usize {
        self.work_inflight.load(Ordering::SeqCst)
    }

    /// What "idle" MEANS — and why each clause is here.
    ///
    /// A daemon is idle only when *nothing can be waiting on it*. Four independent things can make
    /// that false, and socket traffic is NOT one of them:
    ///
    /// 1. **A connected link** (`link_count`). This is the load-bearing clause, and it is a
    ///    refcount, never a timestamp, precisely because a busy daemon can be silent for minutes.
    ///    Every client blocks on its socket for the whole RPC — a forwarded `Rescan` or a
    ///    git-history build on a 243 k-commit repo runs ~75 s with ZERO bytes crossing the socket —
    ///    so the link stays open and counted for the full duration of the work. Timing out on
    ///    *silence* would kill exactly those long builds; counting *links* cannot.
    /// 2. **Daemon-internal work** (`work_inflight`). The one class of work no link covers: sweeps
    ///    the daemon starts on its own, above all the cross-workspace blob GC, which runs with no
    ///    client attached and must not be torn mid-sweep. [`Broker::begin_work`] pins these.
    /// 3. **A streamable-HTTP request in flight** (`http_inflight`). The HTTP front-end is stateless
    ///    — no persistent connection survives between requests — so the link refcount cannot pin it.
    ///    [`Broker::begin_http_request`] fills the gap, and also stamps `last_http_ms` so a burst of
    ///    short requests keeps the daemon warm across the gaps between them.
    /// 4. **An already-started drain.** Draining/Stopped is terminal; re-reaping is meaningless.
    ///
    /// Only once all four are clear does the elapsed-time test apply. `last_activity_ms` is stamped
    /// on every link connect/disconnect and `last_http_ms` on every HTTP request, so the window
    /// measures time since the daemon last had *anyone* to serve — over either front-end — not time
    /// since the last packet. See [`Broker::elapsed_since_activity`].
    pub async fn is_idle_for(&self, idle_for: Duration) -> bool {
        if self.link_count.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if self.work_inflight.load(Ordering::SeqCst) != 0 {
            return false;
        }
        // A stateless HTTP request in flight pins the daemon just like a connected UDS link. ~keep
        if self.http_inflight.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if matches!(self.state().await, LifecycleState::Draining | LifecycleState::Stopped) {
            return false;
        }
        self.elapsed_since_activity() >= idle_for.as_millis() as u64
    }

    /// Milliseconds since the daemon last had anyone to serve — the max recency across UDS links
    /// (`last_activity_ms`) and streamable-HTTP requests (`last_http_ms`). Taking the more-recent of
    /// the two is what stops the reaper from tearing down a daemon that is busy over HTTP but silent
    /// on the socket (or vice versa). Both fields start at `0` ("epoch"), so a daemon that has never
    /// seen either kind of traffic still reaps on the normal window.
    fn elapsed_since_activity(&self) -> u64 {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let last = self
            .last_activity_ms
            .load(Ordering::SeqCst)
            .max(self.last_http_ms.load(Ordering::SeqCst));
        now_ms.saturating_sub(last)
    }

    /// The idle reaper's ONE entry point: re-check idleness and flip to `Draining` under the
    /// registry lock, returning whether this call is the one that started the drain.
    ///
    /// The lock makes the check-and-set atomic *against other drains* — two reapers, or a reaper
    /// racing a `Stop` RPC, cannot both decide they own the drain. It does NOT serialize against the
    /// accept loop, which bumps `link_count` with a bare atomic and never takes this lock: a
    /// connection can still be accepted in the instant between our zero-link read and the shutdown
    /// signal landing.
    ///
    /// That residual interleaving is deliberately handled *downstream* rather than excluded here,
    /// because it cannot be excluded here — see [`Broker::drain_links`]. The late link is counted
    /// before its task is spawned, the front-end waits for it after it stops accepting, and so it is
    /// served to completion instead of being torn. Excluding it up front would mean taking the
    /// registry lock on every accept — real contention on the hot path to close a window that the
    /// drain already closes for free.
    pub async fn try_begin_idle_drain(&self, idle_for: Duration) -> bool {
        let sinks: Vec<mpsc::Sender<CommsOut>> = {
            let mut reg = self.registry.lock().await;
            if matches!(reg.state, LifecycleState::Draining | LifecycleState::Stopped) {
                return false;
            }
            if self.link_count.load(Ordering::SeqCst) != 0
                || self.work_inflight.load(Ordering::SeqCst) != 0
                || self.http_inflight.load(Ordering::SeqCst) != 0
            {
                return false;
            }
            if self.elapsed_since_activity() < idle_for.as_millis() as u64 {
                return false;
            }
            reg.state = LifecycleState::Draining;
            reg.sinks.values().map(|s| s.tx.clone()).collect()
        };
        self.finish_drain(sinks).await;
        true
    }

    /// Wait for every link accepted before the drain to finish its in-flight request, up to
    /// `grace`. Returns the number of links still open when we stopped waiting (0 on a clean drain).
    ///
    /// This is what makes the reap non-destructive. The front-end calls it AFTER it has unlinked the
    /// socket and stopped accepting, which orders the two halves of the exit correctly:
    ///
    /// * The socket is gone first, so a client that has not connected yet fails at `connect()` and
    ///   its `ensure_daemon` spawns a fresh daemon — it never talks to a dying one.
    /// * A connection sitting unaccepted in the kernel backlog is closed by the listener drop. The
    ///   client sees EOF *before any reply*, and the daemon provably never read a byte of it, so the
    ///   client's single-shot reconnect-and-retry replays it against the fresh daemon exactly once —
    ///   no duplicate mutation. (This backlog window is inherent: `connect()` completes in the
    ///   kernel without the daemon's participation, so no daemon-side lock can exclude it. It is
    ///   closed on the client, not here.)
    /// * A link that WAS accepted is finished here rather than torn, which is what lets the retry
    ///   above be safe: the daemon never dies holding a dispatched-but-unanswered request.
    pub async fn drain_links(&self, grace: Duration) -> usize {
        let deadline = Instant::now() + grace;
        loop {
            let open = self.link_count.load(Ordering::SeqCst);
            if open == 0 {
                return 0;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    open,
                    "comms: links still open at the end of the drain grace; exiting anyway"
                );
                return open;
            }
            tokio::time::sleep(DRAIN_POLL_EVERY).await;
        }
    }

    /// Archive every active thread idle past `ttl`. Returns the count archived. Best-effort — a
    /// store error is surfaced to the caller (the daemon logs it). This is the reaper hook.
    pub fn archive_idle_threads(&self, ttl: Duration) -> Result<usize, CommsStoreError> {
        self.store.archive_idle(ttl)
    }

    /// Permanently reclaim archived threads idle past `ttl` (row + messages + members + cursors).
    /// Returns the count purged. The retention tail after [`archive_idle_threads`](Self::archive_idle_threads);
    /// a store error is surfaced to the caller (the daemon logs it).
    pub fn purge_archived_threads(&self, ttl: Duration) -> Result<usize, CommsStoreError> {
        self.store.purge_archived(ttl)
    }

    /// Shed hot workspaces idle past `ttl` from RAM (their on-disk cache survives). Returns the
    /// count evicted. The daemon's periodic sweep calls this so cold indexes free memory.
    pub fn evict_idle_workspaces(&self, ttl: Duration) -> usize {
        self.git_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, entry| entry.idle_for() < ttl);
        self.workspaces.evict_idle(ttl)
    }

    /// Reap orphaned workspace cache dirs, then reference-count the machine-global blob store across
    /// every *surviving* workspace and reap orphan blobs — both under the WRITE side of
    /// [`Broker::blob_gc_lock`], so no rescan is writing blobs mid-sweep. Only the daemon calls this:
    /// it alone sees every workspace, the precondition for a safe cross-workspace sweep.
    ///
    /// The workspace reap runs FIRST and inside the same guard on purpose. A workspace whose worktree
    /// was deleted still votes in the blob GC's live set, pinning its blobs in the global store
    /// forever; dropping its vote in the same sweep means those blobs are reclaimed immediately
    /// instead of surviving until the next cycle. The blocking filesystem work runs off the reactor.
    ///
    /// Held under a [`WorkGuard`] for its whole duration: this is the one long-running thing the
    /// daemon does with no client attached, so without it the idle reaper could fire mid-sweep and
    /// take the process down between the workspace reap and the blob reap.
    pub async fn run_blob_gc(&self) -> Result<crate::store_gc::GcReport, crate::store_gc::GcError> {
        self.run_blob_gc_with_lock_timeout(GC_LOCK_TIMEOUT).await
    }

    /// [`Broker::run_blob_gc`] with an explicit bound on how long the sweep waits for the
    /// blob-GC write lock. The wait must be bounded: a runaway rescan holds the read side for
    /// its whole (potentially unbounded) duration, and an unbounded `write().await` here is how
    /// the maintenance loop silently parked forever while the cache grew to 116 GB. On timeout
    /// the cycle returns [`GcError::Starved`](crate::store_gc::GcError::Starved) — skipped and
    /// retried on the next tick, never wedged. Test seam: tests pass a tiny bound.
    pub(crate) async fn run_blob_gc_with_lock_timeout(
        &self,
        lock_timeout: std::time::Duration,
    ) -> Result<crate::store_gc::GcReport, crate::store_gc::GcError> {
        let _working = self.begin_work();
        let Ok(_sweep_guard) = tokio::time::timeout(lock_timeout, self.blob_gc_lock.write()).await else {
            let error = crate::store_gc::GcError::Starved(lock_timeout);
            crate::store_gc::persist_gc_error(&error);
            return Err(error);
        };
        let result = match tokio::task::spawn_blocking(crate::store_gc::reap_gc_and_enforce_budget).await {
            Ok(result) => result,
            Err(join) => Err(crate::store_gc::GcError::Join(join.to_string())),
        };
        if let Err(error) = &result {
            crate::store_gc::persist_gc_error(error);
        }
        result
    }

    /// Handle one request on a link. Returns the direct response.
    pub async fn handle(
        &self,
        req: CommsRequest,
        session: &mut Session,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> CommsResponse {
        self.touch();
        match self.dispatch(req, session, link_tx).await {
            Ok(resp) => resp,
            Err(e) => CommsResponse::Error {
                code: "store_error".to_string(),
                message: e.to_string(),
            },
        }
    }

    async fn dispatch(
        &self,
        req: CommsRequest,
        session: &mut Session,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> Result<CommsResponse, CommsStoreError> {
        match req {
            CommsRequest::Hello {
                agent,
                proto_ver,
                remote,
                cwd,
            } => {
                let resp = self.on_hello(agent, proto_ver, remote, cwd.clone(), session)?;
                if let (CommsResponse::Welcome { .. }, Some(root)) = (&resp, cwd) {
                    let mut registry = self.machine_registry.lock().await;
                    if let Err(error) = registry.register_workspace(&root) {
                        tracing::warn!(%error, root = %root.display(), "comms: registry auto-register failed");
                    }
                }
                Ok(resp)
            }
            CommsRequest::Register { card } => self.on_register(session, card),
            CommsRequest::ListAgents { thread } => self.on_list_agents(thread),
            CommsRequest::ThreadStart { subject, path, members } => {
                self.on_thread_start(session, subject, path, members)
            }
            CommsRequest::ThreadJoin { thread } => self.on_thread_join(session, thread),
            CommsRequest::ThreadLeave { thread } => self.on_thread_leave(session, thread),
            CommsRequest::ThreadList {
                remote,
                cwd,
                subject_contains,
                include_archived,
            } => self.on_thread_list(session, remote, cwd, subject_contains, include_archived),
            CommsRequest::ThreadPost {
                thread,
                subject,
                tags,
                reply_to,
                body,
            } => self.on_post(session, thread, subject, tags, reply_to, body).await,
            CommsRequest::ThreadHistory {
                thread,
                cursor,
                limit,
                since_micros,
            } => self.on_history(thread, cursor, limit, since_micros),
            CommsRequest::ThreadMembers { thread } => self.on_thread_members(thread),
            CommsRequest::ThreadAddMember { thread, member } => self.on_thread_add_member(session, thread, member),
            CommsRequest::ThreadRemoveMember { thread, member } => {
                self.on_thread_remove_member(session, thread, member)
            }
            CommsRequest::ThreadArchive { thread } => self.on_thread_archive(session, thread),
            CommsRequest::GetBody { message_id } => self.on_get_body(message_id),
            CommsRequest::Inbox {
                cursor,
                limit,
                mark_read,
                since_micros,
                ..
            } => self.on_inbox(session, cursor, limit, mark_read, since_micros),
            CommsRequest::AckInbox {
                message_ids,
                thread,
                to_seq,
            } => self.on_ack(session, message_ids, thread, to_seq),
            CommsRequest::Subscribe { thread } => self.on_subscribe(session, thread, link_tx).await,
            CommsRequest::SubscribeInbox { thread } => self.on_subscribe_inbox(session, thread, link_tx).await,
            CommsRequest::Unsubscribe { sub } => self.on_unsubscribe(sub).await,
            CommsRequest::Rescan {
                root,
                paths,
                full,
                embed,
            } => Ok(self.on_rescan(root, paths, full, embed).await),
            CommsRequest::ResolvedRefs { root, query } => Ok(self.on_resolved_refs(root, query).await),
            CommsRequest::CodeSearchLanes { root, query } => Ok(self.on_code_search_lanes(root, query).await),
            CommsRequest::GitHistory { root, op } => Ok(self.on_git_history(root, op).await),
            #[cfg(feature = "memory")]
            CommsRequest::Memory { root, scope, op } => Ok(self.on_memory(root, scope, op).await),
            #[cfg(feature = "memory")]
            CommsRequest::Governance { root, scope, op } => Ok(self.on_governance(root, scope, op).await),
            CommsRequest::AccessedPaths => Ok(self.on_accessed_paths()),
            CommsRequest::WorkspacesList => Ok(self.on_workspaces_list().await),
            CommsRequest::WorktreesList { repo_id } => Ok(self.on_worktrees_list(repo_id).await),
            CommsRequest::BranchesList { repo_id } => Ok(self.on_branches_list(repo_id).await),
            CommsRequest::WorktreeClaim {
                repo_id,
                name,
                claimant,
            } => Ok(self.on_worktree_claim(repo_id, name, claimant).await),
            CommsRequest::WorktreeRelease {
                repo_id,
                name,
                claimant,
            } => Ok(self.on_worktree_release(repo_id, name, claimant).await),
            CommsRequest::Ping => Ok(CommsResponse::Pong),
            CommsRequest::Status => Ok(self.on_status().await),
            CommsRequest::Stop => Ok(self.on_stop().await),
        }
    }

    /// Scan/rescan a workspace on the sole-writer pool. The scan is CPU-bound, so it runs on a
    /// blocking thread while the reactor keeps serving other links. A scan/store error becomes a
    /// `CommsResponse::Error` (never a torn link).
    async fn on_rescan(
        &self,
        root: std::path::PathBuf,
        paths: Option<Vec<std::path::PathBuf>>,
        full: bool,
        embed: bool,
    ) -> CommsResponse {
        self.mark_active().await;
        // A tripped scan token means the daemon is draining (every drain route cancels first, ~keep
        // and the token never un-trips). Launching a scan now would only produce a doomed ~keep
        // partial pass that defeats coalescing — refuse instead; the client retries against ~keep
        // the next daemon. ~keep
        if self.scan_cancel.is_cancelled() {
            return CommsResponse::Error {
                code: "rescan_draining".to_string(),
                message: "daemon draining; rescan refused (retry against the next daemon)".to_string(),
            };
        }
        let _rescan_guard = self.blob_gc_lock.read().await;
        let pool = Arc::clone(&self.workspaces);
        let cancel = self.scan_cancel.clone();
        let started = Instant::now();
        match tokio::task::spawn_blocking(move || pool.rescan(&root, paths, full, embed, &cancel)).await {
            // A cancelled pass committed only part of the tree — surface it as an error so no ~keep
            // client mistakes the partial index for a completed rescan. ~keep
            Ok(Ok((_, true))) => CommsResponse::Error {
                code: "rescan_cancelled".to_string(),
                message: "daemon draining; partial scan committed".to_string(),
            },
            Ok(Ok((stats, false))) => CommsResponse::Rescanned {
                scanned: stats.scanned,
                updated: stats.updated,
                docs_indexed: stats.docs_indexed,
                removed: stats.removed,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "rescan_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "rescan_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Report the daemon's currently-hot workspaces for the statusline.
    fn on_accessed_paths(&self) -> CommsResponse {
        CommsResponse::Accessed {
            workspaces: self.workspaces.accessed(),
        }
    }

    /// List every registered workspace in the machine registry.
    async fn on_workspaces_list(&self) -> CommsResponse {
        let registry = self.machine_registry.lock().await;
        CommsResponse::Workspaces {
            workspaces: registry.workspaces(),
        }
    }

    /// List a registered repo's worktrees. An unknown repo id yields an empty list.
    async fn on_worktrees_list(&self, repo_id: String) -> CommsResponse {
        let registry = self.machine_registry.lock().await;
        CommsResponse::Worktrees {
            worktrees: registry.worktrees(&repo_id),
        }
    }

    /// List a registered repo's local branches. An unknown repo id yields an empty list.
    async fn on_branches_list(&self, repo_id: String) -> CommsResponse {
        let registry = self.machine_registry.lock().await;
        CommsResponse::Branches {
            branches: registry.branches(&repo_id),
        }
    }

    /// Advisory-claim a worktree. An unknown `(repo_id, name)` returns `held = false`.
    async fn on_worktree_claim(&self, repo_id: String, name: String, claimant: String) -> CommsResponse {
        let mut registry = self.machine_registry.lock().await;
        match registry.claim_worktree(&repo_id, &name, &claimant) {
            Ok(held) => CommsResponse::ClaimOutcome { held },
            Err(error) => registry_error(error),
        }
    }

    /// Drop machine-registry rows whose on-disk path is gone, returning how many were removed.
    ///
    /// The registry is append-only by construction: a row is written when a workspace registers and
    /// nothing ever retires it, so every throwaway checkout and test tempdir stays forever and buries
    /// the live repos `workspace workspaces` exists to surface. Called from the daemon's periodic
    /// maintenance pass; a failure is logged and reported as zero rather than propagated, because a
    /// prune is opportunistic housekeeping and must never take the daemon down.
    pub async fn prune_missing_registry_rows(&self) -> usize {
        let mut registry = self.machine_registry.lock().await;
        match registry.prune_missing() {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(%error, "comms: machine registry prune failed");
                0
            }
        }
    }

    /// Release an advisory worktree claim held by `claimant`.
    async fn on_worktree_release(&self, repo_id: String, name: String, claimant: String) -> CommsResponse {
        let mut registry = self.machine_registry.lock().await;
        match registry.release_worktree(&repo_id, &name, &claimant) {
            Ok(held) => CommsResponse::ClaimOutcome { held },
            Err(error) => registry_error(error),
        }
    }

    /// Enter the Draining state, notify every live sink to disconnect, and fire the accept-loop
    /// shutdown signal so the front-end stops accepting. Firing the signal is what makes a `Stop`
    /// RPC (and SIGTERM/idle-reap/ownership-loss, which all route here) actually terminate the
    /// daemon rather than merely notify connected clients. Idempotent — repeated drains re-send
    /// `true`, which the watch receiver already holds.
    pub async fn begin_drain(&self) {
        let sinks: Vec<mpsc::Sender<CommsOut>> = {
            let mut reg = self.registry.lock().await;
            reg.state = LifecycleState::Draining;
            reg.sinks.values().map(|s| s.tx.clone()).collect()
        };
        self.finish_drain(sinks).await;
    }

    /// The tail shared by [`Broker::begin_drain`] and [`Broker::try_begin_idle_drain`]: tell every
    /// live sink we are going away, then fire the accept-loop shutdown signal. Split out so the
    /// idle path can make its decision under the registry lock without holding it across the sends.
    pub(super) async fn finish_drain(&self, sinks: Vec<mpsc::Sender<CommsOut>>) {
        // Trip the scan token FIRST: a mid-flight rescan must start winding down before (not ~keep
        // after) clients are told to disconnect, or the runtime teardown blocks on it. ~keep
        self.scan_cancel.cancel();
        for tx in sinks {
            let _ = tx.send(CommsOut::Notification(CommsNotification::Shutdown)).await;
        }
        if let Some(shutdown) = self.shutdown.get() {
            let _ = shutdown.send(true);
        }
    }

    /// Current lifecycle state.
    pub async fn state(&self) -> LifecycleState {
        self.registry.lock().await.state
    }
}

/// Per-link session context. Established by `Hello`, then read by every subsequent handler on
/// that link.
#[derive(Default)]
pub struct Session {
    /// The authenticated agent id for this link.
    pub agent: Option<AgentId>,
    /// The scope chain captured at Hello, used for path-glob discovery.
    pub chain: Option<ScopeChain>,
}

/// Map a [`RegistryError`](crate::registry::RegistryError) (only surfaced on a claim/release
/// persist failure) into a stable-token error response.
fn registry_error(error: crate::registry::RegistryError) -> CommsResponse {
    CommsResponse::Error {
        code: "registry_error".to_string(),
        message: error.to_string(),
    }
}

#[path = "daemon_threads.rs"]
pub(super) mod threads;
#[cfg(test)]
use super::model::now_micros;
#[cfg(test)]
use super::protocol::{PROTO_VER, SeqMeta};
#[cfg(test)]
use threads::{sanitize_id, validate_dimensions};

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "daemon_forward_tests.rs"]
mod forward_tests;
