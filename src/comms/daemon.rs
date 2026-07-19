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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
/// "thirty minutes since anyone last called us". So each throwaway `BASEMIND_COMMS_DIR` (every test
/// run, every CI job) leaves a daemon resident for the whole window, and a machine running the suite
/// every few minutes carries a standing population of them — around a dozen at ~6 MB RSS each,
/// measured. They do all exit; the unbounded growth was [`LinkGuard`] leaking its refcount on an
/// unwind, not this constant. Shortening the window would shrink that standing population, and
/// reaping early is cheap (a respawn re-OPENS the on-disk fjall indexes rather than rebuilding
/// them), but that is a change to shipped behaviour and is left as a separate decision.
pub const IDLE_REAP_AFTER: Duration = Duration::from_secs(30 * 60);
/// How often the idle reaper re-checks the broker. Small relative to [`IDLE_REAP_AFTER`], so the
/// worst-case overshoot past the window is one tick.
pub const IDLE_REAP_CHECK_EVERY: Duration = Duration::from_secs(60);

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

/// How long a drain waits for links accepted before it started to finish their in-flight request
/// before exiting anyway. Bounded so one wedged client cannot pin a draining daemon forever; ample
/// for any request that is actually progressing, and the idle path normally finds zero links.
pub const DRAIN_GRACE: Duration = Duration::from_secs(10);
/// Poll cadence while waiting out [`DRAIN_GRACE`].
const DRAIN_POLL_EVERY: Duration = Duration::from_millis(25);

/// RAII refcount for one connected link, held for the link's whole life; see
/// [`Broker::register_link`].
///
/// The guard is taken in the ACCEPT LOOP, synchronously, before the link is spawned onto its own
/// task — not inside that task. That ordering is the point: if the increment happened inside the
/// spawned task, there would be a window in which a connection had been accepted but was not yet
/// counted, and an idle check landing in that window would see zero links and reap a daemon that
/// had just taken on work.
pub struct LinkGuard {
    broker: Arc<Broker>,
}

impl Drop for LinkGuard {
    fn drop(&mut self) {
        self.broker.link_disconnected();
    }
}

/// RAII marker that a unit of daemon-internal work is in flight; see [`Broker::begin_work`].
///
/// Work with a client attached is already covered by the link refcount — the client blocks on the
/// socket for the whole RPC. This exists for the work that has NO client: the periodic
/// cross-workspace blob GC, which the reaper must not tear down mid-sweep (it is the sole writer of
/// the global blob store, and a half-applied sweep is exactly the torn state the reap is supposed to
/// avoid). Dropping the guard stamps activity, so a sweep that finishes also restarts the idle clock.
pub struct WorkGuard<'a> {
    broker: &'a Broker,
}

impl Drop for WorkGuard<'_> {
    fn drop(&mut self) {
        self.broker.work_inflight.fetch_sub(1, Ordering::SeqCst);
        self.broker.touch();
    }
}

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

/// A registered notification sink for one subscription. The link's writer half drains it.
pub(super) struct SubSink {
    /// The thread this sink streams.
    pub(super) thread: ThreadId,
    /// The agent owning the subscription. Retained for diagnostics; the fan-out routes by thread.
    #[allow(dead_code)]
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
    workspaces: Arc<WorkspacePool>,
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
    pub(super) subscriber_count: AtomicUsize,
    link_count: AtomicUsize,
    /// Daemon-internal work units in flight (see [`Broker::begin_work`]). Distinct from
    /// `link_count`: this covers work NO client is attached to — the blob GC above all — which the
    /// idle reaper would otherwise be free to tear down mid-sweep.
    work_inflight: AtomicUsize,
    last_activity_ms: AtomicU64,
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
        Self {
            store,
            workspaces: Arc::new(WorkspacePool::new(workspace_pool::DEFAULT_HOT_CAP)),
            git_history: std::sync::Mutex::new(AHashMap::new()),
            git_history_open_lock: Mutex::new(()),
            registry: Mutex::new(Registry {
                sinks: AHashMap::new(),
                state: LifecycleState::Starting,
            }),
            machine_registry: Mutex::new(machine_registry),
            blob_gc_lock: RwLock::new(()),
            shutdown: std::sync::OnceLock::new(),
            subscriber_count: AtomicUsize::new(0),
            link_count: AtomicUsize::new(0),
            work_inflight: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
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
        self.touch();
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

    /// Mark a unit of daemon-internal work as running for as long as the returned guard lives, so
    /// the idle reaper cannot mistake it for idleness. See [`WorkGuard`].
    pub fn begin_work(&self) -> WorkGuard<'_> {
        self.work_inflight.fetch_add(1, Ordering::SeqCst);
        WorkGuard { broker: self }
    }

    /// Number of daemon-internal work units currently running. Exposed for tests.
    pub fn work_inflight(&self) -> usize {
        self.work_inflight.load(Ordering::SeqCst)
    }

    /// What "idle" MEANS — and why each clause is here.
    ///
    /// A daemon is idle only when *nothing can be waiting on it*. Three independent things can make
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
    /// 3. **An already-started drain.** Draining/Stopped is terminal; re-reaping is meaningless.
    ///
    /// Only once all three are clear does the elapsed-time test apply, and `last_activity_ms` is
    /// stamped on every link connect/disconnect — so the window measures time since the daemon last
    /// had anyone to serve, not time since the last packet.
    pub async fn is_idle_for(&self, idle_for: Duration) -> bool {
        if self.link_count.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if self.work_inflight.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if matches!(self.state().await, LifecycleState::Draining | LifecycleState::Stopped) {
            return false;
        }
        let now_ms = self.started.elapsed().as_millis() as u64;
        let last = self.last_activity_ms.load(Ordering::SeqCst);
        now_ms.saturating_sub(last) >= idle_for.as_millis() as u64
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
            if self.link_count.load(Ordering::SeqCst) != 0 || self.work_inflight.load(Ordering::SeqCst) != 0 {
                return false;
            }
            let now_ms = self.started.elapsed().as_millis() as u64;
            let last = self.last_activity_ms.load(Ordering::SeqCst);
            if now_ms.saturating_sub(last) < idle_for.as_millis() as u64 {
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
        // Shedding a git-history entry also RELEASES its fjall lock, so a cold repo's index becomes
        // openable again by a standalone `basemind scan` / `rescan` on the same machine.
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
        let _working = self.begin_work();
        let _sweep_guard = self.blob_gc_lock.write().await;
        tokio::task::spawn_blocking(crate::store_gc::reap_and_gc_global)
            .await
            .map_err(|join| crate::store_gc::GcError::Join(join.to_string()))?
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
                // Best-effort: registering a serve session's cwd populates the machine registry so
                // the coordination tools see the repo without a separate register step. A discovery
                // or persist failure is logged and ignored — Hello must not fail on it.
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
            CommsRequest::Unsubscribe { sub } => self.on_unsubscribe(sub).await,
            CommsRequest::Rescan {
                root,
                paths,
                full,
                embed,
            } => Ok(self.on_rescan(root, paths, full, embed).await),
            CommsRequest::ResolvedRefs { root, query } => Ok(self.on_resolved_refs(root, query).await),
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
            CommsRequest::Stop => {
                self.begin_drain().await;
                Ok(CommsResponse::Ok)
            }
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
        // Hold the READ side across the whole scan so a concurrent blob GC (WRITE side) cannot
        // reference-count and reap this rescan's freshly-written blobs before its index.msgpack lands.
        let _rescan_guard = self.blob_gc_lock.read().await;
        let pool = Arc::clone(&self.workspaces);
        let started = Instant::now();
        match tokio::task::spawn_blocking(move || pool.rescan(&root, paths, full, embed)).await {
            Ok(Ok(stats)) => CommsResponse::Rescanned {
                scanned: stats.scanned,
                updated: stats.updated,
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

    /// Answer a forwarded precise resolved-reference read from the workspace's read-write fjall index
    /// (the daemon holds it as the sole writer, so the cross-file `refs_by_def` / `refs_by_path`
    /// edges a read-only serve cannot see are present here). The prefix scan is blocking, so it runs
    /// on a blocking thread. A pool/open error becomes a `CommsResponse::Error` (never a torn link).
    async fn on_resolved_refs(
        &self,
        root: std::path::PathBuf,
        query: crate::comms::resolved_proto::ResolvedRefQuery,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        match tokio::task::spawn_blocking(move || {
            pool.with_workspace(&root, |store| resolve_refs_against(store, &query))
        })
        .await
        {
            Ok(Ok(result)) => CommsResponse::ResolvedRefs(result),
            Ok(Err(error)) => CommsResponse::Error {
                code: "resolved_refs_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "resolved_refs_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Run a forwarded CORE memory operation against the workspace's read-write index. The daemon is
    /// the sole fjall writer, and the pool's per-workspace store lock serializes same-workspace ops,
    /// making the forwarded `memory_put` read-modify-write atomic (no per-key lock needed here). The
    /// fjall work is blocking, so it runs on a blocking thread. Any error becomes a
    /// `CommsResponse::Error` (never a torn link).
    #[cfg(feature = "memory")]
    async fn on_memory(
        &self,
        root: std::path::PathBuf,
        scope: String,
        op: super::memory_proto::MemoryOp,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        let outcome = tokio::task::spawn_blocking(move || {
            pool.with_workspace_mut(&root, |store| {
                let idx = store
                    .index_db
                    .as_ref()
                    .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
                crate::mcp::memory_ops::run_memory_op(idx, &scope, &op)
            })
        })
        .await;
        match outcome {
            Ok(Ok(Ok(outcome))) => CommsResponse::Memory(outcome),
            Ok(Ok(Err(error))) => CommsResponse::Error {
                code: "memory_op_failed".to_string(),
                message: error.to_string(),
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "memory_workspace_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "memory_panicked".to_string(),
                message: join.to_string(),
            },
        }
    }

    /// Run a forwarded PROPOSAL governance operation against the workspace's read-write index. Same
    /// contract as [`on_memory`](Self::on_memory): the daemon is the sole fjall writer, the pool's
    /// per-workspace store lock serializes same-workspace ops (so the mine-apply tombstone-check +
    /// insert see one consistent view), the fjall work runs on a blocking thread, and any error
    /// becomes a `CommsResponse::Error` (never a torn link).
    #[cfg(feature = "memory")]
    async fn on_governance(
        &self,
        root: std::path::PathBuf,
        scope: String,
        op: super::proposals_proto::GovernanceOp,
    ) -> CommsResponse {
        self.mark_active().await;
        let pool = Arc::clone(&self.workspaces);
        let outcome = tokio::task::spawn_blocking(move || {
            pool.with_workspace_mut(&root, |store| {
                let idx = store
                    .index_db
                    .as_ref()
                    .ok_or(crate::mcp::memory_ops::MemoryOpError::IndexUnavailable)?;
                crate::mcp::proposals_ops::run_governance_op(idx, &scope, &op)
            })
        })
        .await;
        match outcome {
            Ok(Ok(Ok(outcome))) => CommsResponse::Governance(outcome),
            Ok(Ok(Err(error))) => CommsResponse::Error {
                code: "governance_op_failed".to_string(),
                message: error.to_string(),
            },
            Ok(Err(error)) => CommsResponse::Error {
                code: "governance_workspace_failed".to_string(),
                message: error.to_string(),
            },
            Err(join) => CommsResponse::Error {
                code: "governance_panicked".to_string(),
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
    async fn finish_drain(&self, sinks: Vec<mpsc::Sender<CommsOut>>) {
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

/// Answer a [`ResolvedRefQuery`] against an open workspace store. Delegates to the shared
/// `crate::query` resolvers, which read the fjall `refs_by_def` / `refs_by_path` partitions when the
/// index is open — as it always is on the daemon (sole writer, opens read-write) — so the reply
/// carries the full cross-file edge set. A degraded index (no fjall) falls back to intra-file blobs.
fn resolve_refs_against(
    store: &crate::store::Store,
    query: &crate::comms::resolved_proto::ResolvedRefQuery,
) -> crate::comms::resolved_proto::ResolvedRefResult {
    use crate::comms::resolved_proto::{ResolvedRefQuery, ResolvedRefResult};
    match query {
        ResolvedRefQuery::ReferencesTo { def_path, def_start } => {
            ResolvedRefResult::References(crate::query::resolved_references(store, def_path, *def_start))
        }
        ResolvedRefQuery::DefinitionOf { use_path, use_start } => {
            ResolvedRefResult::Definition(crate::query::definition_of(store, use_path, *use_start))
        }
    }
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
