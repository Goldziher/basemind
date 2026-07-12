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
//! Active⇄Idle edge; `Draining` (a `Stop` RPC or SIGTERM) stops accepting, flushes, then releases
//! the flock and unlinks the socket on the way to `Stopped`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

use super::cursor::Cursor;
use super::ids::{AgentId, ThreadId};
use super::model::{AgentCard, AgentKind, AgentRecord, Membership, MessageBody, MessageMeta, Thread, now_micros};
use super::protocol::{CommsNotification, CommsOut, CommsRequest, CommsResponse, PROTO_VER, SeqMeta, StatusReport};
use super::scope::{self, ScopeChain};
use super::store::{self, CommsStore, CommsStoreError};
use super::workspace_pool::{self, WorkspacePool};
use crate::registry::Registry as MachineRegistry;

/// Default page size when a client omits `limit`.
pub const DEFAULT_LIMIT: u32 = 100;
/// Hard cap on a page, mirroring the MCP `limit` ceiling.
pub const MAX_LIMIT: u32 = 1000;

/// Idle window after which a daemon with no connected links and no activity self-terminates.
pub const IDLE_REAP_AFTER: Duration = Duration::from_secs(30 * 60);
/// How often the idle reaper re-checks the broker. Small relative to [`IDLE_REAP_AFTER`].
pub const IDLE_REAP_CHECK_EVERY: Duration = Duration::from_secs(60);

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
/// cache survives; the next request re-opens it lazily. Well below [`IDLE_REAP_AFTER`] so cold
/// workspaces free memory long before the whole daemon self-terminates.
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
struct SubSink {
    /// The thread this sink streams.
    thread: ThreadId,
    /// The agent owning the subscription. Retained for diagnostics; the fan-out routes by thread.
    #[allow(dead_code)]
    agent: AgentId,
    /// Where notifications are pushed.
    tx: mpsc::Sender<CommsOut>,
}

/// In-RAM broker state behind a single async mutex.
struct Registry {
    /// Live notification sinks keyed by subscription handle.
    sinks: AHashMap<u64, SubSink>,
    /// Current lifecycle state.
    state: LifecycleState,
}

/// The broker. Cheap to share via `Arc`; every front-end and link holds one.
pub struct Broker {
    store: Arc<CommsStore>,
    /// Hot read-write workspace indexes. The daemon is the machine's sole fjall writer; front-ends
    /// forward their scans/rescans here so concurrent read-only sessions never contend for the lock.
    workspaces: Arc<WorkspacePool>,
    registry: Mutex<Registry>,
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
    subscriber_count: AtomicUsize,
    link_count: AtomicUsize,
    last_activity_ms: AtomicU64,
    next_sub: AtomicU64,
    started: Instant,
    version: String,
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
            registry: Mutex::new(Registry {
                sinks: AHashMap::new(),
                state: LifecycleState::Starting,
            }),
            machine_registry: Mutex::new(machine_registry),
            blob_gc_lock: RwLock::new(()),
            subscriber_count: AtomicUsize::new(0),
            link_count: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
            next_sub: AtomicU64::new(1),
            started: Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
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

    /// True when the broker has no connected links and no activity within `idle_for`.
    pub async fn is_idle_for(&self, idle_for: Duration) -> bool {
        if self.link_count.load(Ordering::Relaxed) != 0 {
            return false;
        }
        if matches!(self.state().await, LifecycleState::Draining | LifecycleState::Stopped) {
            return false;
        }
        let now_ms = self.started.elapsed().as_millis() as u64;
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        now_ms.saturating_sub(last) >= idle_for.as_millis() as u64
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
        self.workspaces.evict_idle(ttl)
    }

    /// Reference-count the machine-global blob store across every workspace and reap orphans, under
    /// the WRITE side of [`Broker::blob_gc_lock`] so no rescan is writing blobs mid-sweep. Only the
    /// daemon calls this — it alone sees every workspace's references, the precondition for a safe
    /// cross-workspace sweep. The blocking filesystem work runs off the reactor.
    pub async fn run_blob_gc(&self) -> Result<crate::store_gc::GcReport, crate::store_gc::GcError> {
        let _sweep_guard = self.blob_gc_lock.write().await;
        tokio::task::spawn_blocking(crate::store_gc::gc_global_blobs)
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
            CommsRequest::Rescan { root, paths, full } => Ok(self.on_rescan(root, paths, full).await),
            #[cfg(feature = "memory")]
            CommsRequest::Memory { root, scope, op } => Ok(self.on_memory(root, scope, op).await),
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
    ) -> CommsResponse {
        self.mark_active().await;
        // Hold the READ side across the whole scan so a concurrent blob GC (WRITE side) cannot
        // reference-count and reap this rescan's freshly-written blobs before its index.msgpack lands.
        let _rescan_guard = self.blob_gc_lock.read().await;
        let pool = Arc::clone(&self.workspaces);
        let started = Instant::now();
        match tokio::task::spawn_blocking(move || pool.rescan(&root, paths, full)).await {
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

    fn on_hello(
        &self,
        agent: AgentId,
        proto_ver: u32,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        session: &mut Session,
    ) -> Result<CommsResponse, CommsStoreError> {
        if proto_ver != PROTO_VER {
            return Ok(CommsResponse::Error {
                code: "proto_skew".to_string(),
                message: format!("daemon speaks proto {PROTO_VER}, client sent {proto_ver}"),
            });
        }
        session.agent = Some(agent.clone());
        session.chain = Some(build_chain(remote, cwd));

        let now = now_micros();
        let record = match self.store.get_agent(&agent)? {
            Some(mut existing) => {
                existing.last_seen = now;
                existing
            }
            None => AgentRecord {
                agent_id: agent,
                card: AgentCard::default(),
                kind: AgentKind::Other,
                first_seen: now,
                last_seen: now,
            },
        };
        self.store.put_agent(&record)?;

        Ok(CommsResponse::Welcome {
            proto_ver: PROTO_VER,
            daemon_version: self.version.clone(),
        })
    }

    fn on_register(&self, session: &Session, card: AgentCard) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let now = now_micros();
        let record = match self.store.get_agent(&agent)? {
            Some(mut existing) => {
                existing.card = card;
                existing.last_seen = now;
                existing
            }
            None => AgentRecord {
                agent_id: agent,
                card,
                kind: AgentKind::Other,
                first_seen: now,
                last_seen: now,
            },
        };
        self.store.put_agent(&record)?;
        Ok(CommsResponse::Ok)
    }

    fn on_list_agents(&self, thread: Option<ThreadId>) -> Result<CommsResponse, CommsStoreError> {
        let agents = match thread {
            None => self.store.list_agents()?,
            Some(thread) => {
                let members = self.store.members(&thread)?;
                let mut out = Vec::new();
                for id in members {
                    if let Some(rec) = self.store.get_agent(&id)? {
                        out.push(rec);
                    }
                }
                out
            }
        };
        Ok(CommsResponse::Agents(agents))
    }

    /// Start a thread addressed by at least two of subject / path / members. The creator becomes an
    /// implicit member; any explicit members are added too. Rejects fewer than two dimensions.
    fn on_thread_start(
        &self,
        session: &Session,
        subject: Option<String>,
        path: Option<String>,
        members: Vec<AgentId>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(creator) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let subject = subject.filter(|s| !s.is_empty());
        let path = path.filter(|p| !p.is_empty());
        if let Err(message) = validate_dimensions(subject.as_deref(), path.as_deref(), &members, &creator) {
            return Ok(CommsResponse::Error {
                code: "insufficient_dimensions".to_string(),
                message,
            });
        }

        // The full member set: the creator plus any explicit members, deduplicated.
        let mut member_set: Vec<AgentId> = vec![creator.clone()];
        for m in members {
            if !member_set.contains(&m) {
                member_set.push(m);
            }
        }

        let now = now_micros();
        let id = mint_thread_id(&creator);
        let thread = Thread {
            id: id.clone(),
            subject,
            path,
            members: member_set.clone(),
            creator: creator.clone(),
            active: true,
            created_at: now,
            last_activity: 0,
        };
        self.store.put_thread(&thread)?;
        for agent in &member_set {
            self.store.add_member(&Membership {
                agent_id: agent.clone(),
                thread: id.clone(),
                created_at: now,
            })?;
        }
        Ok(CommsResponse::Thread(thread))
    }

    fn on_thread_join(&self, session: &Session, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        self.store.add_member(&Membership {
            agent_id: agent.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })?;
        if !record.members.contains(&agent) {
            record.members.push(agent);
            self.store.put_thread(&record)?;
        }
        Ok(CommsResponse::Ok)
    }

    fn on_thread_leave(&self, session: &Session, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        self.store.remove_member(&thread, &agent)?;
        if let Some(mut record) = self.store.get_thread(&thread)? {
            record.members.retain(|m| m != &agent);
            self.store.put_thread(&record)?;
        }
        Ok(CommsResponse::Ok)
    }

    /// List threads DISCOVERABLE to the caller: member OR cwd matches the path glob OR (when set)
    /// the subject substring filter matches. Never all threads. Archived excluded unless requested.
    fn on_thread_list(
        &self,
        session: &Session,
        remote: Option<String>,
        cwd: Option<std::path::PathBuf>,
        subject_contains: Option<String>,
        include_archived: bool,
    ) -> Result<CommsResponse, CommsStoreError> {
        let agent = session.agent.clone();
        let chain = build_chain(remote, cwd);
        let filter = subject_contains.filter(|s| !s.is_empty());
        let mut out = Vec::new();
        for thread in self.store.list_threads()? {
            if !thread.active && !include_archived {
                continue;
            }
            let is_member = agent.as_ref().is_some_and(|a| thread.members.contains(a));
            let path_hit = thread
                .path
                .as_deref()
                .is_some_and(|p| !chain.cwd.as_os_str().is_empty() && scope::path_matches(p, &chain.cwd));
            let subject_hit = match (&filter, &thread.subject) {
                (Some(needle), Some(subject)) => subject.contains(needle.as_str()),
                _ => false,
            };
            if is_member || path_hit || subject_hit {
                out.push(thread);
            }
        }
        Ok(CommsResponse::Threads(out))
    }

    fn on_thread_members(&self, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        if self.store.get_thread(&thread)?.is_none() {
            return Ok(unknown_thread(&thread));
        }
        Ok(CommsResponse::Members {
            members: self.store.members(&thread)?,
        })
    }

    fn on_thread_add_member(
        &self,
        session: &Session,
        thread: ThreadId,
        member: AgentId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        if record.creator != agent {
            return Ok(not_creator());
        }
        self.store.add_member(&Membership {
            agent_id: member.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })?;
        if !record.members.contains(&member) {
            record.members.push(member);
            self.store.put_thread(&record)?;
        }
        Ok(CommsResponse::Ok)
    }

    fn on_thread_remove_member(
        &self,
        session: &Session,
        thread: ThreadId,
        member: AgentId,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        if record.creator != agent {
            return Ok(not_creator());
        }
        self.store.remove_member(&thread, &member)?;
        record.members.retain(|m| m != &member);
        self.store.put_thread(&record)?;
        Ok(CommsResponse::Ok)
    }

    fn on_thread_archive(&self, session: &Session, thread: ThreadId) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let Some(mut record) = self.store.get_thread(&thread)? else {
            return Ok(unknown_thread(&thread));
        };
        if record.creator != agent {
            return Ok(not_creator());
        }
        record.active = false;
        self.store.put_thread(&record)?;
        Ok(CommsResponse::Ok)
    }

    async fn on_post(
        &self,
        session: &Session,
        thread: ThreadId,
        subject: String,
        tags: Vec<String>,
        reply_to: Option<String>,
        body: Vec<u8>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        if self.store.get_thread(&thread)?.is_none() {
            return Ok(unknown_thread(&thread));
        }
        let id = mint_message_id(&thread, &agent);
        let meta = store::build_meta(id, thread.clone(), agent, subject, tags, reply_to, &body);
        let (_, stored) = self.store.post(&thread, meta, MessageBody(body))?;
        if let Some(mut record) = self.store.get_thread(&thread)? {
            record.last_activity = stored.ts_micros;
            self.store.put_thread(&record)?;
        }
        self.fan_out(&thread, &stored).await;
        Ok(CommsResponse::Posted { message_id: stored.id })
    }

    fn on_history(
        &self,
        thread: ThreadId,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        since_micros: Option<i64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let after = decode_after(cursor.as_ref(), thread.as_str());
        let limit = clamp_limit(limit);
        let page = self.store.history(&thread, after, limit)?;
        let next = page.more.then(|| Cursor::encode(thread.as_str(), page.last_seq));
        let messages = page
            .messages
            .into_iter()
            .filter(|(_, meta)| keep_since(meta.ts_micros, since_micros))
            .map(|(seq, meta)| SeqMeta { seq, meta })
            .collect();
        Ok(CommsResponse::History {
            messages,
            next_cursor: next,
        })
    }

    fn on_get_body(&self, message_id: String) -> Result<CommsResponse, CommsStoreError> {
        let body = self.store.get_body(&message_id)?;
        Ok(CommsResponse::Body { body })
    }

    fn on_inbox(
        &self,
        session: &mut Session,
        cursor: Option<Cursor>,
        limit: Option<u32>,
        mark_read: bool,
        since_micros: Option<i64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let limit = clamp_limit(limit);
        let resume = cursor.as_ref().and_then(|c| c.decode().ok());
        let mut threads = self.store.threads_for_agent(&agent)?;
        threads.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut collected: Vec<SeqMeta> = Vec::new();
        let mut delivered_high: Vec<(ThreadId, u64)> = Vec::new();
        let mut unread_remaining: u32 = 0;
        let mut next_cursor: Option<Cursor> = None;

        for thread in &threads {
            let read_seq = self.store.read_cursor(&agent, thread)?;
            let after = match &resume {
                Some(pos) if pos.thread == thread.as_str() => pos.seq.max(read_seq),
                _ => read_seq,
            };
            let remaining = limit.saturating_sub(collected.len());
            let want = remaining.saturating_add(1).max(1);
            let rows = self.store.history_with_seq(thread, after, want)?;
            for (seq, meta) in rows {
                if meta.from == agent || !keep_since(meta.ts_micros, since_micros) {
                    upsert_high(&mut delivered_high, thread, seq);
                    continue;
                }
                if collected.len() < limit {
                    collected.push(SeqMeta { seq, meta });
                    upsert_high(&mut delivered_high, thread, seq);
                } else {
                    unread_remaining = unread_remaining.saturating_add(1);
                    if next_cursor.is_none() {
                        let resume_seq = highest_for(&delivered_high, thread).unwrap_or(after);
                        next_cursor = Some(Cursor::encode(thread.as_str(), resume_seq));
                    }
                }
            }
        }

        if mark_read {
            for (thread, seq) in &delivered_high {
                self.store.set_read_cursor(&agent, thread, *seq)?;
            }
        }

        Ok(CommsResponse::Inbox {
            messages: collected,
            unread: unread_remaining,
            next_cursor,
        })
    }

    fn on_ack(
        &self,
        session: &Session,
        message_ids: Vec<String>,
        thread: Option<ThreadId>,
        to_seq: Option<u64>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        let bulk = matches!((&thread, to_seq), (Some(_), Some(_)));
        if message_ids.is_empty() && !bulk {
            return Ok(CommsResponse::Error {
                code: "empty_ack".to_string(),
                message: "ack requires message_ids or a (thread, to_seq) pair".to_string(),
            });
        }

        let mut targets: Vec<(ThreadId, u64)> = Vec::new();
        let mut acked: u32 = 0;
        if !message_ids.is_empty() {
            for (_, thread, seq) in self.store.resolve_ids(&message_ids)? {
                acked = acked.saturating_add(1);
                upsert_high(&mut targets, &thread, seq);
            }
        }
        if let (Some(thread), Some(seq)) = (thread, to_seq) {
            upsert_high(&mut targets, &thread, seq);
        }

        let mut cursors_advanced: Vec<(String, u64)> = Vec::new();
        for (thread, seq) in &targets {
            let before = self.store.read_cursor(&agent, thread)?;
            self.store.set_read_cursor(&agent, thread, *seq)?;
            let after = self.store.read_cursor(&agent, thread)?;
            if after > before {
                cursors_advanced.push((thread.as_str().to_string(), after));
            }
        }

        Ok(CommsResponse::Acked {
            acked,
            cursors_advanced,
        })
    }

    async fn on_subscribe(
        &self,
        session: &Session,
        thread: ThreadId,
        link_tx: &mpsc::Sender<CommsOut>,
    ) -> Result<CommsResponse, CommsStoreError> {
        let Some(agent) = session.agent.clone() else {
            return Ok(need_hello());
        };
        if self.store.get_thread(&thread)?.is_none() {
            return Ok(unknown_thread(&thread));
        }
        self.store.add_member(&Membership {
            agent_id: agent.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })?;
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        {
            let mut reg = self.registry.lock().await;
            reg.sinks.insert(
                sub,
                SubSink {
                    thread,
                    agent,
                    tx: link_tx.clone(),
                },
            );
            reg.state = LifecycleState::Active;
        }
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
        Ok(CommsResponse::Subscribed { sub })
    }

    async fn on_unsubscribe(&self, sub: u64) -> Result<CommsResponse, CommsStoreError> {
        let removed = {
            let mut reg = self.registry.lock().await;
            reg.sinks.remove(&sub)
        };
        if removed.is_some() {
            self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
            self.maybe_idle().await;
        }
        Ok(CommsResponse::Ok)
    }

    async fn on_status(&self) -> CommsResponse {
        let threads = self
            .store
            .list_threads()
            .map(|t| t.iter().filter(|th| th.active).count())
            .unwrap_or(0);
        CommsResponse::Status(StatusReport {
            pid: std::process::id(),
            version: self.version.clone(),
            proto_ver: PROTO_VER,
            uptime_secs: self.started.elapsed().as_secs(),
            threads: u32::try_from(threads).unwrap_or(u32::MAX),
            subscribers: u32::try_from(self.subscriber_count()).unwrap_or(u32::MAX),
        })
    }

    /// Push a new message to every live sink subscribed to `thread`. Best-effort: a sink whose
    /// channel is full or closed is dropped.
    async fn fan_out(&self, thread: &ThreadId, meta: &MessageMeta) {
        let mut dead: Vec<u64> = Vec::new();
        {
            let reg = self.registry.lock().await;
            for (sub, sink) in reg.sinks.iter() {
                if &sink.thread == thread {
                    let note = CommsOut::Notification(CommsNotification::Message(meta.clone()));
                    if sink.tx.try_send(note).is_err() {
                        dead.push(*sub);
                    }
                }
            }
        }
        if !dead.is_empty() {
            let mut reg = self.registry.lock().await;
            for sub in dead {
                if reg.sinks.remove(&sub).is_some() {
                    self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Transition to Idle when the last subscriber leaves.
    async fn maybe_idle(&self) {
        if self.subscriber_count() == 0 {
            let mut reg = self.registry.lock().await;
            if reg.state == LifecycleState::Active {
                reg.state = LifecycleState::Idle;
                tracing::debug!("comms: broker idle (no subscribers); socket + flock retained");
            }
        }
    }

    /// Enter the Draining state and notify every live sink to disconnect.
    pub async fn begin_drain(&self) {
        let sinks: Vec<mpsc::Sender<CommsOut>> = {
            let mut reg = self.registry.lock().await;
            reg.state = LifecycleState::Draining;
            reg.sinks.values().map(|s| s.tx.clone()).collect()
        };
        for tx in sinks {
            let _ = tx.send(CommsOut::Notification(CommsNotification::Shutdown)).await;
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

fn need_hello() -> CommsResponse {
    CommsResponse::Error {
        code: "no_hello".to_string(),
        message: "send Hello before any other request".to_string(),
    }
}

fn unknown_thread(thread: &ThreadId) -> CommsResponse {
    CommsResponse::Error {
        code: "unknown_thread".to_string(),
        message: format!("no thread {}", thread.as_str()),
    }
}

fn not_creator() -> CommsResponse {
    CommsResponse::Error {
        code: "not_creator".to_string(),
        message: "only the thread creator may manage membership or archive it".to_string(),
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

fn clamp_limit(limit: Option<u32>) -> usize {
    usize::try_from(limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)).unwrap_or(DEFAULT_LIMIT as usize)
}

fn decode_after(cursor: Option<&Cursor>, thread: &str) -> u64 {
    match cursor.and_then(|c| c.decode().ok()) {
        Some(pos) if pos.thread == thread || pos.thread.is_empty() => pos.seq,
        _ => 0,
    }
}

/// Whether a message with `ts_micros` passes the optional recency cutoff.
fn keep_since(ts_micros: i64, since_micros: Option<i64>) -> bool {
    match since_micros {
        Some(cut) => ts_micros >= cut,
        None => true,
    }
}

/// Record the highest delivered `seq` for `thread` in a small per-page accumulator.
fn upsert_high(acc: &mut Vec<(ThreadId, u64)>, thread: &ThreadId, seq: u64) {
    if let Some(entry) = acc.iter_mut().find(|(t, _)| t == thread) {
        if seq > entry.1 {
            entry.1 = seq;
        }
    } else {
        acc.push((thread.clone(), seq));
    }
}

/// Look up the highest delivered `seq` recorded for `thread`.
fn highest_for(acc: &[(ThreadId, u64)], thread: &ThreadId) -> Option<u64> {
    acc.iter().find(|(t, _)| t == thread).map(|(_, s)| *s)
}

#[path = "daemon_threads.rs"]
mod threads;
#[cfg(test)]
use threads::sanitize_id;
use threads::{build_chain, mint_message_id, mint_thread_id, validate_dimensions};

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
