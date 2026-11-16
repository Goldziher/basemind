//! The broker: the single owner of all comms state.
//!
//! [`Broker`] holds the agent store ([`AgentHub`]), the hot workspaces and the machine registry,
//! and handles each [`CommsRequest`]. `Hello` fixes who a link speaks for (ADR-0021); the agent
//! requests act as that identity and are answered in `daemon_agents.rs`.
//!
//! ## Lifecycle
//!
//! `Starting → Active → Draining → Stopped`. `Draining` stops accepting, flushes, then releases
//! the flock and unlinks the socket on the way to `Stopped`.
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
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::agents_hub::AgentHub;
use super::git_history_ops::HistoryEntry;
use super::protocol::{CommsOut, CommsRequest, CommsResponse};
use super::v2::api::{Principal, RepoKey};
use super::workspace_pool::{self, WorkspacePool};
use crate::registry::Registry as MachineRegistry;

/// Idle window after which a daemon with no connected links and no work in flight self-terminates.
///
/// A `basemind serve` session holds one relay link for as long as it lasts, so the window starts
/// when the last session closes. The other callers (the CLI verbs, the MCP comms helpers) connect,
/// do their RPC and drop the client; with only those, `link_count == 0` between requests and the
/// window is "N minutes since anyone last called us". Reaping early is cheap (a respawn re-OPENS the on-disk
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
/// without sleeping for ten minutes; also a field escape hatch for a machine that wants daemons to
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
const DRAIN_POLL_EVERY: Duration = Duration::from_millis(25);
/// How long a GC cycle waits for the blob-GC write lock before declaring itself starved and
/// skipping the cycle. Every rescan holds the read side for its whole duration, so this must be
/// long enough to outlast a legitimate big-monorepo scan yet bounded — an unbounded wait behind
/// a runaway rescan is how the maintenance loop silently parked forever (116 GB incident).
const GC_LOCK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub use super::daemon_guards::{LinkGuard, RelayGuard, WorkGuard};

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
    /// Serving.
    Active,
    /// Stop requested: refusing new work, flushing.
    Draining,
    /// Fully stopped; flock released, socket unlinked.
    Stopped,
}

/// In-RAM broker state behind a single async mutex.
pub(super) struct Registry {
    /// Current lifecycle state.
    pub(super) state: LifecycleState,
}

/// The broker. Cheap to share via `Arc`; every front-end and link holds one.
pub struct Broker {
    /// The agent store and everything the daemon keeps beside it (ADR-0021).
    pub(super) agents: Arc<AgentHub>,
    /// Process-wide heavy-work gate shared by every relay and HTTP MCP connection.
    pub(crate) heavy_admission: Arc<crate::mcp::admission::HeavyAdmission>,
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
    machine_registry: Arc<Mutex<MachineRegistry>>,
    /// The accept-loop shutdown signal, installed by the daemon entry point after it builds the
    /// `watch` channel. `begin_drain` fires it so a `Stop` RPC (or any drain) actually breaks the
    /// front-end accept loop — not just notifies connected sinks. Absent in tests that drive the
    /// broker directly, where `begin_drain` still transitions state and notifies sinks.
    pub(super) shutdown: std::sync::OnceLock<watch::Sender<bool>>,
    /// Cooperative cancellation for in-flight scans. Every drain route ends in `finish_drain`,
    /// which trips this FIRST — so a `Stop` RPC, SIGTERM, the idle reaper, and socket-ownership
    /// loss all interrupt a mid-scan `spawn_blocking` at per-file granularity instead of letting
    /// the runtime teardown block on it (previously only SIGKILL could end a scanning daemon).
    pub(super) scan_cancel: crate::scanner::ScanCancel,
    link_count: AtomicUsize,
    pub(super) relay_count: AtomicUsize,
    /// Relay links whose handshake has begun and not yet ended in a registered session or a
    /// refusal. A `Stop` is refused while any is open; see [`Broker::begin_relay_handshake`].
    pub(super) relay_handshakes: AtomicUsize,
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
    pub(super) started: Instant,
    pub(super) version: String,
}

impl Broker {
    /// Construct a broker over an opened agent store, opening the machine registry from the
    /// machine-global cache. A registry-open failure degrades to an empty in-memory registry (rooted
    /// at a throwaway path) rather than failing the daemon — coordination tools then return empty
    /// until a workspace registers. Use [`Broker::with_registry`] to inject a registry (tests).
    pub fn new(agents: AgentHub) -> Self {
        let registry = MachineRegistry::from_data_home().unwrap_or_else(|error| {
            tracing::warn!(%error, "comms: machine registry open failed; using an empty in-memory registry");
            MachineRegistry::open(
                &std::env::temp_dir().join(format!("basemind-registry-fallback-{}", std::process::id())),
            )
            .expect("open fallback registry in temp dir")
        });
        Self::with_registry(agents, registry)
    }

    /// Construct a broker over an opened agent store and an explicit machine registry. The daemon
    /// owns the registry as its sole writer; the coordination tools read/mutate through it. Tests
    /// inject an isolated registry here.
    pub fn with_registry(agents: AgentHub, machine_registry: MachineRegistry) -> Self {
        let workspaces = Arc::new(WorkspacePool::new(workspace_pool::DEFAULT_HOT_CAP));
        // Adopt the pool's drain token so `begin_drain`'s cancel reaches BOTH dispatch paths: the
        // socket path (which clones `self.scan_cancel` into `pool.rescan`) and the in-process host
        // seam (`WorkspacePool::host_rescan`, which uses the pool's own copy). One flag, every scan.
        let scan_cancel = workspaces.scan_cancel();
        Self {
            agents: Arc::new(agents),
            heavy_admission: Arc::new(crate::mcp::admission::HeavyAdmission::default()),
            workspaces,
            git_history: std::sync::Mutex::new(AHashMap::new()),
            git_history_open_lock: Mutex::new(()),
            registry: Mutex::new(Registry {
                state: LifecycleState::Starting,
            }),
            machine_registry: Arc::new(Mutex::new(machine_registry)),
            shutdown: std::sync::OnceLock::new(),
            scan_cancel,
            link_count: AtomicUsize::new(0),
            relay_count: AtomicUsize::new(0),
            relay_handshakes: AtomicUsize::new(0),
            ever_linked: AtomicBool::new(false),
            work_inflight: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
            http_inflight: AtomicUsize::new(0),
            last_http_ms: AtomicU64::new(0),
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
        if reg.state == LifecycleState::Starting {
            reg.state = LifecycleState::Active;
        }
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
    /// this consumes the magic, reads the [`RelayHello`](super::relay::RelayHello) within
    /// [`HELLO_TIMEOUT`](super::relay::HELLO_TIMEOUT), and validates it.
    /// A proto/view mismatch, or a failure to build the workspace's shared stack, is answered with a
    /// non-accepting [`RelayWelcome`](super::relay::RelayWelcome) whose token tells the client
    /// whether to retry. `link` is the accept-loop link guard, held for the whole session so the
    /// idle reaper counts this rmcp client as live.
    pub async fn serve_relay_connection<S>(self: Arc<Self>, mut stream: S, link: LinkGuard)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use super::relay;

        let handshake = self.begin_relay_handshake();
        self.mark_active().await;
        let hello = match relay::accept_hello(&mut stream).await {
            Ok(hello) => hello,
            Err(error) => {
                tracing::warn!(%error, "relay: reading the preamble and hello failed; dropping the link");
                return;
            }
        };

        let daemon_version = env!("CARGO_PKG_VERSION").to_string();
        let decline = |code: &str, detail: Option<String>| relay::RelayWelcome {
            relay_proto_ver: relay::RELAY_PROTO_VER,
            daemon_version: daemon_version.clone(),
            accepted: false,
            code: Some(code.to_string()),
            detail,
        };

        if hello.relay_proto_ver != relay::RELAY_PROTO_VER {
            let _ = relay::write_welcome(&mut stream, &decline(relay::decline::PROTO_SKEW, None)).await;
            return;
        }
        if hello.view != crate::store::VIEW_WORKING {
            let _ = relay::write_welcome(&mut stream, &decline(relay::decline::VIEW_UNSUPPORTED, None)).await;
            return;
        }

        // ~keep Build/fetch the shared stack BEFORE accepting, so a build failure reaches the client
        // ~keep as a decline it can act on rather than "accepted" against a stack we cannot serve.
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
                let (code, detail) = super::relay_decline::of_build_failure(&error);
                let _ = relay::write_welcome(&mut stream, &decline(code, detail)).await;
                return;
            }
        };
        let _conn = match self.workspaces.begin_conn_async(&hello.root).await {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!(%error, "relay: connection accounting failed");
                let (code, detail) = super::relay_decline::of_build_failure(&anyhow::Error::new(error));
                let _ = relay::write_welcome(&mut stream, &decline(code, detail)).await;
                return;
            }
        };
        let Some(_relay) = self.try_register_relay().await else {
            let _ = relay::write_welcome(&mut stream, &decline(relay::decline::DAEMON_DRAINING, None)).await;
            return;
        };
        drop(handshake);

        let welcome = relay::RelayWelcome {
            relay_proto_ver: relay::RELAY_PROTO_VER,
            daemon_version,
            accepted: true,
            code: None,
            detail: None,
        };
        if let Err(error) = relay::write_welcome(&mut stream, &welcome).await {
            tracing::warn!(%error, "relay: sending welcome failed");
            return;
        }

        let agent = hello.agent.to_string();
        tracing::info!(agent = %agent, root = %hello.root.display(), "relay: hosting rmcp session");
        let server = crate::mcp::BasemindServer::from_shared(shared, agent, Arc::clone(&self.heavy_admission));
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
        {
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
        }
        self.finish_drain().await;
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

    /// Shed hot workspaces idle past `ttl` from RAM (their on-disk cache survives). Returns the
    /// count evicted. The daemon's periodic sweep calls this so cold indexes free memory.
    pub fn evict_idle_workspaces(&self, ttl: Duration) -> usize {
        self.git_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, entry| entry.idle_for() < ttl);
        self.workspaces.evict_idle(ttl)
    }

    /// Shed hosted READ STACKS idle past `ttl` while keeping their hot store entries, returning the
    /// count dropped. Swept on its own, much shorter clock than
    /// [`evict_idle_workspaces`](Self::evict_idle_workspaces): the stack is the O(corpus) half of a
    /// hot workspace and is rebuilt from the same blobs on the next connection.
    pub fn evict_idle_read_stacks(&self, ttl: Duration) -> usize {
        self.workspaces.evict_idle_read_stacks(ttl)
    }

    /// Reap orphaned workspace cache dirs, then reference-count the machine-global blob store across
    /// every *surviving* workspace and reap orphan blobs — both holding the workspace pool's blob-GC
    /// gate exclusively (see [`super::blob_gc_gate`]), so no scan the daemon runs is writing blobs
    /// mid-sweep. Only the daemon calls this:
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
    /// blob-GC gate. The wait must be bounded: a runaway rescan holds the shared side for
    /// its whole (potentially unbounded) duration, and an unbounded `write().await` here is how
    /// the maintenance loop silently parked forever while the cache grew to 116 GB. On timeout
    /// the cycle returns [`GcError::Starved`](crate::store_gc::GcError::Starved) — skipped and
    /// retried on the next tick, never wedged. Test seam: tests pass a tiny bound.
    pub(crate) async fn run_blob_gc_with_lock_timeout(
        &self,
        lock_timeout: std::time::Duration,
    ) -> Result<crate::store_gc::GcReport, crate::store_gc::GcError> {
        let _working = self.begin_work();
        let gate = self.workspaces.blob_gc_gate();
        let swept = tokio::task::spawn_blocking(move || {
            let Some(_sweeping) = gate.try_sweep(lock_timeout) else {
                return Err(crate::store_gc::GcError::Starved(lock_timeout));
            };
            crate::store_gc::reap_gc_and_enforce_budget()
        });
        let result = match swept.await {
            Ok(result) => result,
            Err(join) => Err(crate::store_gc::GcError::Join(join.to_string())),
        };
        if let Err(error) = &result {
            crate::store_gc::persist_gc_error(error);
        }
        result
    }

    /// The signal raised when a pass displaced artifacts (see
    /// [`WorkspacePool::cleanup_signal`](super::workspace_pool::WorkspacePool)); the daemon's cleanup
    /// task waits on it to run a prompt blob sweep.
    pub fn cleanup_signal(&self) -> Arc<tokio::sync::Notify> {
        self.workspaces.cleanup_signal()
    }

    /// Handle one request on a link and frame its answer: a [`CommsRequest::Call`] is unwrapped and
    /// answered with a [`CommsOut::Reply`] echoing its id; a bare request gets the uncorrelated
    /// [`CommsOut::Response`]. `gone` is cancelled when the client hangs up mid-request.
    pub async fn handle_framed(&self, req: CommsRequest, session: &mut Session, gone: &CancellationToken) -> CommsOut {
        match req {
            CommsRequest::Call { id, request } => CommsOut::Reply {
                id,
                response: self.handle(*request, session, gone).await,
            },
            bare => CommsOut::Response(self.handle(bare, session, gone).await),
        }
    }

    /// Handle one request on a link. Returns the direct response.
    pub async fn handle(&self, req: CommsRequest, session: &mut Session, gone: &CancellationToken) -> CommsResponse {
        self.touch();
        match req {
            CommsRequest::Hello {
                proto_ver,
                principal,
                root,
                host,
            } => self.on_hello(proto_ver, principal, root, host, session).await,
            CommsRequest::AgentPost { sub, body } => self.on_agent_post(session, sub, body).await,
            CommsRequest::AgentInbox { sub, body, wait_secs } => {
                self.on_agent_inbox(session, sub, body, wait_secs, gone).await
            }
            CommsRequest::AgentThreads { sub, query } => self.on_agent_threads(session, sub, query).await,
            CommsRequest::AgentThreadUpdate { sub, update } => self.on_agent_thread_update(session, sub, update).await,
            CommsRequest::AgentStatus => self.on_agent_status(session),
            CommsRequest::AgentCleanup { archive } => self.on_agent_cleanup(archive).await,
            CommsRequest::Rescan {
                root,
                paths,
                full,
                embed,
                verify,
            } => self.on_rescan(root, paths, full, embed, verify).await,
            CommsRequest::ResolvedRefs { root, query } => self.on_resolved_refs(root, query).await,
            CommsRequest::IndexRead { root, query } => self.on_index_read(root, query).await,
            CommsRequest::CodeSearchLanes { root, query } => self.on_code_search_lanes(root, query).await,
            CommsRequest::GitHistory { root, op } => self.on_git_history(root, op).await,
            #[cfg(feature = "memory")]
            CommsRequest::Memory { root, scope, op } => self.on_memory(root, scope, op).await,
            CommsRequest::AccessedPaths => self.on_accessed_paths(),
            CommsRequest::WorkspacesList => self.on_workspaces_list().await,
            CommsRequest::WorktreesList { repo_id } => self.on_worktrees_list(repo_id).await,
            CommsRequest::BranchesList { repo_id } => self.on_branches_list(repo_id).await,
            CommsRequest::Ping => CommsResponse::Pong,
            CommsRequest::Status => self.on_status(),
            CommsRequest::Stop => self.on_stop().await,
            // `handle_framed` unwraps the one legal level of `Call`; a nested one is malformed.
            CommsRequest::Call { .. } => CommsResponse::Error {
                code: "nested_call".to_string(),
                message: "a correlated call cannot wrap another call".to_string(),
            },
        }
    }

    /// The daemon's own status.
    fn on_status(&self) -> CommsResponse {
        CommsResponse::Status(super::protocol::StatusReport {
            pid: std::process::id(),
            version: self.version.clone(),
            build_id: crate::version::build_id().to_string(),
            proto_ver: super::protocol::PROTO_VER,
            uptime_secs: self.started.elapsed().as_secs(),
            agent_store: self.agents.status(None, None).state,
        })
    }
}

/// Per-link session context. Established by `Hello`, then read by every subsequent handler on
/// that link.
#[derive(Default)]
pub struct Session {
    /// The principal the link's agent requests act as.
    pub principal: Option<Principal>,
    /// The canonical root the client named.
    pub root: Option<std::path::PathBuf>,
    /// The agent harness the client named (`BASEMIND_HOST`).
    pub host: Option<String>,
    /// The repository of `root`.
    pub repo: Option<RepoKey>,
}

#[path = "daemon_workspace_handlers.rs"]
mod workspace_handlers;

#[path = "daemon_store_blocking.rs"]
mod store_blocking;

#[cfg(test)]
use super::protocol::PROTO_VER;
#[cfg(test)]
use super::v2::api::CommsError;

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "daemon_forward_tests.rs"]
mod forward_tests;
